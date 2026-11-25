//! Full-screen selection overlay over a frozen screenshot: rectangle / window / screen, still / video.
use crate::config::Shape;
use crate::still::Shot;
use crate::util::{wide, Rect};
use std::cell::RefCell;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_OVL_DONE: u32 = WM_APP + 30; // wparam 1 = picked, 0 = cancelled
const CLASS: PCWSTR = w!("KirinukiOverlay");

#[derive(Clone, Copy, Debug)]
pub struct Pick {
    pub rect: Rect,
    pub video: bool,
    pub audio: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Btn {
    Rect,
    Window,
    Screen,
    Still,
    Video,
    Audio,
    Close,
}

pub struct Opts {
    pub shape: Shape,
    pub video: bool,
    pub audio: bool,
    /// video not offered (already recording)
    pub still_only: bool,
}

struct Ovl {
    owner: HWND,
    hwnd: HWND,
    vs: Rect,
    shot: Option<Shot>,
    bright: (HDC, HBITMAP, HGDIOBJ),
    dim: (HDC, HBITMAP, HGDIOBJ),
    back: (HDC, HBITMAP, HGDIOBJ),
    o: Opts,
    drag: Option<(i32, i32)>,
    sel: Option<Rect>,
    hover: Option<Rect>,
    wins: Vec<Rect>,
    mons: Vec<Rect>,
    /// toolbar origin (client coords) and scale
    bar_xy: (i32, i32),
    scale: f32,
    hot: Option<Btn>,
    result: Option<Pick>,
}

thread_local! {
    static OVL: RefCell<Option<Ovl>> = const { RefCell::new(None) };
}

pub fn is_open() -> bool {
    OVL.with(|o| o.borrow().is_some())
}

// ---------- layout (pure, unit-tested) ----------

pub const BTN: i32 = 40;
const PAD: i32 = 6;
const SEP: i32 = 9;

/// Buttons with their x offsets (96-dpi units) and the total width.
pub fn layout(video: bool, still_only: bool) -> (Vec<(Btn, i32)>, i32) {
    let mut groups: Vec<Vec<Btn>> = vec![vec![Btn::Rect, Btn::Window, Btn::Screen]];
    if !still_only {
        groups.push(vec![Btn::Still, Btn::Video]);
    }
    if video && !still_only {
        groups.push(vec![Btn::Audio]);
    }
    groups.push(vec![Btn::Close]);
    let mut x = PAD;
    let mut out = vec![];
    for (gi, g) in groups.iter().enumerate() {
        if gi > 0 {
            x += SEP;
        }
        for b in g {
            out.push((*b, x));
            x += BTN;
        }
    }
    (out, x + PAD)
}

pub fn bar_height() -> i32 {
    BTN + PAD * 2
}

fn s(v: i32, scale: f32) -> i32 {
    (v as f32 * scale).round() as i32
}

// ---------- window / monitor lists ----------

fn top_windows(skip_owner_pid: u32) -> Vec<Rect> {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let (v, pid) = &mut *(lp.0 as *mut (Vec<Rect>, u32));
        if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
            return TRUE;
        }
        let mut p = 0u32;
        GetWindowThreadProcessId(h, Some(&mut p));
        if p == *pid {
            return TRUE;
        }
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TRANSPARENT.0 != 0 {
            return TRUE;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4);
        if cloaked != 0 {
            return TRUE;
        }
        let mut cls = [0u16; 64];
        let n = GetClassNameW(h, &mut cls) as usize;
        let cls = String::from_utf16_lossy(&cls[..n]);
        if cls == "Progman" || cls == "WorkerW" {
            return TRUE;
        }
        let mut r = RECT::default();
        if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as *mut _, std::mem::size_of::<RECT>() as u32).is_err() {
            let _ = GetWindowRect(h, &mut r);
        }
        let r = Rect::from_win(&r);
        if r.w() > 4 && r.h() > 4 {
            v.push(r);
        }
        TRUE
    }
    let mut data = (Vec::new(), skip_owner_pid);
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut data as *mut _ as isize));
    }
    data.0
}

pub fn monitors() -> Vec<Rect> {
    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        let v = &mut *(lp.0 as *mut Vec<Rect>);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(m, &mut mi);
        v.push(Rect::from_win(&mi.rcMonitor));
        TRUE
    }
    let mut v: Vec<Rect> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

// ---------- GDI helpers ----------

unsafe fn dib(w: i32, h: i32, src: Option<&[u8]>, mul: f32) -> (HDC, HBITMAP, HGDIOBJ) {
    let screen = GetDC(None);
    let dc = CreateCompatibleDC(Some(screen));
    ReleaseDC(None, screen);
    let bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, ..Default::default() },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let bmp = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
    if let (Some(src), false) = (src, bits.is_null()) {
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
        if mul >= 0.999 {
            dst.copy_from_slice(src);
        } else {
            let m = (mul * 256.0) as u32;
            for (d, s) in dst.iter_mut().zip(src.iter()) {
                *d = ((*s as u32 * m) >> 8) as u8;
            }
        }
    }
    let old = SelectObject(dc, bmp.into());
    (dc, bmp, old)
}

unsafe fn free(d: (HDC, HBITMAP, HGDIOBJ)) {
    SelectObject(d.0, d.2);
    let _ = DeleteObject(d.1.into());
    let _ = DeleteDC(d.0);
}

unsafe fn icon_font(scale: f32, px: f32) -> HFONT {
    let f = wide("Segoe Fluent Icons");
    CreateFontW(-((px * scale) as i32), 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, PCWSTR(f.as_ptr()))
}

fn glyph(b: Btn, audio: bool) -> u16 {
    match b {
        Btn::Rect => 0xF407,
        Btn::Window => 0xE737,
        Btn::Screen => 0xE7F4,
        Btn::Still => 0xE722,
        Btn::Video => 0xE714,
        Btn::Audio => {
            if audio {
                0xE767
            } else {
                0xE74F
            }
        }
        Btn::Close => 0xE711,
    }
}

fn selected(b: Btn, o: &Opts) -> bool {
    match b {
        Btn::Rect => o.shape == Shape::Rect,
        Btn::Window => o.shape == Shape::Window,
        Btn::Screen => o.shape == Shape::Screen,
        Btn::Still => !o.video,
        Btn::Video => o.video,
        _ => false,
    }
}

/// Draws the toolbar with its top-left at (x, y) in `dc`.
pub unsafe fn draw_toolbar(dc: HDC, x: i32, y: i32, scale: f32, o: &Opts, hot: Option<Btn>) {
    let (btns, tw) = layout(o.video, o.still_only);
    let (w, h) = (s(tw, scale), s(bar_height(), scale));
    let bg = CreateSolidBrush(COLORREF(0x002B2B2B));
    let pen = CreatePen(PS_SOLID, 1, COLORREF(0x00454545));
    let ob = SelectObject(dc, bg.into());
    let op = SelectObject(dc, pen.into());
    let rad = s(16, scale);
    let _ = RoundRect(dc, x, y, x + w, y + h, rad, rad);
    SelectObject(dc, ob);
    SelectObject(dc, op);
    let _ = DeleteObject(bg.into());
    let _ = DeleteObject(pen.into());
    let f = icon_font(scale, 17.0);
    let of = SelectObject(dc, f.into());
    SetBkMode(dc, TRANSPARENT);
    let mut prev_x: Option<i32> = None;
    for (b, bx) in &btns {
        // separator when there is a gap from the previous button
        if let Some(px) = prev_x {
            if *bx - px > BTN {
                let sx = x + s(px + BTN + SEP / 2, scale);
                let sep = CreateSolidBrush(COLORREF(0x00505050));
                FillRect(dc, &RECT { left: sx, top: y + s(PAD + 8, scale), right: sx + 1.max(s(1, scale)), bottom: y + s(PAD + BTN - 8, scale) }, sep);
                let _ = DeleteObject(sep.into());
            }
        }
        prev_x = Some(*bx);
        let r = RECT { left: x + s(*bx, scale), top: y + s(PAD, scale), right: x + s(*bx + BTN, scale), bottom: y + s(PAD + BTN, scale) };
        let sel = selected(*b, o);
        if sel || hot == Some(*b) {
            let br = CreateSolidBrush(if sel { COLORREF(0x00484848) } else { COLORREF(0x00383838) });
            let rr = CreateRoundRectRgn(r.left + 2, r.top + 2, r.right - 1, r.bottom - 1, s(8, scale), s(8, scale));
            let _ = FillRgn(dc, rr, br);
            let _ = DeleteObject(rr.into());
            let _ = DeleteObject(br.into());
        }
        SetTextColor(dc, if sel { COLORREF(0x00FFC86E) } else { COLORREF(0x00EAEAEA) });
        let mut g = [glyph(*b, o.audio)];
        let mut rr = r;
        DrawTextW(dc, &mut g, &mut rr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }
    SelectObject(dc, of);
    let _ = DeleteObject(f.into());
}

pub fn hit_toolbar(px: i32, py: i32, x: i32, y: i32, scale: f32, o: &Opts) -> Option<Btn> {
    let (btns, _) = layout(o.video, o.still_only);
    for (b, bx) in btns {
        let r = Rect::new(x + s(bx, scale), y + s(PAD, scale), x + s(bx + BTN, scale), y + s(PAD + BTN, scale));
        if r.contains(px, py) {
            return Some(b);
        }
    }
    None
}

fn toolbar_rect(ov: &Ovl) -> Rect {
    let (_, tw) = layout(ov.o.video, ov.o.still_only);
    Rect::new(ov.bar_xy.0, ov.bar_xy.1, ov.bar_xy.0 + s(tw, ov.scale), ov.bar_xy.1 + s(bar_height(), ov.scale))
}

// ---------- open / close ----------

pub fn register() {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSW { lpfnWndProc: Some(proc_), hInstance: hinst.into(), lpszClassName: CLASS, hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(), ..Default::default() };
        RegisterClassW(&wc);
    }
}

pub fn open(owner: HWND, shot: Shot, o: Opts) {
    if is_open() {
        return;
    }
    unsafe {
        let vs = shot.rect;
        let wins = top_windows(windows::Win32::System::Threading::GetCurrentProcessId());
        let mons = monitors();
        // toolbar on the monitor under the cursor
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = mons.iter().copied().find(|m| m.contains(pt.x, pt.y)).unwrap_or(vs);
        let hm = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let scale = crate::scale_for(hm);
        let (_, tw) = layout(o.video, o.still_only);
        let bar_xy = ((mon.l + mon.r) / 2 - s(tw, scale) / 2 - vs.l, mon.t + s(12, scale) - vs.t);

        let (w, h) = (vs.w(), vs.h());
        let bright = dib(w, h, Some(&shot.bgra), 1.0);
        let dim = dib(w, h, Some(&shot.bgra), 0.55);
        let back = dib(w, h, None, 1.0);
        let ov = Ovl {
            owner,
            hwnd: HWND::default(),
            vs,
            shot: Some(shot),
            bright,
            dim,
            back,
            o,
            drag: None,
            sel: None,
            hover: None,
            wins,
            mons,
            bar_xy,
            scale,
            hot: None,
            result: None,
        };
        OVL.with(|c| *c.borrow_mut() = Some(ov));
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let hwnd = CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW, CLASS, w!("kirinuki"), WS_POPUP, vs.l, vs.t, w, h, None, None, Some(hinst.into()), None).unwrap_or_default();
        OVL.with(|c| {
            if let Some(ov) = c.borrow_mut().as_mut() {
                ov.hwnd = hwnd;
            }
        });
        crate::hook::set_overlay(true);
        let _ = ShowWindow(hwnd, SW_SHOW);
        bring_front(hwnd);
        let _ = SetFocus(Some(hwnd));
        // something may still come on top (an old-style taskbar re-raises itself over a non-active
        // topmost window and would then take clicks that start on it): keep checking
        let _ = SetTimer(Some(hwnd), RAISE_TIMER, 200, None);
        // start with the hover state for the current pointer
        let _ = PostMessageW(Some(hwnd), WM_MOUSEMOVE, WPARAM(0), LPARAM((((pt.y - vs.t) as u32 as isize) << 16) | ((pt.x - vs.l) as u16 as isize)));
    }
}

const RAISE_TIMER: usize = 1;

thread_local! {
    static LAST_ABOVE: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

/// Makes the overlay the active window. A plain SetForegroundWindow can be refused (we are a
/// background process reacting to a hook), so borrow the foreground thread's input state if needed.
unsafe fn bring_front(hwnd: HWND) {
    let _ = SetForegroundWindow(hwnd);
    if GetForegroundWindow() == hwnd {
        return;
    }
    let fg = GetForegroundWindow();
    let fg_tid = GetWindowThreadProcessId(fg, None);
    let me = windows::Win32::System::Threading::GetCurrentThreadId();
    let attached = fg_tid != 0 && fg_tid != me && windows::Win32::System::Threading::AttachThreadInput(me, fg_tid, true).as_bool();
    let _ = BringWindowToTop(hwnd);
    let _ = SetForegroundWindow(hwnd);
    if attached {
        let _ = windows::Win32::System::Threading::AttachThreadInput(me, fg_tid, false);
    }
    let ok = GetForegroundWindow() == hwnd;
    crate::logf!("overlay: foreground {}", if ok { "ok (attached)" } else { "refused" });
}

/// First visible window above ours (None = we are on top).
unsafe fn window_above(hwnd: HWND) -> Option<HWND> {
    let mut h = GetWindow(hwnd, GW_HWNDPREV).ok()?;
    loop {
        if IsWindowVisible(h).as_bool() {
            return Some(h);
        }
        h = GetWindow(h, GW_HWNDPREV).ok()?;
    }
}

fn class_of(h: HWND) -> String {
    let mut b = [0u16; 128];
    let n = unsafe { GetClassNameW(h, &mut b) };
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}

/// Destroys the overlay; returns the pick (if any) and the frozen shot.
pub fn close() -> (Option<Pick>, Option<Shot>) {
    crate::hook::set_overlay(false);
    let ov = OVL.with(|c| c.borrow_mut().take());
    let Some(mut ov) = ov else { return (None, None) };
    unsafe {
        let _ = KillTimer(Some(ov.hwnd), RAISE_TIMER);
        let _ = DestroyWindow(ov.hwnd);
        free(ov.bright);
        free(ov.dim);
        free(ov.back);
    }
    (ov.result.take(), ov.shot.take())
}

/// Current options (to remember the user's last choice).
pub fn current_opts() -> Option<(Shape, bool, bool)> {
    OVL.with(|c| c.borrow().as_ref().map(|o| (o.o.shape, o.o.video, o.o.audio)))
}

pub fn cancel() {
    OVL.with(|c| {
        if let Some(ov) = c.borrow().as_ref() {
            unsafe {
                let _ = PostMessageW(Some(ov.owner), WM_OVL_DONE, WPARAM(0), LPARAM(0));
            }
        }
    });
}

// ---------- painting ----------

fn highlight(ov: &Ovl) -> Option<Rect> {
    ov.sel.or(ov.hover)
}

unsafe fn paint(ov: &Ovl, hdc: HDC, rc: RECT) {
    let (b, d, k) = (ov.bright.0, ov.dim.0, ov.back.0);
    let (x, y, w, h) = (rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top);
    let _ = BitBlt(k, x, y, w, h, Some(d), x, y, SRCCOPY);
    if let Some(hl) = highlight(ov) {
        let c = Rect::new(hl.l - ov.vs.l, hl.t - ov.vs.t, hl.r - ov.vs.l, hl.b - ov.vs.t);
        let i = c.intersect(&Rect::from_win(&rc));
        if !i.is_empty() {
            let _ = BitBlt(k, i.l, i.t, i.w(), i.h(), Some(b), i.l, i.t, SRCCOPY);
        }
        let pen = CreatePen(PS_SOLID, s(2, ov.scale).max(1), COLORREF(0x00FFFFFF));
        let op = SelectObject(k, pen.into());
        let ob = SelectObject(k, GetStockObject(NULL_BRUSH));
        let _ = Rectangle(k, c.l, c.t, c.r, c.b);
        SelectObject(k, ob);
        SelectObject(k, op);
        let _ = DeleteObject(pen.into());
    }
    let tb = toolbar_rect(ov);
    if !tb.intersect(&Rect::from_win(&rc)).is_empty() {
        draw_toolbar(k, tb.l, tb.t, ov.scale, &ov.o, ov.hot);
    }
    let _ = BitBlt(hdc, x, y, w, h, Some(k), x, y, SRCCOPY);
}

unsafe fn invalidate_screen_rect(hwnd: HWND, vs: Rect, r: Option<Rect>, pad: i32) {
    if let Some(r) = r {
        let c = Rect::new(r.l - vs.l - pad, r.t - vs.t - pad, r.r - vs.l + pad, r.b - vs.t + pad).to_win();
        let _ = InvalidateRect(Some(hwnd), Some(&c), false);
    }
}

// ---------- input ----------

enum Action {
    None,
    Done,
    Cancel,
}

fn on_button(ov: &mut Ovl, b: Btn) -> Action {
    match b {
        Btn::Rect => ov.o.shape = Shape::Rect,
        Btn::Window => ov.o.shape = Shape::Window,
        Btn::Screen => ov.o.shape = Shape::Screen,
        Btn::Still => ov.o.video = false,
        Btn::Video => ov.o.video = true,
        Btn::Audio => ov.o.audio = !ov.o.audio,
        Btn::Close => return Action::Cancel,
    }
    ov.sel = None;
    ov.hover = None;
    Action::None
}

fn finish(ov: &mut Ovl, r: Rect) -> Action {
    let r = r.intersect(&ov.vs);
    if r.is_empty() {
        return Action::None;
    }
    ov.result = Some(Pick { rect: r, video: ov.o.video && !ov.o.still_only, audio: ov.o.audio });
    Action::Done
}

fn hover_for(ov: &Ovl, sx: i32, sy: i32) -> Option<Rect> {
    match ov.o.shape {
        Shape::Rect => None,
        Shape::Window => ov.wins.iter().copied().find(|r| r.contains(sx, sy)).map(|r| r.intersect(&ov.vs)),
        Shape::Screen => ov.mons.iter().copied().find(|r| r.contains(sx, sy)),
    }
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let xy = (((lp.0 & 0xFFFF) as i16) as i32, (((lp.0 >> 16) & 0xFFFF) as i16) as i32);
    let r = OVL.with(|c| {
        let mut g = match c.try_borrow_mut() {
            Ok(g) => g,
            Err(_) => return None,
        };
        let ov = g.as_mut()?;
        if ov.hwnd != hwnd && !ov.hwnd.is_invalid() {
            return None;
        }
        let tb = toolbar_rect(ov);
        let mut action = Action::None;
        let handled: Option<LRESULT> = match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint(ov, hdc, ps.rcPaint);
                let _ = EndPaint(hwnd, &ps);
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_TIMER if wp.0 == RAISE_TIMER => {
                if let Some(above) = window_above(hwnd) {
                    if LAST_ABOVE.with(|c| c.replace(above.0 as isize)) != above.0 as isize {
                        crate::logf!("overlay: {} came on top, raising", class_of(above));
                    }
                    let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                    if GetForegroundWindow() != hwnd {
                        bring_front(hwnd);
                    }
                }
                Some(LRESULT(0))
            }
            WM_SETCURSOR => {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let over_bar = tb.contains(pt.x - ov.vs.l, pt.y - ov.vs.t);
                let cur = if over_bar || ov.o.shape != Shape::Rect { IDC_ARROW } else { IDC_CROSS };
                SetCursor(LoadCursorW(None, cur).ok());
                Some(LRESULT(1))
            }
            WM_MOUSEMOVE => {
                let (sx, sy) = (xy.0 + ov.vs.l, xy.1 + ov.vs.t);
                let hot = if ov.drag.is_none() { hit_toolbar(xy.0, xy.1, tb.l, tb.t, ov.scale, &ov.o) } else { None };
                if hot != ov.hot {
                    ov.hot = hot;
                    let _ = InvalidateRect(Some(hwnd), Some(&tb.to_win()), false);
                }
                let old = highlight(ov);
                if let Some((x0, y0)) = ov.drag {
                    ov.sel = Some(Rect::from_points(x0, y0, sx, sy));
                } else if tb.contains(xy.0, xy.1) {
                    ov.hover = None;
                } else {
                    ov.hover = hover_for(ov, sx, sy);
                }
                let new = highlight(ov);
                if old != new {
                    let pad = s(4, ov.scale);
                    invalidate_screen_rect(hwnd, ov.vs, old, pad);
                    invalidate_screen_rect(hwnd, ov.vs, new, pad);
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                let (sx, sy) = (xy.0 + ov.vs.l, xy.1 + ov.vs.t);
                crate::logf!("overlay: down {sx},{sy}");
                if let Some(b) = hit_toolbar(xy.0, xy.1, tb.l, tb.t, ov.scale, &ov.o) {
                    action = on_button(ov, b);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                } else if tb.contains(xy.0, xy.1) {
                } else {
                    match ov.o.shape {
                        Shape::Rect => {
                            ov.drag = Some((sx, sy));
                            ov.sel = Some(Rect::new(sx, sy, sx, sy));
                            SetCapture(hwnd);
                        }
                        _ => {
                            if let Some(h) = hover_for(ov, sx, sy) {
                                action = finish(ov, h);
                            }
                        }
                    }
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                if let Some((x0, y0)) = ov.drag.take() {
                    let _ = ReleaseCapture();
                    let r = Rect::from_points(x0, y0, xy.0 + ov.vs.l, xy.1 + ov.vs.t);
                    if r.w() >= 4 && r.h() >= 4 {
                        action = finish(ov, r);
                    } else {
                        let old = ov.sel.take();
                        invalidate_screen_rect(hwnd, ov.vs, old, s(4, ov.scale));
                    }
                }
                Some(LRESULT(0))
            }
            WM_RBUTTONUP => {
                action = Action::Cancel;
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                if wp.0 as u16 == VK_ESCAPE.0 {
                    action = Action::Cancel;
                }
                Some(LRESULT(0))
            }
            _ => None,
        };
        match action {
            Action::Done => {
                let _ = PostMessageW(Some(ov.owner), WM_OVL_DONE, WPARAM(1), LPARAM(0));
            }
            Action::Cancel => {
                let _ = PostMessageW(Some(ov.owner), WM_OVL_DONE, WPARAM(0), LPARAM(0));
            }
            Action::None => {}
        }
        handled
    });
    r.unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp))
}

/// Offscreen render of the overlay over a synthetic screen (for checking the look without showing it).
pub unsafe fn render_preview(shot: &Shot, o: Opts, hl: Option<Rect>, hot: Option<Btn>, scale: f32) -> Vec<u8> {
    let vs = shot.rect;
    let (w, h) = (vs.w(), vs.h());
    let (_, tw) = layout(o.video, o.still_only);
    let ov = Ovl {
        owner: HWND::default(),
        hwnd: HWND::default(),
        vs,
        shot: None,
        bright: dib(w, h, Some(&shot.bgra), 1.0),
        dim: dib(w, h, Some(&shot.bgra), 0.55),
        back: dib(w, h, None, 1.0),
        o,
        drag: None,
        sel: hl,
        hover: None,
        wins: vec![],
        mons: vec![],
        bar_xy: (w / 2 - s(tw, scale) / 2, s(12, scale)),
        scale,
        hot,
        result: None,
    };
    let out = crate::render_offscreen(w, h, |dc| paint(&ov, dc, RECT { left: 0, top: 0, right: w, bottom: h }));
    free(ov.bright);
    free(ov.dim);
    free(ov.back);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_groups() {
        let (b, w) = layout(false, false);
        assert_eq!(b.iter().map(|x| x.0).collect::<Vec<_>>(), vec![Btn::Rect, Btn::Window, Btn::Screen, Btn::Still, Btn::Video, Btn::Close]);
        assert_eq!(w, PAD + BTN * 6 + SEP * 2 + PAD);
        let (b, _) = layout(true, false);
        assert!(b.iter().any(|x| x.0 == Btn::Audio));
        let (b, _) = layout(true, true);
        assert!(!b.iter().any(|x| matches!(x.0, Btn::Video | Btn::Still | Btn::Audio)));
    }

    #[test]
    fn toolbar_hits() {
        let o = Opts { shape: Shape::Rect, video: false, audio: true, still_only: false };
        let (b, _) = layout(false, false);
        for (btn, bx) in b {
            let x = 100 + s(bx + BTN / 2, 1.5);
            let y = 50 + s(PAD + BTN / 2, 1.5);
            assert_eq!(hit_toolbar(x, y, 100, 50, 1.5, &o), Some(btn));
        }
        assert_eq!(hit_toolbar(99, 60, 100, 50, 1.5, &o), None);
    }
}
