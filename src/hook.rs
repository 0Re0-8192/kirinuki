//! Low-level keyboard hook that takes Win+Shift+S / Win+Shift+R before the shell sees them.
//! The callback only posts a message and returns (a slow hook gets removed by Windows).
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_HK_SNIP: u32 = WM_APP + 20;
pub const WM_HK_REC: u32 = WM_APP + 21;

pub const WM_HK_ESC: u32 = WM_APP + 22;

static TARGET: AtomicIsize = AtomicIsize::new(0);
static OVERLAY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_overlay(open: bool) {
    OVERLAY.store(open, Ordering::SeqCst);
}
// bit0 lwin, bit1 rwin, bit2 shift(any), bit3 ctrl, bit4 alt, bit5 "we ate a key while win was down", bit6 eating S, bit7 eating R
static STATE: AtomicU32 = AtomicU32::new(0);
const MASK_KEY: u16 = 0xE8; // unassigned; tells the shell "another key was pressed" so Start doesn't open

fn set(bit: u32, on: bool) {
    if on {
        STATE.fetch_or(bit, Ordering::SeqCst);
    } else {
        STATE.fetch_and(!bit, Ordering::SeqCst);
    }
}

unsafe fn send_mask_key() {
    let mk = |up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(MASK_KEY), wScan: 0, dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) }, time: 0, dwExtraInfo: 0 },
        },
    };
    let inputs = [mk(false), mk(true)];
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

/// Pure decision: given the state bits and a key event, returns (swallow, message to post, new state).
/// `real_mods`: modifier bits (1|2|4|8|16) read from the live keyboard state on S/R key-down,
/// because tracked bits go stale when a key-up happens where the hook can't see it (lock screen, elevated window).
pub fn decide(state: u32, vk: u32, up: bool, real_mods: Option<u32>) -> (bool, Option<u32>, u32) {
    let mut s = state;
    let bit = |b: u32, on: bool, s: &mut u32| if on { *s |= b } else { *s &= !b };
    match vk {
        0x5B => bit(1, !up, &mut s),
        0x5C => bit(2, !up, &mut s),
        0x10 | 0xA0 | 0xA1 => bit(4, !up, &mut s),
        0x11 | 0xA2 | 0xA3 => bit(8, !up, &mut s),
        0x12 | 0xA4 | 0xA5 => bit(16, !up, &mut s),
        _ => {}
    }
    if let Some(m) = real_mods {
        s = (s & !31) | (m & 31);
    }
    let win = s & 3 != 0;
    let combo = win && s & 4 != 0 && s & 8 == 0 && s & 16 == 0;
    for (key, eat_bit, msg) in [(0x53u32, 64u32, WM_HK_SNIP), (0x52, 128, WM_HK_REC)] {
        if vk != key {
            continue;
        }
        if !up && s & eat_bit != 0 {
            // auto-repeat of a key we already took (even if Win/Shift were let go meanwhile)
            return (true, None, s);
        }
        if !up && combo {
            s |= eat_bit | 32;
            return (true, Some(msg), s);
        }
        if up && s & eat_bit != 0 {
            s &= !eat_bit;
            return (true, None, s);
        }
    }
    (false, None, s)
}

unsafe fn live_mods() -> u32 {
    let down = |vk: u16| GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0;
    (down(0x5B) as u32) | (down(0x5C) as u32) << 1 | (down(0x10) as u32) << 2 | (down(0x11) as u32) << 3 | (down(0x12) as u32) << 4
}

unsafe extern "system" fn proc_(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        let up = wp.0 as u32 == WM_KEYUP || wp.0 as u32 == WM_SYSKEYUP;
        // Esc while the overlay is up (the overlay may not have keyboard focus)
        if k.vkCode == 0x1B && OVERLAY.load(Ordering::SeqCst) {
            if !up {
                let _ = PostMessageW(Some(HWND(TARGET.load(Ordering::SeqCst) as *mut _)), WM_HK_ESC, WPARAM(0), LPARAM(0));
            }
            return LRESULT(1);
        }
        if k.vkCode != MASK_KEY as u32 {
            let before = STATE.load(Ordering::SeqCst);
            // Win released after we ate a key: send the mask key first so Start doesn't pop up
            if up && (k.vkCode == 0x5B || k.vkCode == 0x5C) && before & 32 != 0 {
                send_mask_key();
                set(32, false);
            }
            let real = if !up && (k.vkCode == 0x53 || k.vkCode == 0x52) {
                // "eating" bit left over from a key-up we never saw: only a key that is still down repeats
                let eat_bit = if k.vkCode == 0x53 { 64 } else { 128 };
                if GetAsyncKeyState(k.vkCode as i32) as u16 & 0x8000 == 0 {
                    set(eat_bit, false);
                }
                Some(live_mods())
            } else {
                None
            };
            let (swallow, msg, after) = decide(STATE.load(Ordering::SeqCst), k.vkCode, up, real);
            STATE.store(after, Ordering::SeqCst);
            if let Some(m) = msg {
                let _ = PostMessageW(Some(HWND(TARGET.load(Ordering::SeqCst) as *mut _)), m, WPARAM(0), LPARAM(0));
            }
            if swallow {
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);

/// Installs the hook on its own thread (so a busy UI thread can't delay keys). Messages go to `target`.
/// Windows silently drops a hook that ever answers too slowly, so it is re-installed every minute.
pub fn install(target: HWND) {
    TARGET.store(target.0 as isize, Ordering::SeqCst);
    std::thread::spawn(|| unsafe {
        use windows::Win32::System::Threading::*;
        HOOK_THREAD.store(GetCurrentThreadId(), Ordering::SeqCst);
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let set = || SetWindowsHookExW(WH_KEYBOARD_LL, Some(proc_), Some(hinst.into()), 0);
        let mut h = set();
        match &h {
            Ok(_) => crate::logf!("hook: installed"),
            Err(e) => crate::logf!("hook: failed {e}"),
        }
        SetTimer(None, 0, 60_000, None);
        let mut m = MSG::default();
        while GetMessageW(&mut m, None, 0, 0).as_bool() {
            if m.message == WM_TIMER {
                // new one first, so there is no moment without a hook
                match set() {
                    Ok(new) => {
                        if let Ok(old) = h {
                            let _ = UnhookWindowsHookEx(old);
                        }
                        h = Ok(new);
                    }
                    Err(e) => crate::logf!("hook: reinstall failed {e}"),
                }
            }
        }
        if let Ok(h) = h {
            let _ = UnhookWindowsHookEx(h);
        }
    });
}

pub fn uninstall() {
    let t = HOOK_THREAD.load(Ordering::SeqCst);
    if t != 0 {
        unsafe {
            let _ = PostThreadMessageW(t, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(seq: &[(u32, bool)]) -> (Vec<bool>, Vec<u32>) {
        let mut s = 0;
        let mut sw = vec![];
        let mut msgs = vec![];
        for &(vk, up) in seq {
            let (a, m, n) = decide(s, vk, up, None);
            s = n;
            sw.push(a);
            if let Some(m) = m {
                msgs.push(m);
            }
        }
        (sw, msgs)
    }

    #[test]
    fn win_shift_s_is_taken() {
        let (sw, msgs) = run(&[(0x5B, false), (0xA0, false), (0x53, false), (0x53, true), (0xA0, true), (0x5B, true)]);
        assert_eq!(sw, vec![false, false, true, true, false, false]);
        assert_eq!(msgs, vec![WM_HK_SNIP]);
    }

    #[test]
    fn autorepeat_posts_once() {
        let (_, msgs) = run(&[(0x5C, false), (0xA1, false), (0x53, false), (0x53, false), (0x53, false), (0x53, true)]);
        assert_eq!(msgs, vec![WM_HK_SNIP]);
    }

    #[test]
    fn other_combos_pass() {
        // plain S, Shift+S, Win+S, Win+Ctrl+Shift+S
        let (sw, msgs) = run(&[(0x53, false), (0x53, true), (0xA0, false), (0x53, false), (0x53, true), (0xA0, true)]);
        assert!(sw.iter().all(|s| !s));
        assert!(msgs.is_empty());
        let (sw, msgs) = run(&[(0x5B, false), (0x53, false), (0x53, true), (0x5B, true)]);
        assert!(sw.iter().all(|s| !s));
        assert!(msgs.is_empty());
        let (_, msgs) = run(&[(0x5B, false), (0xA2, false), (0xA0, false), (0x53, false)]);
        assert!(msgs.is_empty());
    }

    #[test]
    fn stale_win_is_corrected_by_live_state() {
        // Win key-up was never seen (lock screen): tracked state says Win is down
        let s = 1;
        let (a, m, _) = decide(s | 4, 0x53, false, Some(4)); // live: only Shift
        assert!(!a);
        assert!(m.is_none());
        // and the reverse: stale Ctrl doesn't block a real Win+Shift+S
        let (a, m, _) = decide(8, 0x53, false, Some(1 | 4));
        assert!(a);
        assert_eq!(m, Some(WM_HK_SNIP));
    }

    #[test]
    fn repeat_after_win_released_still_eaten() {
        let (sw, msgs) = run(&[(0x5B, false), (0xA0, false), (0x53, false), (0x5B, true), (0x53, false), (0x53, false), (0x53, true)]);
        assert_eq!(sw, vec![false, false, true, false, true, true, true]);
        assert_eq!(msgs, vec![WM_HK_SNIP]);
    }

    #[test]
    fn win_shift_r() {
        let (_, msgs) = run(&[(0x5B, false), (0xA0, false), (0x52, false), (0x52, true)]);
        assert_eq!(msgs, vec![WM_HK_REC]);
    }

    #[test]
    fn s_released_after_shift_still_eaten() {
        let (sw, _) = run(&[(0x5B, false), (0xA0, false), (0x53, false), (0xA0, true), (0x5B, true), (0x53, true)]);
        assert_eq!(sw, vec![false, false, true, false, false, true]);
    }
}
