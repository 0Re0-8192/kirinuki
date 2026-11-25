use std::io::Write;
use std::sync::Mutex;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);

pub fn log_init() {
    let path = std::env::temp_dir().join("kirinuki.log");
    if std::fs::metadata(&path).map(|m| m.len() > 2_000_000).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        *LOG.lock().unwrap() = Some(f);
    }
}

pub fn log(msg: &str) {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = format!("[{t}] {msg}\n");
    eprint!("{line}");
    if let Some(f) = LOG.lock().unwrap().as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! logf {
    ($($a:tt)*) => { $crate::util::log(&format!($($a)*)) };
}

/// Rect in physical screen pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub l: i32,
    pub t: i32,
    pub r: i32,
    pub b: i32,
}

impl Rect {
    pub fn new(l: i32, t: i32, r: i32, b: i32) -> Self {
        Rect { l, t, r, b }
    }
    pub fn w(&self) -> i32 {
        self.r - self.l
    }
    pub fn h(&self) -> i32 {
        self.b - self.t
    }
    pub fn from_points(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        Rect::new(x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))
    }
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }
    pub fn intersect(&self, o: &Rect) -> Rect {
        let r = Rect::new(self.l.max(o.l), self.t.max(o.t), self.r.min(o.r), self.b.min(o.b));
        if r.r <= r.l || r.b <= r.t {
            Rect::default()
        } else {
            r
        }
    }
    pub fn is_empty(&self) -> bool {
        self.w() <= 0 || self.h() <= 0
    }
    pub fn inflate(&self, d: i32) -> Rect {
        Rect::new(self.l - d, self.t - d, self.r + d, self.b + d)
    }
    pub fn to_win(&self) -> windows::Win32::Foundation::RECT {
        windows::Win32::Foundation::RECT { left: self.l, top: self.t, right: self.r, bottom: self.b }
    }
    pub fn from_win(r: &windows::Win32::Foundation::RECT) -> Rect {
        Rect::new(r.left, r.top, r.right, r.bottom)
    }
}

/// Even size, at least 2 (H.264 needs even dimensions).
pub fn even(v: i32) -> u32 {
    ((v.max(2)) & !1) as u32
}

/// Where to put the stop bar so it stays outside the recorded area.
/// `mon` is the work area of the monitor holding the region, `others` the work areas of other monitors.
/// Returns (x, y, inside) where inside=true means no free space was found and the bar overlaps the region.
pub fn place_bar(region: &Rect, mon: &Rect, others: &[Rect], bw: i32, bh: i32, gap: i32) -> (i32, i32, bool) {
    let cx = (region.l + region.r) / 2 - bw / 2;
    let clamp_x = |x: i32, m: &Rect| x.max(m.l).min(m.r - bw);
    // above
    let y = region.t - gap - bh;
    if y >= mon.t {
        return (clamp_x(cx, mon), y, false);
    }
    // below
    let y = region.b + gap;
    if y + bh <= mon.b {
        return (clamp_x(cx, mon), y, false);
    }
    // left / right
    let ym = ((region.t + region.b) / 2 - bh / 2).max(mon.t).min(mon.b - bh);
    if region.l - gap - bw >= mon.l {
        return (region.l - gap - bw, ym, false);
    }
    if region.r + gap + bw <= mon.r {
        return (region.r + gap, ym, false);
    }
    // another monitor
    if let Some(m) = others.first() {
        return ((m.l + m.r) / 2 - bw / 2, m.t + gap, false);
    }
    (clamp_x(cx, mon), (region.t + gap).max(mon.t), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_sizes() {
        assert_eq!(even(101), 100);
        assert_eq!(even(1), 2);
        assert_eq!(even(0), 2);
        assert_eq!(even(1920), 1920);
    }

    #[test]
    fn bar_goes_above_then_below() {
        let mon = Rect::new(0, 0, 1920, 1040);
        let (_, y, inside) = place_bar(&Rect::new(100, 200, 500, 400), &mon, &[], 200, 40, 8);
        assert_eq!((y, inside), (152, false));
        let (_, y, inside) = place_bar(&Rect::new(100, 10, 500, 400), &mon, &[], 200, 40, 8);
        assert_eq!((y, inside), (408, false));
    }

    #[test]
    fn bar_full_region_uses_other_monitor_or_inside() {
        let mon = Rect::new(0, 0, 1920, 1040);
        let full = Rect::new(0, 0, 1920, 1040);
        let (x, y, inside) = place_bar(&full, &mon, &[Rect::new(1920, 0, 3840, 1040)], 200, 40, 8);
        assert_eq!((x, y, inside), (2780, 8, false));
        let (_, _, inside) = place_bar(&full, &mon, &[], 200, 40, 8);
        assert!(inside);
    }

    #[test]
    fn bar_never_overlaps_region_when_outside() {
        let mon = Rect::new(0, 0, 1920, 1040);
        for &(l, t, r, b) in &[(0, 0, 1920, 900), (0, 100, 1920, 1040), (300, 0, 1920, 1040), (0, 0, 1500, 1040)] {
            let reg = Rect::new(l, t, r, b);
            let (x, y, inside) = place_bar(&reg, &mon, &[], 200, 40, 8);
            assert!(!inside, "{reg:?}");
            let bar = Rect::new(x, y, x + 200, y + 40);
            assert!(bar.intersect(&reg).is_empty(), "{reg:?} {bar:?}");
            assert!(bar.intersect(&mon) == bar, "{reg:?} {bar:?}");
        }
    }
}
