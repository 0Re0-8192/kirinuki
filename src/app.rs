//! Resident mode: tray icon, hotkeys, overlay -> still / recording.
use crate::bar::{self, Bar};
use crate::config::Config;
use crate::hook;
use crate::logf;
use crate::overlay::{self, Opts};
use crate::record::{Recorder, Target};
use crate::still;
use crate::util::{wide, Rect};
use std::cell::RefCell;
use std::path::PathBuf;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 40;
const TIMER_WATCH: usize = 7;
const MENU_STOP: usize = 1;
const MENU_FOLDER: usize = 2;
const MENU_EXIT: usize = 3;
const MENU_AUTOSTART: usize = 4;

struct Recording {
    rec: Recorder,
    bar: Bar,
    path: PathBuf,
    /// paused by us while the selection overlay is up (so the overlay isn't recorded)
    overlay_pause: bool,
}

struct App {
    hwnd: HWND,
    cfg: Config,
    recording: Option<Recording>,
    last_file: Option<PathBuf>,
    icon_idle: HICON,
    icon_rec: HICON,
    taskbar_created: u32,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

// ---------- tray ----------

/// Rounded dark square with a glyph (or a red dot when recording), alpha-correct.
unsafe fn make_icon(recording: bool) -> HICON {
    let size = GetSystemMetrics(SM_CXSMICON).max(16) * 2;
    let px = crate::render_offscreen(size, size, |dc| {
        let bg = CreateSolidBrush(COLORREF(0x00303030));
        FillRect(dc, &RECT { left: 0, top: 0, right: size, bottom: size }, bg);
        let _ = DeleteObject(bg.into());
        if recording {
            let red = CreateSolidBrush(COLORREF(0x003C3CE8));
            let ob = SelectObject(dc, red.into());
            let op = SelectObject(dc, GetStockObject(NULL_PEN));
            let m = size / 4;
            let _ = Ellipse(dc, m, m, size - m, size - m);
            SelectObject(dc, ob);
            SelectObject(dc, op);
            let _ = DeleteObject(red.into());
        } else {
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0x00FFFFFF));
            let f = wide("Segoe Fluent Icons");
            let font = CreateFontW(-(size * 7 / 10), 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, ANTIALIASED_QUALITY, 0, PCWSTR(f.as_ptr()));
            let of = SelectObject(dc, font.into());
            let mut g = [0xF407u16];
            let mut r = RECT { left: 0, top: 0, right: size, bottom: size };
            DrawTextW(dc, &mut g, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            SelectObject(dc, of);
            let _ = DeleteObject(font.into());
        }
    });
    // RGBA -> BGRA with a rounded-corner alpha mask
    let rad = size as f32 * 0.22;
    let mut bgra = vec![0u8; px.len()];
    for y in 0..size {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            let cx = (x as f32 + 0.5).clamp(rad, size as f32 - rad);
            let cy = (y as f32 + 0.5).clamp(rad, size as f32 - rad);
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let a = (rad - d + 0.5).clamp(0.0, 1.0);
            let a8 = (a * 255.0) as u32;
            for c in 0..3 {
                bgra[i + c] = ((px[i + 2 - c] as u32 * a8) / 255) as u8; // premultiplied
            }
            bgra[i + 3] = a8 as u8;
        }
    }
    let bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: size, biHeight: -size, biPlanes: 1, biBitCount: 32, ..Default::default() },
        ..Default::default()
    };
    let screen = GetDC(None);
    let mut bits = std::ptr::null_mut();
    let color = CreateDIBSection(Some(screen), &bi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
    ReleaseDC(None, screen);
    if !bits.is_null() {
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, bgra.len());
    }
    let mask = CreateBitmap(size, size, 1, 1, None);
    let ii = ICONINFO { fIcon: TRUE, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = CreateIconIndirect(&ii).unwrap_or_default();
    let _ = DeleteObject(mask.into());
    let _ = DeleteObject(color.into());
    icon
}

unsafe fn tray(hwnd: HWND, op: NOTIFY_ICON_MESSAGE, icon: HICON, tip: &str) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    for (i, c) in tip.encode_utf16().take(127).enumerate() {
        nid.szTip[i] = c;
    }
    let _ = Shell_NotifyIconW(op, &nid);
    if op == NIM_ADD {
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
    }
}

unsafe fn balloon(hwnd: HWND, title: &str, text: &str) {
    let mut nid = NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, uFlags: NIF_INFO, dwInfoFlags: NIIF_NONE | NIIF_RESPECT_QUIET_TIME, ..Default::default() };
    for (i, c) in title.encode_utf16().take(63).enumerate() {
        nid.szInfoTitle[i] = c;
    }
    for (i, c) in text.encode_utf16().take(255).enumerate() {
        nid.szInfo[i] = c;
    }
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

unsafe fn open_path(p: &std::path::Path, select: bool) {
    let (file, params) = if select {
        (wide("explorer.exe"), wide(&format!("/select,\"{}\"", p.display())))
    } else {
        (wide(&p.display().to_string()), wide(""))
    };
    ShellExecuteW(None, w!("open"), PCWSTR(file.as_ptr()), PCWSTR(params.as_ptr()), PCWSTR::null(), SW_SHOWNORMAL);
}

// ---------- actions ----------

fn open_overlay(app: &mut App, video: Option<bool>) {
    if overlay::is_open() {
        return;
    }
    let Some(shot) = still::grab_screen() else {
        logf!("app: screen grab failed");
        return;
    };
    let still_only = app.recording.is_some();
    if let Some(r) = app.recording.as_mut() {
        if !r.rec.shared.paused() {
            r.rec.shared.set_paused(true);
            r.overlay_pause = true;
        }
    }
    let o = Opts { shape: app.cfg.shape, video: if still_only { false } else { video.unwrap_or(app.cfg.video) }, audio: app.cfg.audio, still_only };
    overlay::open(app.hwnd, shot, o);
}

fn on_pick(app: &mut App, picked: bool) {
    if let Some((shape, video, audio)) = overlay::current_opts() {
        let changed = app.cfg.shape != shape || (app.recording.is_none() && app.cfg.video != video) || app.cfg.audio != audio;
        app.cfg.shape = shape;
        if app.recording.is_none() {
            app.cfg.video = video;
        }
        app.cfg.audio = audio;
        if changed {
            app.cfg.save();
        }
    }
    let (pick, shot) = overlay::close();
    if let Some(r) = app.recording.as_mut() {
        if r.overlay_pause {
            r.overlay_pause = false;
            r.rec.shared.set_paused(false);
        }
    }
    if !picked {
        return;
    }
    let (Some(pick), Some(shot)) = (pick, shot) else { return };
    if pick.video {
        start_recording(app, pick.rect, pick.audio, &shot);
    } else {
        save_still(app, &shot, &pick.rect);
    }
}

fn save_still(app: &mut App, shot: &still::Shot, r: &Rect) {
    let Some((w, h, bgra)) = shot.crop(r) else { return };
    let png = still::encode_png(w, h, &bgra);
    let copied = still::copy_image(app.hwnd, w, h, &bgra, &png);
    let _ = std::fs::create_dir_all(&app.cfg.save_dir);
    let path = still::new_path(&app.cfg.save_dir, "png");
    let saved = std::fs::write(&path, &png).is_ok();
    logf!("still: {w}x{h} copied={copied} saved={saved} {}", path.display());
    unsafe {
        if saved {
            app.last_file = Some(path.clone());
            balloon(app.hwnd, "コピーしました", &path.file_name().unwrap().to_string_lossy());
        } else {
            balloon(app.hwnd, "保存できません", &app.cfg.save_dir.display().to_string());
        }
    }
}

/// Region clipped to the monitor it overlaps most (recording works per monitor).
fn clip_to_monitor(r: &Rect) -> (HMONITOR, Rect) {
    unsafe {
        let m = MonitorFromRect(&r.to_win(), MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(m, &mut mi);
        (m, r.intersect(&Rect::from_win(&mi.rcMonitor)))
    }
}

fn start_recording(app: &mut App, r: Rect, audio: bool, shot: &still::Shot) {
    let (m, rect) = clip_to_monitor(&r);
    if rect.w() < 8 || rect.h() < 8 {
        return;
    }
    if std::fs::create_dir_all(&app.cfg.save_dir).is_err() {
        unsafe { balloon(app.hwnd, "保存できません", &app.cfg.save_dir.display().to_string()) };
        return;
    }
    let path = still::new_path(&app.cfg.save_dir, "mp4");
    let seed = shot.crop(&rect).map(|(_, _, px)| px);
    match crate::begin_recording(app.hwnd, Target { mon: m.0 as isize, rect }, path.display().to_string(), audio, seed) {
        Ok((rec, bar)) => {
            app.recording = Some(Recording { rec, bar, path, overlay_pause: false });
            unsafe {
                tray(app.hwnd, NIM_MODIFY, app.icon_rec, "kirinuki ●");
                SetTimer(Some(app.hwnd), TIMER_WATCH, 500, None);
            }
        }
        Err(e) => {
            logf!("app: record start failed: {e}");
            unsafe { balloon(app.hwnd, "録画できません", &e) };
        }
    }
}

fn stop_recording(app: &mut App) {
    let Some(r) = app.recording.take() else { return };
    unsafe {
        let _ = KillTimer(Some(app.hwnd), TIMER_WATCH);
    }
    r.bar.close();
    let res = r.rec.stop();
    unsafe { tray(app.hwnd, NIM_MODIFY, app.icon_idle, "kirinuki") };
    match res {
        Ok(rr) => {
            logf!("app: recorded {rr:?}");
            still::copy_file(app.hwnd, &r.path);
            app.last_file = Some(r.path.clone());
            unsafe { balloon(app.hwnd, "保存しました", &r.path.file_name().unwrap().to_string_lossy()) };
        }
        Err(e) => {
            logf!("app: record failed: {e}");
            unsafe { balloon(app.hwnd, "録画できません", &e) };
        }
    }
}

/// Shows the tray menu and returns the chosen command. Must be called WITHOUT the app borrowed:
/// the menu runs a modal loop that dispatches our own messages.
fn show_menu(hwnd: HWND, recording: bool) -> usize {
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return 0 };
        if recording {
            let _ = AppendMenuW(m, MF_STRING, MENU_STOP, w!("録画を停止"));
        }
        let _ = AppendMenuW(m, MF_STRING, MENU_FOLDER, w!("保存先を開く"));
        let auto = if crate::autostart::enabled() { MF_CHECKED } else { MF_UNCHECKED };
        let _ = AppendMenuW(m, MF_STRING | auto, MENU_AUTOSTART, w!("サインイン時に起動"));
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(m, MF_STRING, MENU_EXIT, w!("終了"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, pt.x, pt.y, None, hwnd, None);
        let _ = DestroyMenu(m);
        cmd.0 as usize
    }
}

fn menu_command(hwnd: HWND, cmd: usize) {
    match cmd {
        MENU_STOP => {
            with_app(stop_recording);
        }
        MENU_FOLDER => {
            with_app(|a| {
                let _ = std::fs::create_dir_all(&a.cfg.save_dir);
                unsafe { open_path(&a.cfg.save_dir, false) };
            });
        }
        MENU_AUTOSTART => {
            let on = !crate::autostart::enabled();
            let ok = crate::autostart::set(on);
            logf!("app: autostart {} {}", on, if ok { "ok" } else { "failed" });
        }
        MENU_EXIT => {
            with_app(stop_recording);
            // outside the borrow: WM_DESTROY needs the app too
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        _ => {}
    }
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        hook::WM_HK_SNIP => {
            with_app(|a| open_overlay(a, None));
            LRESULT(0)
        }
        hook::WM_HK_REC => {
            with_app(|a| {
                if a.recording.is_some() {
                    stop_recording(a)
                } else if !overlay::is_open() {
                    open_overlay(a, Some(true))
                }
            });
            LRESULT(0)
        }
        hook::WM_HK_ESC => {
            overlay::cancel();
            LRESULT(0)
        }
        overlay::WM_OVL_DONE => {
            with_app(|a| on_pick(a, wp.0 == 1));
            LRESULT(0)
        }
        bar::WM_BAR_STOP => {
            with_app(stop_recording);
            LRESULT(0)
        }
        bar::WM_BAR_PAUSE => {
            with_app(|a| {
                if let Some(r) = &a.recording {
                    r.rec.shared.set_paused(!r.rec.shared.paused());
                }
            });
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER_WATCH => {
            // recorder ended by itself (error): clean up
            with_app(|a| {
                if a.recording.as_ref().map(|r| r.rec.finished()).unwrap_or(false) {
                    stop_recording(a);
                }
            });
            LRESULT(0)
        }
        WM_TRAY => {
            let ev = (lp.0 & 0xFFFF) as u32;
            match ev {
                WM_LBUTTONUP => {
                    with_app(|a| {
                        if a.recording.is_some() {
                            stop_recording(a)
                        } else {
                            open_overlay(a, None)
                        }
                    });
                }
                WM_CONTEXTMENU => {
                    let recording = with_app(|a| a.recording.is_some()).unwrap_or(false);
                    let cmd = show_menu(hwnd, recording);
                    menu_command(hwnd, cmd);
                }
                x if x == NIN_BALLOONUSERCLICK => {
                    with_app(|a| {
                        if let Some(p) = &a.last_file {
                            open_path(p, true);
                        }
                    });
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            hook::uninstall();
            tray(hwnd, NIM_DELETE, HICON::default(), "");
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => {
            let tc = with_app(|a| a.taskbar_created).unwrap_or(0);
            if tc != 0 && msg == tc {
                with_app(|a| {
                    let icon = if a.recording.is_some() { a.icon_rec } else { a.icon_idle };
                    tray(a.hwnd, NIM_ADD, icon, "kirinuki")
                });
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

pub fn run() -> i32 {
    unsafe {
        let mutex = CreateMutexW(None, true, w!("Local\\kirinuki-resident"));
        if windows::Win32::Foundation::GetLastError() == ERROR_ALREADY_EXISTS {
            logf!("app: already running");
            return 0;
        }
        let _keep = mutex;
        overlay::register();
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSW { lpfnWndProc: Some(proc_), hInstance: hinst.into(), lpszClassName: w!("KirinukiApp"), ..Default::default() };
        RegisterClassW(&wc);
        let hwnd = match CreateWindowExW(WS_EX_TOOLWINDOW, w!("KirinukiApp"), w!("kirinuki"), WS_POPUP, 0, 0, 0, 0, None, None, Some(hinst.into()), None) {
            Ok(h) => h,
            Err(e) => {
                logf!("app: window failed: {e}");
                return 1;
            }
        };
        logf!("app: hwnd={:?}", hwnd.0);
        let cfg = Config::load();
        logf!("app: start, save_dir={}", cfg.save_dir.display());
        let app = App {
            hwnd,
            cfg,
            recording: None,
            last_file: None,
            icon_idle: make_icon(false),
            icon_rec: make_icon(true),
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
        };
        tray(hwnd, NIM_ADD, app.icon_idle, "kirinuki");
        APP.with(|a| *a.borrow_mut() = Some(app));
        hook::install(hwnd);
        let mut m = MSG::default();
        while GetMessageW(&mut m, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&m);
            DispatchMessageW(&m);
        }
        logf!("app: exit");
        0
    }
}
