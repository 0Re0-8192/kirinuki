//! Frozen screenshot of the whole virtual screen, cropping, PNG, clipboard.
use crate::util::Rect;
use std::path::{Path, PathBuf};
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::CF_DIB;
use windows::Win32::UI::Shell::DROPFILES;
use windows::Win32::UI::WindowsAndMessaging::*;

/// BGRA, top-down, in physical virtual-screen coordinates.
pub struct Shot {
    pub rect: Rect,
    pub bgra: Vec<u8>,
}

pub fn virtual_screen() -> Rect {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        Rect::new(x, y, x + GetSystemMetrics(SM_CXVIRTUALSCREEN), y + GetSystemMetrics(SM_CYVIRTUALSCREEN))
    }
}

pub fn grab_screen() -> Option<Shot> {
    let r = virtual_screen();
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: r.w(), biHeight: -r.h(), biPlanes: 1, biBitCount: 32, ..Default::default() },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            return None;
        };
        let old = SelectObject(dc, bmp.into());
        let ok = BitBlt(dc, 0, 0, r.w(), r.h(), Some(screen), r.l, r.t, SRCCOPY | CAPTUREBLT).is_ok();
        let _ = GdiFlush();
        let bgra = std::slice::from_raw_parts(bits as *const u8, (r.w() * r.h() * 4) as usize).to_vec();
        SelectObject(dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        ok.then_some(Shot { rect: r, bgra })
    }
}

impl Shot {
    /// Crop `area` (screen coordinates); returns (w, h, BGRA).
    pub fn crop(&self, area: &Rect) -> Option<(u32, u32, Vec<u8>)> {
        let a = area.intersect(&self.rect);
        if a.is_empty() {
            return None;
        }
        let (w, h) = (a.w() as usize, a.h() as usize);
        let stride = self.rect.w() as usize * 4;
        let mut out = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            let o = (a.t - self.rect.t) as usize + y;
            let s = o * stride + (a.l - self.rect.l) as usize * 4;
            out.extend_from_slice(&self.bgra[s..s + w * 4]);
        }
        Some((w as u32, h as u32, out))
    }
}

pub fn encode_png(w: u32, h: u32, bgra: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for p in bgra.chunks_exact(4) {
        rgb.extend_from_slice(&[p[2], p[1], p[0]]);
    }
    let mut out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut out, w, h);
        e.set_color(png::ColorType::Rgb);
        e.set_compression(png::Compression::Fast);
        let mut wr = e.write_header().unwrap();
        wr.write_image_data(&rgb).unwrap();
    }
    out
}

/// 24-bit bottom-up DIB (what every app understands as CF_DIB).
pub fn dib24(w: u32, h: u32, bgra: &[u8]) -> Vec<u8> {
    let row = ((w * 3 + 3) & !3) as usize;
    let hdr = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w as i32,
        biHeight: h as i32,
        biPlanes: 1,
        biBitCount: 24,
        biCompression: BI_RGB.0,
        biSizeImage: (row * h as usize) as u32,
        ..Default::default()
    };
    let mut out = vec![0u8; std::mem::size_of::<BITMAPINFOHEADER>() + row * h as usize];
    unsafe {
        std::ptr::copy_nonoverlapping(&hdr as *const _ as *const u8, out.as_mut_ptr(), std::mem::size_of::<BITMAPINFOHEADER>());
    }
    let base = std::mem::size_of::<BITMAPINFOHEADER>();
    for y in 0..h as usize {
        let dst = base + (h as usize - 1 - y) * row;
        let src = y * w as usize * 4;
        for x in 0..w as usize {
            out[dst + x * 3..dst + x * 3 + 3].copy_from_slice(&bgra[src + x * 4..src + x * 4 + 3]);
        }
    }
    out
}

unsafe fn set_clip(fmt: u32, bytes: &[u8]) -> bool {
    let Some(g) = global(bytes) else { return false };
    if SetClipboardData(fmt, Some(HANDLE(g.0))).is_ok() {
        true
    } else {
        let _ = GlobalFree(Some(g));
        false
    }
}

unsafe fn global(bytes: &[u8]) -> Option<HGLOBAL> {
    let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
    let p = GlobalLock(h);
    if p.is_null() {
        let _ = GlobalFree(Some(h));
        return None;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
    let _ = GlobalUnlock(h);
    Some(h)
}

unsafe fn open_clipboard(owner: HWND) -> bool {
    // tests must not clobber the user's clipboard
    if std::env::var_os("KIRINUKI_NO_CLIPBOARD").is_some() {
        return false;
    }
    for _ in 0..10 {
        if OpenClipboard(Some(owner)).is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

/// Image to the clipboard as CF_DIB and "PNG".
pub fn copy_image(owner: HWND, w: u32, h: u32, bgra: &[u8], png: &[u8]) -> bool {
    unsafe {
        if !open_clipboard(owner) {
            return false;
        }
        let _ = EmptyClipboard();
        let ok = set_clip(CF_DIB.0 as u32, &dib24(w, h, bgra));
        let fmt = RegisterClipboardFormatW(w!("PNG"));
        if fmt != 0 {
            set_clip(fmt, png);
        }
        let _ = CloseClipboard();
        ok
    }
}

/// File to the clipboard (paste into Explorer, chat apps...).
pub fn copy_file(owner: HWND, path: &Path) -> bool {
    unsafe {
        let mut data = vec![0u8; std::mem::size_of::<DROPFILES>()];
        let df = DROPFILES { pFiles: std::mem::size_of::<DROPFILES>() as u32, fWide: true.into(), ..Default::default() };
        std::ptr::copy_nonoverlapping(&df as *const _ as *const u8, data.as_mut_ptr(), data.len());
        for u in path.as_os_str().to_string_lossy().encode_utf16().chain([0u16, 0u16]) {
            data.extend_from_slice(&u.to_le_bytes());
        }
        if !open_clipboard(owner) {
            return false;
        }
        let _ = EmptyClipboard();
        let ok = set_clip(15 /* CF_HDROP */, &data);
        let _ = CloseClipboard();
        ok
    }
}

/// save_dir\kirinuki_YYYYMMDD_HHMMSS[_n].ext
pub fn new_path(dir: &Path, ext: &str) -> PathBuf {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    let base = format!("kirinuki_{:04}{:02}{:02}_{:02}{:02}{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond);
    let mut p = dir.join(format!("{base}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{base}_{n}.{ext}"));
        n += 1;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_and_dib() {
        // 3x2 image, pixel value = index
        let mut bgra = vec![];
        for i in 0..6u8 {
            bgra.extend_from_slice(&[i, i + 10, i + 20, 255]);
        }
        let s = Shot { rect: Rect::new(-1, -1, 2, 1), bgra };
        let (w, h, c) = s.crop(&Rect::new(0, -1, 5, 1)).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(c[0], 1);
        assert_eq!(c[4 * 3], 5);
        let d = dib24(w, h, &c);
        let base = std::mem::size_of::<BITMAPINFOHEADER>();
        // bottom-up: first stored row is the last image row (pixels 4,5)
        assert_eq!(&d[base..base + 3], &[4, 14, 24]);
        assert_eq!(d.len(), base + 8 * 2);
    }

    #[test]
    fn png_roundtrip() {
        let bgra: Vec<u8> = (0..4 * 4 * 4).map(|i| i as u8).collect();
        let p = encode_png(4, 4, &bgra);
        let dec = png::Decoder::new(std::io::Cursor::new(p));
        let mut r = dec.read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size()];
        let info = r.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (4, 4));
        assert_eq!(&buf[0..3], &[2, 1, 0]);
    }
}
