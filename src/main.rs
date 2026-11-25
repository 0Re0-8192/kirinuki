#![windows_subsystem = "windows"]
mod app;
mod audio;
mod autostart;
mod bar;
mod config;
mod hook;
mod overlay;
mod still;
mod encoder;
mod record;
mod util;

use record::{Recorder, Target};
use util::Rect;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_TEST_STOP: u32 = WM_APP + 50;

fn attach_console() {
    unsafe {
        let _ = windows::Win32::System::Console::AttachConsole(windows::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
}

pub fn monitor_of(r: &Rect) -> (HMONITOR, Rect, Rect) {
    unsafe {
        let m = MonitorFromRect(&r.to_win(), MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(m, &mut mi);
        (m, Rect::from_win(&mi.rcMonitor), Rect::from_win(&mi.rcWork))
    }
}

pub fn other_work_areas(except: HMONITOR) -> Vec<Rect> {
    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        let v = &mut *(lp.0 as *mut Vec<(HMONITOR, Rect)>);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(m, &mut mi);
        v.push((m, Rect::from_win(&mi.rcWork)));
        TRUE
    }
    let mut v: Vec<(HMONITOR, Rect)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v.into_iter().filter(|(m, _)| *m != except).map(|(_, r)| r).collect()
}

pub fn scale_for(m: HMONITOR) -> f32 {
    let (mut x, mut y) = (96u32, 96u32);
    unsafe {
        let _ = GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut x, &mut y);
    }
    x as f32 / 96.0
}

/// Starts recording + bar. Returns the bar and recorder.
pub fn begin_recording(owner: HWND, target: Target, path: String, audio: bool, seed: Option<Vec<u8>>) -> std::result::Result<(Recorder, bar::Bar), String> {
    let rec = Recorder::start(target, path, audio, seed)?;
    let anchor = target.rect;
    let region = Some(anchor);
    let (m, _, work) = monitor_of(&anchor);
    let scale = scale_for(m);
    let (bw, bh) = ((bar::BAR_W as f32 * scale) as i32, (bar::BAR_H as f32 * scale) as i32);
    let gap = (8.0 * scale) as i32 + bar::BORDER;
    let (x, y, inside) = util::place_bar(&anchor, &work, &other_work_areas(m), bw, bh, gap);
    logf!("bar at {x},{y} {bw}x{bh} inside={inside}");
    let b = bar::Bar::show(owner, rec.shared.clone(), x, y, scale, region);
    Ok((rec, b))
}

// ---------- test mode: record a pattern window we own ----------

static mut PATTERN_N: u32 = 0;

unsafe extern "system" fn pattern_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            PATTERN_N += 1;
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let win_dc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            // double-buffered so a capture never sees a half-drawn frame
            let hdc = CreateCompatibleDC(Some(win_dc));
            let bmp = CreateCompatibleBitmap(win_dc, rc.right, rc.bottom);
            let old_bmp = SelectObject(hdc, bmp.into());
            let n = PATTERN_N;
            let col = [0x00D04040u32, 0x0040D040, 0x004040D0, 0x00D0D040][(n / 10 % 4) as usize];
            let br = CreateSolidBrush(COLORREF(col));
            FillRect(hdc, &rc, br);
            let _ = DeleteObject(br.into());
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(0x00FFFFFF));
            let mut t: Vec<u16> = format!("{n}").encode_utf16().collect();
            let f = CreateFontW(-60, 0, 0, 0, 700, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, w!("Segoe UI"));
            let of = SelectObject(hdc, f.into());
            DrawTextW(hdc, &mut t, &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            SelectObject(hdc, of);
            let _ = DeleteObject(f.into());
            let _ = BitBlt(win_dc, 0, 0, rc.right, rc.bottom, Some(hdc), 0, 0, SRCCOPY);
            SelectObject(hdc, old_bmp);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(hdc);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        bar::WM_BAR_STOP | WM_TEST_STOP => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        bar::WM_BAR_PAUSE => {
            if let Some(s) = TEST_SHARED.lock().unwrap().as_ref() {
                s.set_paused(!s.paused());
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let _ = KillTimer(Some(hwnd), wp.0);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

static TEST_SHARED: std::sync::Mutex<Option<std::sync::Arc<record::Shared>>> = std::sync::Mutex::new(None);

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

unsafe fn message_window() -> HWND {
    let hinst = GetModuleHandleW(None).unwrap_or_default();
    let wc = WNDCLASSW { lpfnWndProc: Some(main_proc), hInstance: hinst.into(), lpszClassName: w!("KirinukiMain"), ..Default::default() };
    RegisterClassW(&wc);
    CreateWindowExW(WINDOW_EX_STYLE(0), w!("KirinukiMain"), w!("kirinuki"), WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(hinst.into()), None).unwrap_or_default()
}

unsafe fn pump() {
    let mut m = MSG::default();
    while GetMessageW(&mut m, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&m);
        DispatchMessageW(&m);
    }
}

/// --test-record [--seconds N] [--out path] [--no-audio] [--region l,t,r,b] [--pause-at S]
unsafe fn test_record(args: &[String]) -> i32 {
    let secs: u64 = arg(args, "--seconds").and_then(|s| s.parse().ok()).unwrap_or(5);
    let out = arg(args, "--out").unwrap_or_else(|| std::env::temp_dir().join("kirinuki_test.mp4").display().to_string());
    let audio = !args.iter().any(|a| a == "--no-audio");
    let main = message_window();
    let hinst = GetModuleHandleW(None).unwrap_or_default();

    let region = if let Some(r) = arg(args, "--region") {
        let v: Vec<i32> = r.split(',').filter_map(|s| s.trim().parse().ok()).collect();
        Rect::new(v[0], v[1], v[2], v[3])
    } else {
        // pattern window at the bottom right of the primary work area
        let wc = WNDCLASSW { lpfnWndProc: Some(pattern_proc), hInstance: hinst.into(), lpszClassName: w!("KirinukiPattern"), ..Default::default() };
        RegisterClassW(&wc);
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let (pw, ph) = (320, 200);
        let r = Rect::new(work.right - pw - 40, work.bottom - ph - 80, work.right - 40, work.bottom - 80);
        let h = CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, w!("KirinukiPattern"), w!(""), WS_POPUP, r.l, r.t, r.w(), r.h(), None, None, Some(hinst.into()), None).unwrap_or_default();
        let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        SetTimer(Some(h), 1, 33, None);
        // let it paint before we start
        let t0 = std::time::Instant::now();
        let mut m = MSG::default();
        while t0.elapsed().as_millis() < 300 {
            while PeekMessageW(&mut m, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&m);
                DispatchMessageW(&m);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        r
    };
    let (mon, mon_rect, _) = monitor_of(&region);
    let rect = region.intersect(&mon_rect);
    let target = Target { mon: mon.0 as isize, rect };
    let (rec, b) = match begin_recording(main, target, out.clone(), audio, None) {
        Ok(v) => v,
        Err(e) => {
            logf!("TEST FAIL start: {e}");
            return 2;
        }
    };
    let br = b.rect();
    logf!("TEST bar_rect={},{},{},{} region={},{},{},{}", br.l, br.t, br.r, br.b, rect.l, rect.t, rect.r, rect.b);
    *TEST_SHARED.lock().unwrap() = Some(rec.shared.clone());
    if let Some(p) = arg(args, "--pause-at").and_then(|s| s.parse::<u64>().ok()) {
        // pause for 1 s at p seconds (wall time)
        let s = rec.shared.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(p));
            s.set_paused(true);
            std::thread::sleep(std::time::Duration::from_secs(1));
            s.set_paused(false);
        });
    }
    SetTimer(Some(main), 1, (secs * 1000) as u32, None);
    pump();
    b.close();
    match rec.stop() {
        Ok(r) => {
            logf!("TEST OK {r:?}");
            0
        }
        Err(e) => {
            logf!("TEST FAIL stop: {e}");
            3
        }
    }
}

/// --bar-png path : bar rendered offscreen (for looking at it without showing a window)
unsafe fn bar_png(args: &[String]) -> i32 {
    let path = arg(args, "--bar-png").unwrap();
    let scale = 2.0f32;
    let (w, h) = ((bar::BAR_W as f32 * scale) as i32, (bar::BAR_H as f32 * scale) as i32);
    let mut rows: Vec<u8> = Vec::new();
    for (i, (paused, hot)) in [(false, -1), (true, 1)].iter().enumerate() {
        let _ = i;
        let px = render_offscreen(w, h, |dc| bar::draw(dc, scale, w, h, 83, *paused, *hot));
        rows.extend_from_slice(&px);
    }
    let f = std::fs::File::create(&path).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(f), w as u32, (h * 2) as u32);
    e.set_color(png::ColorType::Rgba);
    let mut wr = e.write_header().unwrap();
    wr.write_image_data(&rows).unwrap();
    0
}

/// --overlay-png path : overlay over a synthetic screen, still + video variants stacked
unsafe fn overlay_png(args: &[String]) -> i32 {
    let path = arg(args, "--overlay-png").unwrap();
    let (w, h) = (1200i32, 600i32);
    let mut bgra = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let win = (x > 150 && x < 650 && y > 120 && y < 480) || (x > 700 && x < 1100 && y > 200 && y < 550);
            let (b, g, r) = if win { (235u8, 235, 235) } else { ((x / 5) as u8, (y / 3) as u8, 120) };
            bgra.extend_from_slice(&[b, g, r, 255]);
        }
    }
    let shot = still::Shot { rect: Rect::new(0, 0, w, h), bgra };
    let mut rows = Vec::new();
    let o1 = overlay::Opts { shape: config::Shape::Rect, video: false, audio: true, still_only: false };
    rows.extend(overlay::render_preview(&shot, o1, Some(Rect::new(200, 160, 600, 420)), Some(overlay::Btn::Window), 1.5));
    let o2 = overlay::Opts { shape: config::Shape::Window, video: true, audio: false, still_only: false };
    rows.extend(overlay::render_preview(&shot, o2, Some(Rect::new(700, 200, 1100, 550)), None, 1.5));
    let f = std::fs::File::create(&path).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(f), w as u32, (h * 2) as u32);
    e.set_color(png::ColorType::Rgba);
    e.write_header().unwrap().write_image_data(&rows).unwrap();
    0
}

unsafe fn glyphs_png(args: &[String]) -> i32 {
    let path = arg(args, "--glyphs").unwrap();
    let cps: [u16; 16] = [0xF407, 0xE7A8, 0xE923, 0xE737, 0xE7F4, 0xE722, 0xE714, 0xE711, 0xECAA, 0xE7C4, 0xE8A9, 0xE740, 0xE9A6, 0xF408, 0xE7B3, 0xEDA4];
    let (cw, h) = (64, 64);
    let w = cw * cps.len() as i32;
    let px = render_offscreen(w, h, |dc| {
        let bg = CreateSolidBrush(COLORREF(0x00202020));
        FillRect(dc, &RECT { left: 0, top: 0, right: w, bottom: h }, bg);
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00F0F0F0));
        let f = CreateFontW(-32, 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, w!("Segoe Fluent Icons"));
        SelectObject(dc, f.into());
        for (i, c) in cps.iter().enumerate() {
            let mut s = [*c];
            let mut r = RECT { left: i as i32 * cw, top: 0, right: (i as i32 + 1) * cw, bottom: h };
            DrawTextW(dc, &mut s, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        }
    });
    let f = std::fs::File::create(&path).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
    e.set_color(png::ColorType::Rgba);
    e.write_header().unwrap().write_image_data(&px).unwrap();
    0
}

/// Renders with GDI into a top-down 32-bit DIB, returns RGBA.
pub unsafe fn render_offscreen(w: i32, h: i32, f: impl FnOnce(HDC)) -> Vec<u8> {
    let screen = GetDC(None);
    let dc = CreateCompatibleDC(Some(screen));
    let bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, ..Default::default() },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let bmp = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
    let old = SelectObject(dc, bmp.into());
    f(dc);
    let _ = GdiFlush();
    let src = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
    let mut out = Vec::with_capacity(src.len());
    for p in src.chunks(4) {
        out.extend_from_slice(&[p[2], p[1], p[0], 255]);
    }
    SelectObject(dc, old);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(dc);
    ReleaseDC(None, screen);
    out
}

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    attach_console();
    util::log_init();
    bar::register();
    let args: Vec<String> = std::env::args().collect();
    logf!("start: args={:?}", &args[1..]);
    let code = unsafe {
        if args.iter().any(|a| a == "--test-record") {
            test_record(&args)
        } else if args.iter().any(|a| a == "--bar-png") {
            bar_png(&args)
        } else if args.iter().any(|a| a == "--glyphs") {
            glyphs_png(&args)
        } else if args.iter().any(|a| a == "--overlay-png") {
            overlay_png(&args)
        } else {
            app::run()
        }
    };
    std::process::exit(code);
}
