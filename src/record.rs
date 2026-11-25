//! Screen recording of a region: Desktop Duplication -> crop -> cursor -> Encoder, fixed frame rate.
//! (Desktop Duplication is also what CRD/VD use; it works without capture permissions.)
use crate::encoder::Encoder;
use crate::logf;
use crate::util::{even, Rect};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const FPS: u32 = 30;

#[derive(Clone, Copy, Debug)]
pub struct Target {
    /// Monitor holding the region (HMONITOR value).
    pub mon: isize,
    /// Region in physical screen pixels, inside that monitor.
    pub rect: Rect,
}

struct Clock {
    start: Instant,
    paused_total: Duration,
    paused_at: Option<Instant>,
}

pub struct Shared {
    stop: AtomicBool,
    clock: Mutex<Clock>,
    /// Screen rect of our own UI (stop bar); blanked in the video wherever it overlaps the region.
    mask: Mutex<Option<Rect>>,
}

impl Shared {
    fn new() -> Self {
        Shared {
            stop: AtomicBool::new(false),
            clock: Mutex::new(Clock { start: Instant::now(), paused_total: Duration::ZERO, paused_at: None }),
            mask: Mutex::new(None),
        }
    }
    pub fn set_mask(&self, r: Option<Rect>) {
        *self.mask.lock().unwrap() = r;
    }
    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
    pub fn paused(&self) -> bool {
        self.clock.lock().unwrap().paused_at.is_some()
    }
    /// Recorded time so far (pauses excluded).
    pub fn active(&self) -> Duration {
        let c = self.clock.lock().unwrap();
        let now = c.paused_at.unwrap_or_else(Instant::now);
        now.saturating_duration_since(c.start).saturating_sub(c.paused_total)
    }
    pub fn set_paused(&self, p: bool) {
        let mut c = self.clock.lock().unwrap();
        match (p, c.paused_at) {
            (true, None) => c.paused_at = Some(Instant::now()),
            (false, Some(at)) => {
                c.paused_total += at.elapsed();
                c.paused_at = None;
            }
            _ => {}
        }
    }
    fn restart_clock(&self) {
        let mut c = self.clock.lock().unwrap();
        c.start = Instant::now();
        c.paused_total = Duration::ZERO;
        c.paused_at = None;
    }
}

pub struct Recorder {
    pub shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<std::result::Result<RecordResult, String>>>,
}

#[derive(Debug)]
pub struct RecordResult {
    pub path: String,
    pub frames: u64,
    pub audio_frames: u64,
    pub gpu: bool,
    pub w: u32,
    pub h: u32,
}

impl Recorder {
    /// `seed`: BGRA image of the region (rect size) shown until the first desktop update arrives.
    pub fn start(target: Target, path: String, audio: bool, seed: Option<Vec<u8>>) -> std::result::Result<Recorder, String> {
        let shared = Arc::new(Shared::new());
        let (tx, rx) = mpsc::channel();
        let sh = shared.clone();
        let th = std::thread::spawn(move || video_thread(target, path, audio, seed, sh, tx));
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(Recorder { shared, thread: Some(th) }),
            Ok(Err(e)) => {
                let _ = th.join();
                Err(e)
            }
            Err(_) => {
                shared.stop.store(true, Ordering::SeqCst);
                Err("recorder did not start in time".into())
            }
        }
    }

    /// Stops and finalizes; blocks until the file is written.
    pub fn stop(mut self) -> std::result::Result<RecordResult, String> {
        self.shared.stop.store(true, Ordering::SeqCst);
        match self.thread.take().unwrap().join() {
            Ok(r) => r,
            Err(_) => Err("recorder thread panicked".into()),
        }
    }

    /// True once the recorder ended on its own (error).
    pub fn finished(&self) -> bool {
        self.thread.as_ref().map(|t| t.is_finished()).unwrap_or(true)
    }
}

fn err<E: std::fmt::Display>(what: &str) -> impl Fn(E) -> String + '_ {
    move |e| format!("{what}: {e}")
}

// ---------- desktop duplication ----------

struct Dupl {
    dev: ID3D11Device,
    output: IDXGIOutput1,
    dupl: Option<IDXGIOutputDuplication>,
    /// Output rect in physical desktop coordinates.
    desktop: Rect,
    last_try: Instant,
}

/// Device on the adapter that owns `mon`, plus that output.
fn open_output(mon: HMONITOR) -> Result<(ID3D11Device, IDXGIOutput1, Rect)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            ai += 1;
            let mut oi = 0;
            while let Ok(out) = adapter.EnumOutputs(oi) {
                oi += 1;
                let desc = out.GetDesc()?;
                if desc.Monitor != mon {
                    continue;
                }
                let mut dev = None;
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut dev),
                    None,
                    None,
                )?;
                let dev: ID3D11Device = dev.unwrap();
                let mt: ID3D11Multithread = dev.cast()?;
                let _ = mt.SetMultithreadProtected(true);
                if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY && desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED {
                    return Err(Error::new(E_NOTIMPL, "rotated monitors are not supported"));
                }
                let r = desc.DesktopCoordinates;
                return Ok((dev, out.cast()?, Rect::new(r.left, r.top, r.right, r.bottom)));
            }
        }
        Err(Error::new(E_FAIL, "monitor not found among DXGI outputs"))
    }
}

impl Dupl {
    fn open(mon: HMONITOR) -> Result<Dupl> {
        let (dev, output, desktop) = open_output(mon)?;
        let mut d = Dupl { dev, output, dupl: None, desktop, last_try: Instant::now() };
        d.reopen()?;
        Ok(d)
    }

    fn reopen(&mut self) -> Result<()> {
        self.dupl = None;
        self.last_try = Instant::now();
        unsafe {
            // resolution / layout changed: the crop would be wrong, so end the recording
            let desc = self.output.GetDesc()?;
            let r = desc.DesktopCoordinates;
            if Rect::new(r.left, r.top, r.right, r.bottom) != self.desktop {
                return Err(Error::new(E_CHANGED_STATE, "display layout changed"));
            }
            // DuplicateOutput1 is the variant for per-monitor-DPI-aware processes
            let d = match self.output.cast::<IDXGIOutput5>() {
                Ok(o5) => o5.DuplicateOutput1(&self.dev, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])?,
                Err(_) => self.output.DuplicateOutput(&self.dev)?,
            };
            self.dupl = Some(d);
        }
        Ok(())
    }

    /// Copies the newest desktop image's `src` box into `dst` at (0,0). Ok(true) if a new image was copied.
    fn grab(&mut self, ctx: &ID3D11DeviceContext, dst: &ID3D11Texture2D, src: &Rect) -> Result<bool> {
        let Some(d) = self.dupl.as_ref() else {
            // lost earlier (UAC, lock screen): retry twice a second, keeping the last image meanwhile
            if self.last_try.elapsed() > Duration::from_millis(500) {
                match self.reopen() {
                    Ok(()) => logf!("record: duplication reopened"),
                    Err(e) if e.code() == E_CHANGED_STATE => return Err(e),
                    Err(_) => {}
                }
            }
            return Ok(false);
        };
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res = None;
            match d.AcquireNextFrame(0, &mut info, &mut res) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    logf!("record: duplication lost, reopening");
                    self.dupl = None;
                    return Ok(false);
                }
                Err(e) => return Err(e),
            }
            let mut copied = false;
            if info.LastPresentTime != 0 {
                if let Some(tex) = res.as_ref().and_then(|r| r.cast::<ID3D11Texture2D>().ok()) {
                    let b = D3D11_BOX { left: src.l as u32, top: src.t as u32, front: 0, right: src.r as u32, bottom: src.b as u32, back: 1 };
                    ctx.CopySubresourceRegion(dst, 0, 0, 0, 0, &tex, 0, Some(&b));
                    copied = true;
                }
            }
            drop(res);
            let _ = d.ReleaseFrame();
            Ok(copied)
        }
    }
}

// ---------- cursor ----------

#[derive(Default)]
struct CursorCache {
    hot: HashMap<isize, (i32, i32)>,
}

impl CursorCache {
    /// Draws the current cursor into `tex` (GDI-compatible), `origin` = region top-left on the desktop.
    unsafe fn draw(&mut self, tex: &ID3D11Texture2D, origin: (i32, i32)) {
        let mut ci = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
        if GetCursorInfo(&mut ci).is_err() || ci.flags.0 & CURSOR_SHOWING.0 == 0 || ci.hCursor.is_invalid() {
            return;
        }
        let key = ci.hCursor.0 as isize;
        let (hx, hy) = *self.hot.entry(key).or_insert_with(|| {
            let mut ii = ICONINFO::default();
            if GetIconInfo(HICON(ci.hCursor.0), &mut ii).is_ok() {
                if !ii.hbmMask.is_invalid() {
                    let _ = windows::Win32::Graphics::Gdi::DeleteObject(ii.hbmMask.into());
                }
                if !ii.hbmColor.is_invalid() {
                    let _ = windows::Win32::Graphics::Gdi::DeleteObject(ii.hbmColor.into());
                }
                (ii.xHotspot as i32, ii.yHotspot as i32)
            } else {
                (0, 0)
            }
        });
        let Ok(surf) = tex.cast::<IDXGISurface1>() else { return };
        let Ok(dc) = surf.GetDC(false) else { return };
        let x = ci.ptScreenPos.x - origin.0 - hx;
        let y = ci.ptScreenPos.y - origin.1 - hy;
        let _ = DrawIconEx(dc, x, y, HICON(ci.hCursor.0), 0, 0, 0, None, DI_NORMAL);
        let _ = surf.ReleaseDC(None);
    }
}

// ---------- recording thread ----------

fn tex_desc(w: u32, h: u32, gdi: bool) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: if gdi { D3D11_RESOURCE_MISC_GDI_COMPATIBLE.0 as u32 } else { 0 },
    }
}

fn new_tex(dev: &ID3D11Device, w: u32, h: u32, gdi: bool) -> Result<ID3D11Texture2D> {
    unsafe {
        let mut t = None;
        dev.CreateTexture2D(&tex_desc(w, h, gdi), None, Some(&mut t))?;
        Ok(t.unwrap())
    }
}

fn open_encoder(path: &str, dev: &ID3D11Device, w: u32, h: u32, audio: bool) -> std::result::Result<(Encoder, Option<IMFDXGIDeviceManager>), String> {
    unsafe {
        let mut token = 0u32;
        let mut mgr: Option<IMFDXGIDeviceManager> = None;
        if MFCreateDXGIDeviceManager(&mut token, &mut mgr).is_ok() {
            let m = mgr.clone().unwrap();
            if m.ResetDevice(dev, token).is_ok() {
                match Encoder::new(path, dev, Some(&m), w, h, FPS, audio) {
                    Ok(e) => return Ok((e, Some(m))),
                    Err(e) => logf!("record: gpu encoder failed, falling back: {e}"),
                }
            }
        }
        let _ = std::fs::remove_file(path);
        Encoder::new(path, dev, None, w, h, FPS, audio).map(|e| (e, None)).map_err(err("encoder"))
    }
}

fn video_thread(target: Target, path: String, audio: bool, seed: Option<Vec<u8>>, shared: Arc<Shared>, ready: mpsc::Sender<std::result::Result<(), String>>) -> std::result::Result<RecordResult, String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    }
    let r = video_inner(target, path, audio, seed, shared.clone(), &ready);
    if let Err(e) = &r {
        logf!("record: {e}");
        let _ = ready.send(Err(e.clone()));
    }
    shared.stop.store(true, Ordering::SeqCst);
    unsafe {
        let _ = MFShutdown();
    }
    r
}

fn video_inner(target: Target, path: String, audio: bool, seed: Option<Vec<u8>>, shared: Arc<Shared>, ready: &mpsc::Sender<std::result::Result<(), String>>) -> std::result::Result<RecordResult, String> {
    let mut dupl = Dupl::open(HMONITOR(target.mon as *mut _)).map_err(err("duplication"))?;
    let dev = dupl.dev.clone();
    let ctx = unsafe { dev.GetImmediateContext() }.map_err(err("d3d11 ctx"))?;

    let (w, h) = (even(target.rect.w()), even(target.rect.h()));
    // source box inside the output, clamped
    let d = dupl.desktop;
    let src = Rect::new(target.rect.l - d.l, target.rect.t - d.t, target.rect.l - d.l + w as i32, target.rect.t - d.t + h as i32)
        .intersect(&Rect::new(0, 0, d.w(), d.h()));
    if src.is_empty() {
        return Err("region is outside the monitor".into());
    }
    let (enc, _mgr) = open_encoder(&path, &dev, w, h, audio)?;
    let gpu = enc.is_gpu();
    logf!("record: {target:?} -> {path} {w}x{h} src={src:?} gpu={gpu} audio={audio}");
    let enc = Arc::new(Mutex::new(Some(enc)));

    let latest = new_tex(&dev, w, h, false).map_err(err("texture"))?;
    let mut got_any = false;
    unsafe {
        let mut rtv = None;
        if dev.CreateRenderTargetView(&latest, None, Some(&mut rtv)).is_ok() {
            ctx.ClearRenderTargetView(rtv.as_ref().unwrap(), &[0.0, 0.0, 0.0, 1.0]);
        }
        // start from the frozen screenshot so the video never begins black
        if let Some(px) = seed.as_ref() {
            let (rw, rh) = (target.rect.w() as u32, target.rect.h() as u32);
            if px.len() == (rw * rh * 4) as usize {
                let b = D3D11_BOX { left: 0, top: 0, front: 0, right: rw.min(w), bottom: rh.min(h), back: 1 };
                ctx.UpdateSubresource(&latest, 0, Some(&b), px.as_ptr() as *const _, rw * 4, 0);
                got_any = true;
            }
        }
    }
    drop(seed);
    let ctx1: Option<ID3D11DeviceContext1> = ctx.cast().ok();

    shared.restart_clock();
    let audio_th = if audio {
        let (s, e) = (shared.clone(), enc.clone());
        Some(std::thread::spawn(move || crate::audio::run(s, e)))
    } else {
        None
    };
    let _ = ready.send(Ok(()));

    unsafe {
        let _ = timeBeginPeriod(1);
    }
    let dur = 10_000_000i64 / FPS as i64;
    let origin = (target.rect.l, target.rect.t);
    let mut cursor = CursorCache::default();
    let mut last_n: i64 = -1;
    let mut fail: Option<String> = None;
    let mut grab_errors = 0;
    let mut dropped = 0u64;
    let mut max_backlog = 0u64;
    let mut was_paused = false;
    let mut resume_hold: Option<Instant> = None;
    let mut prev_mask: Option<Rect> = None;
    let mut mask_moved = 0u32;
    let frame_bytes = (w as u64 * h as u64 * 4).max(1);
    let backlog_limit = (FPS as u64 * 3 / 2).min((1u64 << 30) / frame_bytes).max(24);
    while !shared.stopping() {
        // While paused (and briefly after), keep the last image: our overlay may still be on screen.
        let paused_now = shared.paused();
        if was_paused && !paused_now {
            resume_hold = Some(Instant::now());
        }
        was_paused = paused_now;
        let holding = paused_now || resume_hold.map(|t| t.elapsed() < Duration::from_millis(100)).unwrap_or(false);
        let grabbed = if holding { Ok(false) } else { dupl.grab(&ctx, &latest, &src) };
        match grabbed {
            Ok(true) => got_any = true,
            Ok(false) => {}
            Err(e) if e.code() == E_CHANGED_STATE => {
                logf!("record: {e}, stopping");
                break;
            }
            Err(e) => {
                grab_errors += 1;
                if grab_errors % 30 == 1 {
                    logf!("record: grab failed: {e}");
                }
            }
        }
        // the first image only arrives when something changes; a still desktop is still a valid start
        if !got_any && shared.active() > Duration::from_millis(300) {
            got_any = true;
        }
        if !shared.paused() && got_any {
            let n = (shared.active().as_nanos() as i64 / 100) / dur;
            if n > last_n {
                // encoder falling behind: drop this frame instead of queueing without limit
                let backlog = enc.lock().unwrap().as_ref().map(|e| e.video_backlog()).unwrap_or(0);
                max_backlog = max_backlog.max(backlog);
                // Hardware encoders hold several frames before producing output, so the limit must sit well
                // above that or the pipeline starves: 1.5 s of frames, capped at ~1 GB of queued textures.
                if backlog > backlog_limit && last_n >= 0 {
                    dropped += 1;
                    if dropped % 30 == 1 {
                        logf!("record: encoder behind ({backlog} queued), dropping frames ({dropped})");
                    }
                    last_n = n;
                } else {
                    let out = match new_tex(&dev, w, h, true) {
                        Ok(t) => t,
                        Err(e) => {
                            fail = Some(format!("texture: {e}"));
                            break;
                        }
                    };
                    unsafe {
                        ctx.CopyResource(&out, &latest);
                        cursor.draw(&out, origin);
                        // our own bar, if it sits over the region, is blanked
                        // while the bar moves, the captured image may still show it at the old place
                        let cur = *shared.mask.lock().unwrap();
                        if cur != prev_mask {
                            mask_moved = 3;
                        }
                        let mask = match (cur, prev_mask) {
                            (Some(a), Some(b)) if mask_moved > 0 => Some(Rect::new(a.l.min(b.l), a.t.min(b.t), a.r.max(b.r), a.b.max(b.b))),
                            _ => cur,
                        };
                        if mask_moved > 0 {
                            mask_moved -= 1;
                            if mask_moved == 0 {
                                prev_mask = cur;
                            }
                        } else {
                            prev_mask = cur;
                        }
                        if let (Some(m), Some(c1)) = (mask, ctx1.as_ref()) {
                            let r = target.rect;
                            let rel = Rect::new(m.l - r.l, m.t - r.t, m.r - r.l, m.b - r.t).intersect(&Rect::new(0, 0, w as i32, h as i32));
                            if !rel.is_empty() {
                                let mut rtv = None;
                                if dev.CreateRenderTargetView(&out, None, Some(&mut rtv)).is_ok() {
                                    if let Some(view) = rtv.and_then(|v| v.cast::<ID3D11View>().ok()) {
                                        c1.ClearView(&view, &[0.13, 0.13, 0.13, 1.0], Some(&[rel.to_win()]));
                                    }
                                }
                            }
                        }
                    }
                    // the first frame covers t=0 so video and audio start together
                    let (t, dd) = if last_n < 0 { (0, (n + 1) * dur) } else { (n * dur, dur) };
                    let mut g = enc.lock().unwrap();
                    if let Err(e) = g.as_mut().unwrap().write_video(&out, t, dd) {
                        fail = Some(format!("video write: {e}"));
                        break;
                    }
                    last_n = n;
                }
            }
        }
        let next = Duration::from_nanos(((last_n + 1).max(0) * dur * 100) as u64);
        let now = shared.active();
        let wait = if next > now { (next - now).min(Duration::from_millis(15)) } else { Duration::from_millis(1) };
        std::thread::sleep(wait);
    }
    unsafe {
        let _ = timeEndPeriod(1);
    }
    shared.stop.store(true, Ordering::SeqCst);
    if let Some(t) = audio_th {
        let _ = t.join();
    }
    drop(dupl);
    let mut enc = enc.lock().unwrap().take().unwrap();
    // audio ends where the video ends (silence after a quiet tail)
    enc.pad_audio_to(((last_n + 1).max(0) * dur) as u64);
    let (frames, audio_frames) = (enc.frames, enc.audio_frames);
    if let Some(f) = fail {
        // keep what was recorded so far
        logf!("record: {f}");
        if frames > 0 && enc.finish().is_ok() {
            logf!("record: saved partial recording ({frames} frames)");
            return Ok(RecordResult { path, frames, audio_frames, gpu, w, h });
        }
        return Err(f);
    }
    if frames == 0 {
        drop(enc);
        let _ = std::fs::remove_file(&path);
        return Err("no frames captured".into());
    }
    enc.finish().map_err(err("finalize"))?;
    logf!("record: done {frames} frames, {audio_frames} audio frames, max backlog {max_backlog}/{backlog_limit}, dropped {dropped}");
    Ok(RecordResult { path, frames, audio_frames, gpu, w, h })
}
