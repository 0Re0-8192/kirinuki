//! Start at sign-in: a value under HKCU\...\Run pointing at this exe.
use crate::util::wide;
use windows::core::*;
use windows::Win32::System::Registry::*;

const KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const NAME: PCWSTR = w!("kirinuki");

fn command() -> Option<String> {
    std::env::current_exe().ok().map(|p| format!("\"{}\"", p.display()))
}

/// True when the Run value exists and points at this exe.
pub fn enabled() -> bool {
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, KEY, NAME, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut size)) };
    if r.is_err() {
        return false;
    }
    let n = (size as usize / 2).saturating_sub(1).min(buf.len());
    let v = String::from_utf16_lossy(&buf[..n]);
    command().map(|c| v.eq_ignore_ascii_case(&c)).unwrap_or(false)
}

pub fn set(on: bool) -> bool {
    unsafe {
        if on {
            let Some(c) = command() else { return false };
            let w = wide(&c);
            RegSetKeyValueW(HKEY_CURRENT_USER, KEY, NAME, REG_SZ.0, Some(w.as_ptr().cast()), (w.len() * 2) as u32).is_ok()
        } else {
            let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, KEY, NAME);
            r.is_ok() || r == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND
        }
    }
}
