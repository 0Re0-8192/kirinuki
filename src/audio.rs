//! System audio (WASAPI loopback) -> 48 kHz stereo i16, with silence filled in by the clock.
use crate::encoder::{Encoder, AUDIO_RATE};
use crate::record::Shared;
use crate::logf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows::core::*;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::*;
use windows::Win32::Media::Multimedia::*;
use windows::Win32::System::Com::*;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SampleKind {
    F32,
    I16,
    I24,
    I32,
}

#[derive(Clone, Copy, Debug)]
pub struct InFormat {
    pub kind: SampleKind,
    pub channels: usize,
    pub rate: u32,
    pub block: usize,
}

/// Raw device bytes -> interleaved stereo f32.
pub fn to_stereo(data: &[u8], frames: usize, f: &InFormat, out: &mut Vec<f32>) {
    let bps = f.block / f.channels.max(1);
    let read = |i: usize| -> f32 {
        let o = i * bps;
        match f.kind {
            SampleKind::F32 => f32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]),
            SampleKind::I16 => i16::from_le_bytes([data[o], data[o + 1]]) as f32 / 32768.0,
            SampleKind::I24 => {
                let v = ((data[o] as i32) << 8 | (data[o + 1] as i32) << 16 | (data[o + 2] as i32) << 24) >> 8;
                v as f32 / 8_388_608.0
            }
            SampleKind::I32 => i32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]) as f32 / 2_147_483_648.0,
        }
    };
    for fr in 0..frames {
        let base = fr * f.channels;
        let l = read(base);
        let r = if f.channels > 1 { read(base + 1) } else { l };
        out.push(l);
        out.push(r);
    }
}

/// Linear resampler for interleaved stereo.
pub struct Resampler {
    step: f64,
    pos: f64,
    prev: [f32; 2],
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Resampler { step: from as f64 / to as f64, pos: 0.0, prev: [0.0; 2] }
    }
    pub fn process(&mut self, inp: &[f32], out: &mut Vec<f32>) {
        if (self.step - 1.0).abs() < 1e-9 {
            out.extend_from_slice(inp);
            return;
        }
        let n = inp.len() / 2;
        // pos is in input frames, relative to the start of `inp`; -1 means `prev`
        let get = |i: isize, c: usize, prev: &[f32; 2]| if i < 0 { prev[c] } else { inp[i as usize * 2 + c] };
        while self.pos < n as f64 - 1.0 + 1.0 {
            let i = self.pos.floor() as isize - 1;
            let fr = (self.pos - self.pos.floor()) as f32;
            if (i + 1) as usize >= n {
                break;
            }
            for c in 0..2 {
                let a = get(i, c, &self.prev);
                let b = get(i + 1, c, &self.prev);
                out.push(a + (b - a) * fr);
            }
            self.pos += self.step;
        }
        if n > 0 {
            self.prev = [inp[(n - 1) * 2], inp[(n - 1) * 2 + 1]];
        }
        self.pos -= n as f64;
    }
}

pub fn to_i16(inp: &[f32], out: &mut Vec<i16>) {
    out.extend(inp.iter().map(|v| (v.clamp(-1.0, 1.0) * 32767.0) as i16));
}

fn parse_format(p: *const WAVEFORMATEX) -> InFormat {
    unsafe {
        let wf = &*p;
        let tag = wf.wFormatTag as u32;
        let bits = wf.wBitsPerSample;
        let mut float = tag == WAVE_FORMAT_IEEE_FLOAT;
        if tag == WAVE_FORMAT_EXTENSIBLE {
            let ex = &*(p as *const WAVEFORMATEXTENSIBLE);
            let sub = ex.SubFormat;
            float = sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
        }
        let kind = match (float, bits) {
            (true, _) => SampleKind::F32,
            (false, 16) => SampleKind::I16,
            (false, 24) => SampleKind::I24,
            _ => SampleKind::I32,
        };
        InFormat { kind, channels: wf.nChannels as usize, rate: wf.nSamplesPerSec, block: wf.nBlockAlign as usize }
    }
}

struct Loopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    fmt: InFormat,
}

fn open_loopback() -> Result<Loopback> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let mix = client.GetMixFormat()?;
        let fmt = parse_format(mix);
        let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 2_000_000, 0, mix, None);
        CoTaskMemFree(Some(mix as *const _));
        r?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok(Loopback { client, capture, fmt })
    }
}

/// How to place a chunk of `n` frames that ends "now", keeping audio on the recording clock.
/// `expected` = frames the clock says should exist now. Positive: insert that much silence first.
/// Negative: drop that many frames from the start of the chunk (audio ran ahead).
pub fn align(written: u64, expected: u64, n: u64) -> i64 {
    let start = expected as i64 - n as i64; // where this chunk should begin
    let diff = start - written as i64;
    let slack = AUDIO_RATE as i64 / 50; // 20 ms
    if diff > slack {
        diff
    } else if diff < -2 * slack {
        diff.max(-(n as i64))
    } else {
        0
    }
}

fn write(enc: &Arc<Mutex<Option<Encoder>>>, pcm: &[i16]) {
    if let Some(e) = enc.lock().unwrap().as_mut() {
        if let Err(err) = e.write_audio(pcm) {
            logf!("audio: write failed: {err}");
        }
    }
}

/// Runs until shared.stop. Always produces audio (silence if the device can't be opened).
pub fn run(shared: Arc<Shared>, enc: Arc<Mutex<Option<Encoder>>>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut failures = 0u32;
    let mut open = || match open_loopback() {
        Ok(l) => {
            logf!("audio: loopback {:?}", l.fmt);
            failures = 0;
            Some(l)
        }
        Err(e) => {
            if failures == 0 {
                logf!("audio: loopback unavailable, silence: {e}");
            }
            failures += 1;
            None
        }
    };
    let mut lb = open();
    let mut rs = lb.as_ref().map(|l| Resampler::new(l.fmt.rate, AUDIO_RATE));
    let mut last_open = std::time::Instant::now();
    let mut written: u64 = 0;
    let mut f32buf = Vec::new();
    let mut rsbuf = Vec::new();
    let mut pcm = Vec::new();
    while !shared.stopping() {
        std::thread::sleep(Duration::from_millis(10));
        let paused = shared.paused();
        let mut lost = false;
        pcm.clear();
        if let (Some(l), Some(rs)) = (lb.as_ref(), rs.as_mut()) {
            unsafe {
                loop {
                    let n = match l.capture.GetNextPacketSize() {
                        Ok(n) => n,
                        Err(e) => {
                            // e.g. AUDCLNT_E_DEVICE_INVALIDATED: default output changed
                            logf!("audio: device lost: {e}");
                            lost = true;
                            break;
                        }
                    };
                    if n == 0 {
                        break;
                    }
                    let mut data = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    if l.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() {
                        lost = true;
                        break;
                    }
                    if !paused {
                        f32buf.clear();
                        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                            f32buf.resize(frames as usize * 2, 0.0);
                        } else {
                            let bytes = std::slice::from_raw_parts(data, frames as usize * l.fmt.block);
                            to_stereo(bytes, frames as usize, &l.fmt, &mut f32buf);
                        }
                        rsbuf.clear();
                        rs.process(&f32buf, &mut rsbuf);
                        to_i16(&rsbuf, &mut pcm); // collected; placed once per poll below
                    }
                    let _ = l.capture.ReleaseBuffer(frames);
                }
            }
        }
        // everything drained in this poll ends "now": place it as one chunk
        if !paused && !pcm.is_empty() {
            let n = (pcm.len() / 2) as u64;
            let expected = (shared.active().as_secs_f64() * AUDIO_RATE as f64) as u64;
            let adj = align(written, expected, n);
            if adj > 0 {
                write(&enc, &vec![0i16; adj as usize * 2]);
                written += adj as u64;
            }
            let skip = if adj < 0 { (-adj) as usize * 2 } else { 0 };
            if skip < pcm.len() {
                write(&enc, &pcm[skip..]);
                written += ((pcm.len() - skip) / 2) as u64;
            }
        }
        if lost {
            lb = None;
            rs = None;
        }
        if lb.is_none() && last_open.elapsed() > Duration::from_millis(500) {
            last_open = std::time::Instant::now();
            lb = open();
            rs = lb.as_ref().map(|l| Resampler::new(l.fmt.rate, AUDIO_RATE));
        }
        if paused {
            continue;
        }
        // loopback delivers nothing while the system is silent: fill to the clock
        let expected = (shared.active().as_secs_f64() * AUDIO_RATE as f64) as u64;
        if expected > written + AUDIO_RATE as u64 / 50 {
            let n = (expected - written) as usize;
            write(&enc, &vec![0i16; n * 2]);
            written += n as u64;
        }
    }
    if let Some(l) = lb {
        unsafe {
            let _ = l.client.Stop();
        }
    }
    logf!("audio: done, {written} frames");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_keeps_audio_on_the_clock() {
        // on time
        assert_eq!(align(48000, 48480, 480), 0);
        // a burst of 4 packets drained together that ends now is on time too
        assert_eq!(align(48000, 48000 + 1920, 1920), 0);
        // 100 ms of nothing before this chunk: fill the gap
        assert_eq!(align(48000, 48000 + 4800 + 480, 480), 4800);
        // small jitter is tolerated both ways
        assert_eq!(align(48000, 48000 + 500 + 480, 480), 0);
        assert_eq!(align(48000 + 1500, 48000 + 480, 480), 0);
        // 60 ms ahead: trim the start
        assert_eq!(align(48000 + 2880, 48000 + 480, 480), -480);
        assert_eq!(align(48000 + 2880, 48000 + 4800, 4800), -2880);
    }

    #[test]
    fn stereo_from_mono_f32() {
        let f = InFormat { kind: SampleKind::F32, channels: 1, rate: 48000, block: 4 };
        let data: Vec<u8> = [0.5f32, -0.25].iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut out = Vec::new();
        to_stereo(&data, 2, &f, &mut out);
        assert_eq!(out, vec![0.5, 0.5, -0.25, -0.25]);
    }

    #[test]
    fn surround_takes_front_pair() {
        let f = InFormat { kind: SampleKind::I16, channels: 6, rate: 48000, block: 12 };
        let s: [i16; 6] = [16384, -16384, 1, 2, 3, 4];
        let data: Vec<u8> = s.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut out = Vec::new();
        to_stereo(&data, 1, &f, &mut out);
        assert_eq!(out, vec![0.5, -0.5]);
    }

    #[test]
    fn i24_sign() {
        let f = InFormat { kind: SampleKind::I24, channels: 2, rate: 48000, block: 6 };
        let data = [0x00, 0x00, 0xC0, 0x00, 0x00, 0x40]; // -0.5, 0.5
        let mut out = Vec::new();
        to_stereo(&data, 1, &f, &mut out);
        assert_eq!(out, vec![-0.5, 0.5]);
    }

    #[test]
    fn resample_count_matches_ratio() {
        for &(from, to) in &[(44100u32, 48000u32), (96000, 48000), (48000, 48000)] {
            let mut r = Resampler::new(from, to);
            let mut out = Vec::new();
            let mut total_in = 0usize;
            for _ in 0..100 {
                let chunk = vec![0.1f32; (from as usize / 100) * 2];
                total_in += chunk.len() / 2;
                r.process(&chunk, &mut out);
            }
            let expect = total_in as f64 * to as f64 / from as f64;
            let got = (out.len() / 2) as f64;
            assert!((got - expect).abs() <= 2.0, "{from}->{to}: {got} vs {expect}");
            // the first frame ramps up from the initial 0
            assert!(out[4..].iter().all(|v| (*v - 0.1).abs() < 1e-6), "{from}->{to}");
        }
    }
}
