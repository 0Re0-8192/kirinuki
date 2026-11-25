//! kirinuki.ini next to the exe. Plain key=value lines.
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Rect,
    Window,
    Screen,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub save_dir: PathBuf,
    pub audio: bool,
    pub shape: Shape,
    pub video: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { save_dir: default_dir(), audio: true, shape: Shape::Rect, video: false }
    }
}

/// Pictures\kirinuki
fn default_dir() -> PathBuf {
    use windows::Win32::UI::Shell::{FOLDERID_Pictures, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    unsafe {
        if let Ok(p) = SHGetKnownFolderPath(&FOLDERID_Pictures, KF_FLAG_DEFAULT, None) {
            let s = p.to_string().unwrap_or_default();
            windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
            if !s.is_empty() {
                return PathBuf::from(s).join("kirinuki");
            }
        }
    }
    std::env::temp_dir().join("kirinuki")
}

pub fn path() -> PathBuf {
    std::env::current_exe().map(|p| p.with_file_name("kirinuki.ini")).unwrap_or_else(|_| PathBuf::from("kirinuki.ini"))
}

impl Config {
    pub fn parse(text: &str) -> Config {
        let mut c = Config::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "save_dir" if !v.is_empty() => c.save_dir = PathBuf::from(v),
                "audio" => c.audio = v != "0",
                "video" => c.video = v == "1",
                "shape" => {
                    c.shape = match v {
                        "window" => Shape::Window,
                        "screen" => Shape::Screen,
                        _ => Shape::Rect,
                    }
                }
                _ => {}
            }
        }
        c
    }

    pub fn to_text(&self) -> String {
        let shape = match self.shape {
            Shape::Rect => "rect",
            Shape::Window => "window",
            Shape::Screen => "screen",
        };
        format!(
            "save_dir={}\r\naudio={}\r\nvideo={}\r\nshape={}\r\n",
            self.save_dir.display(),
            self.audio as u8,
            self.video as u8,
            shape
        )
    }

    pub fn load() -> Config {
        std::fs::read_to_string(path()).map(|t| Config::parse(&t)).unwrap_or_default()
    }

    pub fn save(&self) {
        let _ = std::fs::write(path(), self.to_text());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let c = Config { save_dir: PathBuf::from(r"E:\x y"), audio: false, shape: Shape::Window, video: true };
        assert_eq!(Config::parse(&c.to_text()), c);
    }
    #[test]
    fn defaults_on_garbage() {
        assert_eq!(Config::parse("nonsense\nshape=???\n"), Config::default());
    }
}
