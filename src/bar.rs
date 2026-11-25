//! Stop bar (visible to every capturer, kept outside the recorded area) and region border.
use crate::record::Shared;
use crate::util::{wide, Rect};
use std::sync::Arc;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_BAR_STOP: u32 = WM_APP + 10;
pub const WM_BAR_PAUSE: u32 = WM_APP + 11;

const BAR_CLASS: PCWSTR = w!("KirinukiBar");
const BORDER_CLASS: PCWSTR = w!("KirinukiBorder");

/// Base size at 96 dpi.
pub const BAR_W: i32 = 168;
pub const BAR_H: i32 = 40;
pub const BORDER: i32 = 2;

struct BarState {
    owner: HWND,
    shared: Arc<Shared>,
    scale: f32,
    hot: i32,
}

pub struct Bar {
    pub hwnd: HWND,
    border: Option<HWND>,
}

pub fn register() {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(bar_proc),
            hInstance: hinst.into(),
            lpszClassName: BAR_CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(border_proc),
            hInstance: hinst.into(),
            lpszClassName: BORDER_CLASS,
            hbrBackground: CreateSolidBrush(COLORREF(0x003C3CE8)),
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
}

unsafe extern "system" fn border_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_NCHITTEST {
        return LRESULT(HTTRANSPARENT as isize);
    }
    DefWindowProcW(h, m, w, l)
}

impl Bar {
    /// `region`: recorded area (None for window recording: no border). `x, y`: bar position. `scale`: dpi/96.
    pub fn show(owner: HWND, shared: Arc<Shared>, x: i32, y: i32, scale: f32, region: Option<Rect>) -> Bar {
        unsafe {
            let hinst = GetModuleHandleW(None).unwrap_or_default();
            let border = region.map(|r| {
                let o = r.inflate(BORDER);
                let h = CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED,
                    BORDER_CLASS,
                    w!(""),
                    WS_POPUP,
                    o.l,
                    o.t,
                    o.w(),
                    o.h(),
                    None,
                    None,
                    Some(hinst.into()),
                    None,
                )
                .unwrap_or_default();
                let _ = SetLayeredWindowAttributes(h, COLORREF(0), 255, LWA_ALPHA);
                let outer = CreateRectRgn(0, 0, o.w(), o.h());
                let inner = CreateRectRgn(BORDER, BORDER, o.w() - BORDER, o.h() - BORDER);
                CombineRgn(Some(outer), Some(outer), Some(inner), RGN_DIFF);
                let _ = DeleteObject(inner.into());
                SetWindowRgn(h, Some(outer), false);
                let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
                h
            });
            let (bw, bh) = ((BAR_W as f32 * scale) as i32, (BAR_H as f32 * scale) as i32);
            let st = Box::new(BarState { owner, shared, scale, hot: -1 });
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                BAR_CLASS,
                w!("kirinuki"),
                WS_POPUP,
                x,
                y,
                bw,
                bh,
                None,
                None,
                Some(hinst.into()),
                Some(Box::into_raw(st) as *const _),
            )
            .unwrap_or_default();
            // No DWM rounding/shadow: the shadow would reach into the recorded area.
            // Round the corners with a window region instead.
            let pref = DWMWCP_DONOTROUND;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &pref as *const _ as *const _, 4);
            let pol = DWMNCRP_DISABLED;
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_NCRENDERING_POLICY, &pol as *const _ as *const _, 4);
            let rad = (12.0 * scale) as i32;
            SetWindowRgn(hwnd, Some(CreateRoundRectRgn(0, 0, bw + 1, bh + 1, rad, rad)), false);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetTimer(Some(hwnd), 1, 250, None);
            Bar { hwnd, border }
        }
    }

    pub fn close(self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            if let Some(b) = self.border {
                let _ = DestroyWindow(b);
            }
        }
    }

    pub fn rect(&self) -> Rect {
        let mut r = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut r);
        }
        Rect::from_win(&r)
    }
}

/// Button layout in client pixels: 0 = pause, 1 = stop.
fn buttons(scale: f32, w: i32, h: i32) -> [RECT; 2] {
    let s = (32.0 * scale) as i32;
    let pad = (4.0 * scale) as i32;
    let y = (h - s) / 2;
    let stop = RECT { left: w - pad - s, top: y, right: w - pad, bottom: y + s };
    let pause = RECT { left: stop.left - s, top: y, right: stop.left, bottom: y + s };
    [pause, stop]
}

fn hit(b: &[RECT; 2], x: i32, y: i32) -> i32 {
    for (i, r) in b.iter().enumerate() {
        if x >= r.left && x < r.right && y >= r.top && y < r.bottom {
            return i as i32;
        }
    }
    -1
}

fn fmt_time(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

unsafe fn font(scale: f32, px: f32, face: &str) -> HFONT {
    let f = wide(face);
    CreateFontW(-((px * scale) as i32), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, PCWSTR(f.as_ptr()))
}

unsafe fn paint(hwnd: HWND, st: &BarState) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right, rc.bottom);
    let mem = CreateCompatibleDC(Some(hdc));
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old = SelectObject(mem, bmp.into());
    draw(mem, st.scale, w, h, st.shared.active().as_secs(), st.shared.paused(), st.hot);
    let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
    SelectObject(mem, old);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(mem);
    let _ = EndPaint(hwnd, &ps);
}

/// Draws the bar into `dc` (also used for offscreen snapshots).
pub unsafe fn draw(dc: HDC, scale: f32, w: i32, h: i32, secs: u64, paused: bool, hot: i32) {
    let bg = CreateSolidBrush(COLORREF(0x00202020));
    FillRect(dc, &RECT { left: 0, top: 0, right: w, bottom: h }, bg);
    let _ = DeleteObject(bg.into());
    SetBkMode(dc, TRANSPARENT);
    // red dot (dimmed while paused)
    let d = (10.0 * scale) as i32;
    let x = (12.0 * scale) as i32;
    let y = (h - d) / 2;
    let red = CreateSolidBrush(if paused { COLORREF(0x00606060) } else { COLORREF(0x003C3CE8) });
    let pen = SelectObject(dc, GetStockObject(NULL_PEN));
    let ob = SelectObject(dc, red.into());
    let _ = Ellipse(dc, x, y, x + d, y + d);
    SelectObject(dc, ob);
    SelectObject(dc, pen);
    let _ = DeleteObject(red.into());
    // time
    let f = font(scale, 15.0, "Segoe UI");
    let of = SelectObject(dc, f.into());
    SetTextColor(dc, COLORREF(0x00F0F0F0));
    let mut t = wide(&fmt_time(secs));
    t.pop();
    let mut tr = RECT { left: x + d + (8.0 * scale) as i32, top: 0, right: w, bottom: h };
    DrawTextW(dc, &mut t, &mut tr, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    SelectObject(dc, of);
    let _ = DeleteObject(f.into());
    // buttons: pause/play, stop (Segoe Fluent Icons, fallback Segoe MDL2 Assets)
    let b = buttons(scale, w, h);
    let hotbr = CreateSolidBrush(COLORREF(0x00383838));
    if hot >= 0 {
        FillRect(dc, &b[hot as usize], hotbr);
    }
    let _ = DeleteObject(hotbr.into());
    let icons = font(scale, 16.0, "Segoe Fluent Icons");
    let oi = SelectObject(dc, icons.into());
    let glyphs = [if paused { 0xE768u16 } else { 0xE769 }, 0xE71A];
    for (i, g) in glyphs.iter().enumerate() {
        let mut s = [*g];
        let mut r = b[i];
        SetTextColor(dc, if i == 1 { COLORREF(0x006060FF) } else { COLORREF(0x00F0F0F0) });
        DrawTextW(dc, &mut s, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }
    SelectObject(dc, oi);
    let _ = DeleteObject(icons.into());
}

unsafe extern "system" fn bar_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lp.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut BarState;
    if p.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let st = &mut *p;
    let xy = |lp: LPARAM| ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
    let client = || {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        (rc.right, rc.bottom)
    };
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_NCHITTEST => {
            let mut pt = POINT { x: (lp.0 & 0xFFFF) as i16 as i32, y: ((lp.0 >> 16) & 0xFFFF) as i16 as i32 };
            let _ = ScreenToClient(hwnd, &mut pt);
            let (w, h) = client();
            // drag anywhere except the buttons
            return LRESULT(if hit(&buttons(st.scale, w, h), pt.x, pt.y) >= 0 { HTCLIENT } else { HTCAPTION } as isize);
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy(lp);
            let (w, h) = client();
            let hh = hit(&buttons(st.scale, w, h), x, y);
            if hh != st.hot {
                st.hot = hh;
                let _ = InvalidateRect(Some(hwnd), None, false);
                let mut tme = windows::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<windows::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT>() as u32,
                    dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::TrackMouseEvent(&mut tme);
            }
        }
        0x02A3 => {
            // WM_MOUSELEAVE
            st.hot = -1;
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        WM_LBUTTONUP => {
            let (x, y) = xy(lp);
            let (w, h) = client();
            match hit(&buttons(st.scale, w, h), x, y) {
                0 => {
                    let _ = PostMessageW(Some(st.owner), WM_BAR_PAUSE, WPARAM(0), LPARAM(0));
                }
                1 => {
                    let _ = PostMessageW(Some(st.owner), WM_BAR_STOP, WPARAM(0), LPARAM(0));
                }
                _ => {}
            }
        }
        WM_TIMER => {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        WM_WINDOWPOSCHANGED => {
            // the recorder blanks the bar wherever it overlaps the recorded area
            let mut r = RECT::default();
            let _ = GetWindowRect(hwnd, &mut r);
            st.shared.set_mask(Some(Rect::from_win(&r)));
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            paint(hwnd, st);
            return LRESULT(0);
        }
        WM_DESTROY => {
            let _ = KillTimer(Some(hwnd), 1);
            st.shared.set_mask(None);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(p));
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn time_format() {
        assert_eq!(fmt_time(0), "0:00");
        assert_eq!(fmt_time(75), "1:15");
        assert_eq!(fmt_time(3725), "1:02:05");
    }
    #[test]
    fn buttons_hit() {
        let b = buttons(1.0, BAR_W, BAR_H);
        assert_eq!(hit(&b, b[0].left + 1, b[0].top + 1), 0);
        assert_eq!(hit(&b, b[1].right - 1, b[1].bottom - 1), 1);
        assert_eq!(hit(&b, 5, 5), -1);
    }
}
