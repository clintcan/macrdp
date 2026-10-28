//! The last session's virtual display mode, kept across restarts so the
//! display is created in it and the first connect skips the re-mode.
//!
//! Stored as `key=value` lines in `vd-mode` next to the TLS cert. Only the
//! primary display is remembered; a multi-monitor session's secondary
//! displays are still created at connect.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// A virtual display mode in framebuffer pixels and backing scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub scale: u32,
}

impl Mode {
    /// Rejects a size the session can't serve or a 2x mode `virtual_display_scale` never picks.
    pub fn new(width: u32, height: u32, scale: u32) -> Option<Self> {
        let in_band = |v: u32| (200..=8192).contains(&v);
        let ok = in_band(width)
            && in_band(height)
            && match scale {
                1 => true,
                2 => {
                    width.is_multiple_of(2)
                        && height.is_multiple_of(2)
                        && width >= 400
                        && height >= 400
                }
                _ => false,
            };
        ok.then_some(Self {
            width,
            height,
            scale,
        })
    }

    fn parse(text: &str) -> Option<Self> {
        let (mut width, mut height, mut scale) = (None, None, None);
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=')?;
            let value: u32 = value.trim().parse().ok()?;
            match key.trim() {
                "width" => width = Some(value),
                "height" => height = Some(value),
                "scale" => scale = Some(value),
                _ => {}
            }
        }
        Self::new(width?, height?, scale?)
    }

    fn render(&self) -> String {
        format!(
            "width={}\nheight={}\nscale={}\n",
            self.width, self.height, self.scale
        )
    }
}

/// On unless `MACRDP_REMEMBER_VD_MODE` turns it off.
pub fn enabled(env: Option<&str>) -> bool {
    !matches!(
        env.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "off" | "false" | "no")
    )
}

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("vd-mode")
}

/// The stored mode, or `None` when the file is missing, unreadable or invalid.
pub fn load(path: &Path) -> Option<Mode> {
    Mode::parse(&std::fs::read_to_string(path).ok()?)
}

/// Writes through a temp file so a crash mid-write never leaves a torn file.
pub fn save(path: &Path, mode: Mode) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, mode.render())?;
    std::fs::rename(&tmp, path)
}

static STORE: OnceLock<PathBuf> = OnceLock::new();
static LAST: Mutex<Option<Mode>> = Mutex::new(None);

/// Turns on [`remember`], with `current` being the mode already on disk.
pub fn init(path: PathBuf, current: Option<Mode>) {
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = current;
    let _ = STORE.set(path);
}

/// Records a mode just applied to the primary display; no-op before [`init`] or when unchanged.
pub fn remember(width: u32, height: u32, scale: u32) {
    let Some(path) = STORE.get() else {
        return;
    };
    let Some(mode) = Mode::new(width, height, scale) else {
        return;
    };
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if *last == Some(mode) {
        return;
    }
    match save(path, mode) {
        Ok(()) => {
            *last = Some(mode);
            tracing::debug!(?mode, path = %path.display(), "remembered the virtual display mode");
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "could not save the virtual display mode")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("macrdp-vdmode-{}-{name}", std::process::id()))
            .join("vd-mode")
    }

    #[test]
    fn parses_a_stored_mode() {
        assert_eq!(
            Mode::parse("width=3024\nheight=1898\nscale=2\n"),
            Some(Mode {
                width: 3024,
                height: 1898,
                scale: 2
            })
        );
        assert_eq!(
            Mode::parse("# comment\n\n scale = 1 \nheight=1080\nwidth=1920\nextra=7\n"),
            Mode::new(1920, 1080, 1)
        );
    }

    #[test]
    fn rejects_incomplete_or_malformed_text() {
        assert_eq!(Mode::parse(""), None);
        assert_eq!(Mode::parse("width=1920\nheight=1080\n"), None);
        assert_eq!(Mode::parse("width=1920\nheight=1080\nscale=x\n"), None);
        assert_eq!(Mode::parse("width=-1\nheight=1080\nscale=1\n"), None);
        assert_eq!(
            Mode::parse("garbage\nwidth=1920\nheight=1080\nscale=1"),
            None
        );
        assert_eq!(Mode::parse("\u{0}\u{ff}binary"), None);
    }

    #[test]
    fn validates_size_and_scale() {
        assert!(Mode::new(3024, 1898, 2).is_some());
        assert!(Mode::new(200, 200, 1).is_some());
        assert!(Mode::new(8192, 8192, 1).is_some());
        assert_eq!(Mode::new(199, 1080, 1), None);
        assert_eq!(Mode::new(1920, 8193, 1), None);
        assert_eq!(Mode::new(1920, 1080, 0), None);
        assert_eq!(Mode::new(1920, 1080, 3), None);
        assert_eq!(Mode::new(3025, 1898, 2), None);
        assert_eq!(Mode::new(398, 600, 2), None);
    }

    #[test]
    fn round_trips_through_the_file() {
        let path = temp_path("roundtrip");
        let mode = Mode::new(3024, 1898, 2).unwrap();
        save(&path, mode).unwrap();
        assert_eq!(load(&path), Some(mode));
        let other = Mode::new(1920, 1080, 1).unwrap();
        save(&path, other).unwrap();
        assert_eq!(load(&path), Some(other));
        assert!(!path.with_extension("tmp").exists());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_missing_or_corrupt_file_loads_as_none() {
        let path = temp_path("corrupt");
        assert_eq!(load(&path), None);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"width=3024\nheight=18").unwrap();
        assert_eq!(load(&path), None);
        std::fs::write(&path, [0xffu8, 0xfe, 0x00, 0x01]).unwrap();
        assert_eq!(load(&path), None);
        std::fs::write(&path, "width=9000\nheight=1080\nscale=1\n").unwrap();
        assert_eq!(load(&path), None);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn the_switch_defaults_on() {
        assert!(enabled(None));
        assert!(enabled(Some("1")));
        assert!(enabled(Some("maybe")));
        assert!(!enabled(Some("0")));
        assert!(!enabled(Some(" OFF ")));
        assert!(!enabled(Some("false")));
        assert!(!enabled(Some("no")));
    }
}
