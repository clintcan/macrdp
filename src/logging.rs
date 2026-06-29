//! Tracing sink + size-based log rotation.
//!
//! By default macrdp writes tracing to **stdout** (great for `cargo run`). Under
//! the LaunchAgent there is no terminal, so we instead write to a **self-owned,
//! size-bounded rotating file** at `~/Library/Logs/macrdp.log` — without it the
//! launchd-redirected log grew unbounded. The live file keeps the **stable name**
//! `macrdp.log` (the GUI controller reads that exact path and detects crashes by
//! the substring `"panicked"`), rotating logrotate-style to `macrdp.log.1`, `.2`,
//! … up to `max_files`, dropping the oldest.
//!
//! Why a custom rotator and not `tracing-appender`: its `RollingFileAppender`
//! only does **time/date-suffixed** files (`macrdp.log.2026-06-30`), which breaks
//! the stable-name contract above, and its `non_blocking` writer was tried for
//! macrdp before and reverted. This writer is **blocking** and **size-based**.
//!
//! Sink selection (see [`init`]): an explicit `--log-dir` always wins; otherwise
//! we log to a file when stdout is **not** a TTY (the headless/launchd case) and
//! to stdout when it is (interactive). On the file path we also install a panic
//! hook so Rust's panic message lands in `macrdp.log` (preserving the GUI's crash
//! detection now that stderr no longer goes to that file).

use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

/// The stable live-log filename. Archives are `macrdp.log.1`, `.2`, …
const BASE: &str = "macrdp.log";

const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB
const DEFAULT_MAX_FILES: usize = 5;

/// Initialize the global tracing subscriber.
///
/// `filter` is the already-resolved [`EnvFilter`]. `log_dir_override` is the
/// `--log-dir` / `LOG_DIR` value, if any. Resolution:
/// - `Some(dir)` → rotating file in `dir` (explicit override, even interactively).
/// - else stdout-is-a-TTY → stdout (interactive / `cargo run`).
/// - else → rotating file in `~/Library/Logs` (headless under launchd).
///
/// Any failure to set up the file sink falls back to stdout with an `eprintln!`
/// note (which, under launchd, lands in `StandardErrorPath` = `macrdp.err.log`).
pub fn init(filter: EnvFilter, log_dir_override: Option<&Path>) {
    let dir = match log_dir_override {
        Some(d) => Some(d.to_path_buf()),
        None => {
            if std::io::stdout().is_terminal() {
                None
            } else {
                default_log_dir()
            }
        }
    };

    let Some(dir) = dir else {
        // Interactive / no HOME: plain stdout, ANSI on. Unchanged dev behavior.
        tracing_subscriber::fmt().with_env_filter(filter).init();
        return;
    };

    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("macrdp: cannot create log dir {dir:?}: {e}; logging to stdout");
        tracing_subscriber::fmt().with_env_filter(filter).init();
        return;
    }

    let writer = match RotatingWriter::new(&dir) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("macrdp: cannot open log file in {dir:?}: {e}; logging to stdout");
            tracing_subscriber::fmt().with_env_filter(filter).init();
            return;
        }
    };

    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(filter)
        .init();
    install_panic_hook();
    tracing::info!(dir = ?dir, "logging to rotating file ({BASE})");
}

/// `~/Library/Logs` (mirrors `main::default_cert_dir`'s HOME resolution).
fn default_log_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Logs"))
}

/// Route Rust panics through tracing so the message reaches `macrdp.log` (its
/// `Display` contains `"panicked at"`, which the GUI scans for), then chain the
/// previous hook. **Observability only** — does NOT run cleanup (a panicking
/// tokio task unwinds without killing the process; teardown is the startup
/// reaper's job, see `crate::reaper`).
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("{info}");
        prev(info);
    }));
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

fn env_usize(key: &str, default: usize) -> usize {
    // 0 is a legal value here (no archives kept), so don't filter it out.
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(default)
}

/// The rotating file behind a mutex. Tracking `written` in-memory avoids a
/// `stat` per event. **The rotation/write code must never emit a tracing event**
/// (`std::sync::Mutex` is non-reentrant) — errors are reported via `eprintln!`.
struct Rotator {
    dir: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
    max_files: usize,
}

impl Rotator {
    fn open(dir: PathBuf, max_bytes: u64, max_files: usize) -> io::Result<Self> {
        let path = dir.join(BASE);
        // Append + seed the counter from the existing length so a KeepAlive
        // restart continues the same file instead of truncating it.
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            dir,
            file,
            written,
            max_bytes,
            max_files,
        })
    }

    /// `macrdp.log → .1 → .2 → … → .max_files` (oldest dropped), then reopen a
    /// fresh empty `macrdp.log`. `rename` replaces the destination atomically on
    /// unix, so the cascade alone drops the oldest.
    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let base = self.dir.join(BASE);
        for n in (1..self.max_files).rev() {
            let from = self.dir.join(format!("{BASE}.{n}"));
            if from.exists() {
                std::fs::rename(&from, self.dir.join(format!("{BASE}.{}", n + 1)))?;
            }
        }
        if self.max_files >= 1 {
            std::fs::rename(&base, self.dir.join(format!("{BASE}.1")))?;
        } else {
            std::fs::remove_file(&base)?;
        }
        self.file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&base)?;
        self.written = 0;
        Ok(())
    }
}

impl Write for Rotator {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Rotate before an event that would push us over the cap (but never on an
        // empty file — an oversized single event then writes whole, one big file,
        // rather than looping forever).
        if self.written > 0 && self.written + buf.len() as u64 > self.max_bytes {
            if let Err(e) = self.rotate() {
                // Best-effort: keep writing to the current file rather than drop
                // logs. eprintln lands in macrdp.err.log under launchd.
                eprintln!("macrdp: log rotation failed: {e}");
            }
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// `MakeWriter` handle. Cloneable + `'static` so the fmt subscriber can own it.
#[derive(Clone)]
pub struct RotatingWriter(Arc<Mutex<Rotator>>);

impl RotatingWriter {
    /// Open `dir/macrdp.log` (append), reading `MACRDP_LOG_MAX_BYTES`
    /// (default 10 MiB) and `MACRDP_LOG_MAX_FILES` (default 5).
    pub fn new(dir: &Path) -> io::Result<Self> {
        let max_bytes = env_u64("MACRDP_LOG_MAX_BYTES", DEFAULT_MAX_BYTES);
        let max_files = env_usize("MACRDP_LOG_MAX_FILES", DEFAULT_MAX_FILES);
        let rot = Rotator::open(dir.to_path_buf(), max_bytes, max_files)?;
        Ok(Self(Arc::new(Mutex::new(rot))))
    }
}

/// Per-event guard: a bare `MutexGuard` doesn't impl `io::Write`, so wrap it.
pub struct LockedRotator<'a>(MutexGuard<'a, Rotator>);

impl Write for LockedRotator<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'a> MakeWriter<'a> for RotatingWriter {
    type Writer = LockedRotator<'a>;
    fn make_writer(&'a self) -> Self::Writer {
        // Recover from a poisoned lock (a panic while logging) rather than
        // panicking again — keep logging alive.
        LockedRotator(self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "macrdp-logtest-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rotates_at_threshold() {
        let dir = unique_dir("rotate");
        let mut r = Rotator::open(dir.clone(), 100, 5).unwrap();
        // Two ~80-byte writes: the first fits, the second crosses 100 → rotates.
        r.write_all(&[b'a'; 80]).unwrap();
        assert!(!dir.join("macrdp.log.1").exists());
        r.write_all(&[b'b'; 80]).unwrap();
        r.flush().unwrap();
        assert!(dir.join("macrdp.log.1").exists(), "archive should exist");
        // The live file now holds only the second write.
        let live = std::fs::read(dir.join("macrdp.log")).unwrap();
        assert_eq!(live.len(), 80);
        assert_eq!(live[0], b'b');
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn respects_max_files_cap() {
        let dir = unique_dir("cap");
        let max_files = 2;
        let mut r = Rotator::open(dir.clone(), 50, max_files).unwrap();
        // Force several rotations.
        for _ in 0..6 {
            r.write_all(&[b'x'; 60]).unwrap();
        }
        r.flush().unwrap();
        assert!(dir.join("macrdp.log").exists());
        assert!(dir.join("macrdp.log.1").exists());
        assert!(dir.join("macrdp.log.2").exists());
        assert!(
            !dir.join(format!("macrdp.log.{}", max_files + 1)).exists(),
            "must not keep more than max_files archives"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appends_and_seeds_counter_on_open() {
        let dir = unique_dir("seed");
        std::fs::write(dir.join("macrdp.log"), b"preexisting-content").unwrap();
        let r = Rotator::open(dir.clone(), 1024, 5).unwrap();
        assert_eq!(r.written, "preexisting-content".len() as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_parsing_falls_back_to_defaults() {
        // Absent / unset → defaults (these env vars are not set in the test env).
        assert_eq!(
            env_u64("MACRDP_LOG_MAX_BYTES_NOPE", DEFAULT_MAX_BYTES),
            DEFAULT_MAX_BYTES
        );
        assert_eq!(
            env_usize("MACRDP_LOG_MAX_FILES_NOPE", DEFAULT_MAX_FILES),
            DEFAULT_MAX_FILES
        );
    }
}
