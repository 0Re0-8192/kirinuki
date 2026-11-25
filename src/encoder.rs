//! MP4 (H.264 + AAC) writer on Media Foundation's sink writer.
use windows::core::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Media::MediaFoundation::*;

pub const AUDIO_RATE: u32 = 48000;

pub struct Encoder {
    writer: IMFSinkWriter,
    video: u32,
    audio: Option<u32>,
    w: u32,
    h: u32,
    gpu: bool,
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    staging: Option<ID3D11Texture2D>,
    pub frames: u64,
    pub audio_frames: u64,
}

// MF and D3D11 (multithread-protected) objects are free-threaded.
unsafe impl Send for Encoder {}

fn pack(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

impl Encoder {
    /// `manager` set = GPU path (DXGI surfaces, hardware encoder allowed).
    pub fn new(
        path: &str,
        device: &ID3D11Device,
        manager: Option<&IMFDXGIDeviceManager>,
        w: u32,
        h: u32,
        fps: u32,
        audio: bool,
    ) -> Result<Encoder> {
        unsafe {
            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 4)?;
            let attrs = attrs.unwrap();
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, manager.is_some() as u32)?;
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
            if let Some(m) = manager {
                attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, m)?;
            }
            let writer = MFCreateSinkWriterFromURL(&HSTRING::from(path), None, &attrs)?;

            // text-heavy screen content: generous bitrate
            let bitrate = ((w as u64 * h as u64 * fps as u64) / 6).clamp(2_000_000, 40_000_000) as u32;
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            let video = writer.AddStream(&out)?;

            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, w * 4)?;
            inp.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            writer.SetInputMediaType(video, &inp, None)?;

            let audio = if audio {
                let ao = MFCreateMediaType()?;
                ao.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                ao.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
                ao.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                ao.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
                ao.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
                ao.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24000)?;
                let a = writer.AddStream(&ao)?;
                let ai = MFCreateMediaType()?;
                ai.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                ai.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
                ai.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                ai.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
                ai.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
                ai.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
                ai.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_RATE * 4)?;
                writer.SetInputMediaType(a, &ai, None)?;
                Some(a)
            } else {
                None
            };

            writer.BeginWriting()?;
            let ctx = device.GetImmediateContext()?;
            Ok(Encoder {
                writer,
                video,
                audio,
                w,
                h,
                gpu: manager.is_some(),
                device: device.clone(),
                ctx,
                staging: None,
                frames: 0,
                audio_frames: 0,
            })
        }
    }

    pub fn is_gpu(&self) -> bool {
        self.gpu
    }

    /// `tex` must be w x h BGRA and must not be modified afterwards (the writer may read it later).
    pub fn write_video(&mut self, tex: &ID3D11Texture2D, t: i64, dur: i64) -> Result<()> {
        unsafe {
            let buf = if self.gpu {
                let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
                let b2: IMF2DBuffer = buf.cast()?;
                buf.SetCurrentLength(b2.GetContiguousLength()?)?;
                buf
            } else {
                self.cpu_buffer(tex)?
            };
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            s.SetSampleTime(t)?;
            s.SetSampleDuration(dur)?;
            self.writer.WriteSample(self.video, &s)?;
            self.frames += 1;
            Ok(())
        }
    }

    unsafe fn cpu_buffer(&mut self, tex: &ID3D11Texture2D) -> Result<IMFMediaBuffer> {
        if self.staging.is_none() {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.w,
                Height: self.h,
                MipLevels: 1,
                ArraySize: 1,
                Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut t = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut t))?;
            self.staging = t;
        }
        let st = self.staging.as_ref().unwrap();
        self.ctx.CopyResource(st, tex);
        let row = (self.w * 4) as usize;
        let len = row * self.h as usize;
        let buf = MFCreateMemoryBuffer(len as u32)?;
        let mut p = std::ptr::null_mut();
        buf.Lock(&mut p, None, None)?;
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        let mapped = self.ctx.Map(st, 0, D3D11_MAP_READ, 0, Some(&mut m));
        if mapped.is_ok() {
            for y in 0..self.h as usize {
                std::ptr::copy_nonoverlapping((m.pData as *const u8).add(y * m.RowPitch as usize), p.add(y * row), row);
            }
            self.ctx.Unmap(st, 0);
        }
        buf.Unlock()?;
        mapped?;
        buf.SetCurrentLength(len as u32)?;
        Ok(buf)
    }

    /// Interleaved stereo i16 at AUDIO_RATE.
    pub fn write_audio(&mut self, pcm: &[i16]) -> Result<()> {
        let Some(a) = self.audio else { return Ok(()) };
        if pcm.is_empty() {
            return Ok(());
        }
        unsafe {
            let bytes = pcm.len() * 2;
            let buf = MFCreateMemoryBuffer(bytes as u32)?;
            let mut p = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, p, bytes);
            buf.Unlock()?;
            buf.SetCurrentLength(bytes as u32)?;
            let n = (pcm.len() / 2) as u64;
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            s.SetSampleTime((self.audio_frames * 10_000_000 / AUDIO_RATE as u64) as i64)?;
            s.SetSampleDuration((n * 10_000_000 / AUDIO_RATE as u64) as i64)?;
            self.writer.WriteSample(a, &s)?;
            self.audio_frames += n;
            Ok(())
        }
    }

    /// Video samples handed to the writer but not yet encoded.
    pub fn video_backlog(&self) -> u64 {
        unsafe {
            let mut st = MF_SINK_WRITER_STATISTICS { cb: std::mem::size_of::<MF_SINK_WRITER_STATISTICS>() as u32, ..Default::default() };
            if self.writer.GetStatistics(self.video, &mut st).is_ok() {
                st.qwNumSamplesReceived.saturating_sub(st.qwNumSamplesEncoded)
            } else {
                0
            }
        }
    }

    /// Appends silence so the audio track reaches `end` (100 ns units).
    pub fn pad_audio_to(&mut self, end: u64) {
        if self.audio.is_none() {
            return;
        }
        let want = end * AUDIO_RATE as u64 / 10_000_000;
        if want > self.audio_frames {
            let zeros = vec![0i16; ((want - self.audio_frames) * 2) as usize];
            let _ = self.write_audio(&zeros);
        }
    }

    pub fn finish(self) -> Result<()> {
        unsafe { self.writer.Finalize() }
    }
}
