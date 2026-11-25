//! The last session size the virtual display was re-moded to, kept across
//! restarts so a returning client's first connect needs no re-mode.
//!
//! Without it the display is created at `--width`/`--height` on every start,
//! and the first client re-modes it to its own size. ScreenCaptureKit is slow
//! to capture a just-re-moded display: the first connect after a restart took
//! 15.8 s from authentication to the first frame, the next ones 0.4-0.7 s
//! (live 2026-11-25, Windows App for macOS at 3024x1898).
//!
//! The file holds one line, `WIDTHxHEIGHT` in pixels. A missing or unreadable
//! file just means "no remembered size"; saving is best-effort.

use std::path::{Path, PathBuf};

/// The file in `dir` (macrdp's Application Support folder).
pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("last-session-size")
}

/// Smallest and largest sides accepted (MS-RDPBCGR desktop size limits).
const MIN_SIDE: u16 = 200;
const MAX_SIDE: u16 = 8192;

/// A size read back from the file, if it is one this build can serve under
/// `cap` (the operator's session-size cap).
pub fn parse(text: &str, cap: (u16, u16)) -> Option<(u16, u16)> {
    let (w, h) = text.trim().split_once('x')?;
    let (w, h) = (w.trim().parse::<u16>().ok()?, h.trim().parse::<u16>().ok()?);
    let ok = |v: u16, max: u16| (MIN_SIDE..=MAX_SIDE).contains(&v) && v <= max;
    (ok(w, cap.0) && ok(h, cap.1)).then_some((w, h))
}

/// The file's contents for a size.
pub fn format(width: u16, height: u16) -> String {
    format!("{width}x{height}\n")
}

/// The remembered size, if there is a usable one.
pub fn load(path: &Path, cap: (u16, u16)) -> Option<(u16, u16)> {
    parse(&std::fs::read_to_string(path).ok()?, cap)
}

/// Remembers a size. Best-effort: a failure is logged, never fatal.
pub fn save(path: &Path, width: u16, height: u16) {
    let tmp = path.with_extension("tmp");
    let result =
        std::fs::write(&tmp, format(width, height)).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = result {
        tracing::debug!(path = %path.display(), error = %e, "couldn't remember the session size");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: (u16, u16) = (4096, 2304);

    #[test]
    fn a_saved_size_reads_back() {
        assert_eq!(parse(&format(3024, 1898), CAP), Some((3024, 1898)));
        assert_eq!(parse(" 1920 x 1080 \n", CAP), Some((1920, 1080)));
    }

    #[test]
    fn unusable_contents_are_ignored() {
        for text in ["", "1920", "1920x", "x1080", "axb", "-1x1080", "70000x1080"] {
            assert_eq!(parse(text, CAP), None, "{text:?}");
        }
        // Outside the protocol's range, or over the operator's cap.
        assert_eq!(parse("199x1080", CAP), None);
        assert_eq!(parse("5120x1440", CAP), None);
        assert_eq!(parse("1920x2400", CAP), None);
        assert_eq!(parse("5120x1440", (8192, 8192)), Some((5120, 1440)));
    }

    #[test]
    fn save_then_load() {
        let dir = std::env::temp_dir().join(format!("macrdp-last-size-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = path_in(&dir);
        assert_eq!(load(&path, CAP), None);
        save(&path, 3024, 1898);
        assert_eq!(load(&path, CAP), Some((3024, 1898)));
        save(&path, 1920, 1080);
        assert_eq!(load(&path, CAP), Some((1920, 1080)));
        std::fs::remove_dir_all(&dir).ok();
    }
}
