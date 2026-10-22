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

/// The filter used without `RUST_LOG`: debug everywhere with `--verbose`,
/// else info with known noisy third-party targets quieted:
///   - rustls SNI warnings (clients legally put IPs there)
///   - the ironrdp_server::encoder "took N ms" timer (fires above 10ms)
///   - ironrdp_server::server WARN ("Unexpected share data pdu"); ERRORs
///     are still shown — that's where the cert-acceptance "Broken pipe"
///     comes from, which we can't suppress without losing real errors.
const DEFAULT_FILTER: &str =
    "info,rustls=error,ironrdp_server::encoder=error,ironrdp_server::server=error";

/// Third-party targets that log client mistakes as warnings on each
/// connection; lowered also under `RUST_LOG` unless it names the target.
/// rustls: "Illegal SNI extension: ignoring IP address presented as hostname"
/// (mstsc sends the server IP as SNI).
const QUIET_TARGETS: &[(&str, &str)] = &[("rustls", "error")];

/// The directives of a `RUST_LOG` value plus a default for each quiet target
/// it does not name. With a global `off`, nothing is added (`off` stays off).
///
/// The split on ',' also splits a span field list (`[span{a=1,b=2}]`); the
/// pieces only fail to match a quiet target, so at most a default is added.
///
/// Ported from macrdp-v1 3ad34dd / 84a4069 (`filter_directives`), relicensed
/// by the author.
pub fn filter_directives(user: &str) -> String {
    let mut out = user.trim().to_owned();
    let global_off = user
        .split(',')
        .any(|d| d.trim().eq_ignore_ascii_case("off"));
    if global_off {
        return out;
    }
    for (target, level) in QUIET_TARGETS {
        let named = user.split(',').any(|d| {
            let t = d.trim().split(['=', '[']).next().unwrap_or("");
            t == *target || t.starts_with(&format!("{target}::"))
        });
        if !named {
            if !out.is_empty() {
                out.push(',');
            }
            out.push_str(&format!("{target}={level}"));
        }
    }
    out
}

/// The log filter: `RUST_LOG` when set and not empty (with the quiet targets
/// of [`filter_directives`]), else [`DEFAULT_FILTER`] (`debug` with
/// `verbose`). An invalid `RUST_LOG` is not used: a line goes to `notices`
/// (logged once the logger is up) and the default filter is used.
pub fn build_filter(rust_log: Option<&str>, verbose: bool, notices: &mut Vec<String>) -> EnvFilter {
    let default = || EnvFilter::new(if verbose { "debug" } else { DEFAULT_FILTER });
    let Some(user) = rust_log.filter(|v| !v.trim().is_empty()) else {
        return default();
    };
    match EnvFilter::try_new(filter_directives(user)) {
        Ok(f) => f,
        Err(e) => {
            notices.push(format!(
                "invalid RUST_LOG {user:?} ({e}): not used, logging with the default filter"
            ));
            default()
        }
    }
}

/// Initialize the global tracing subscriber.
///
/// `filter` is the already-resolved [`EnvFilter`]. `log_dir_override` is the
/// `--log-dir` / `LOG_DIR` value, if any. Main-sink resolution:
/// - `Some(dir)` → rotating file in `dir` (explicit override, even interactively).
/// - else stdout-is-a-TTY → stdout (interactive / `cargo run`).
/// - else → rotating file in `~/Library/Logs` (headless under launchd).
///
/// `audit_file` is the optional dedicated **JSON audit stream** (`--audit-file` /
/// `AUDIT_FILE` / `MACRDP_AUDIT_JSON`, off by default). When `Some`, the
/// `macrdp::audit` target is *additionally* written there as one JSON object per
/// line — independent of `RUST_LOG` (a `Targets` per-layer filter pins it to
/// `INFO`, so a quiet operational filter never suppresses security events) — for
/// a SIEM collector to tail. The human-readable audit lines still appear in the
/// main log unchanged. **When `audit_file` is `None` the setup is byte-identical
/// to before** (the audit-off path below).
///
/// Any failure to set up a file sink falls back to stdout with an `eprintln!`
/// note (which, under launchd, lands in `StandardErrorPath` = `macrdp.err.log`);
/// an unopenable audit file disables only the JSON stream, never startup.
pub fn init(filter: EnvFilter, log_dir_override: Option<&Path>, audit_file: Option<&Path>) {
    let dir = resolve_main_dir(log_dir_override);

    // Audit-off (default): the exact prior single-sink behavior, unchanged.
    let Some(audit_file) = audit_file else {
        init_main_only(filter, dir);
        return;
    };

    // Audit-on: add a JSON `macrdp::audit` layer alongside the main sink. If the
    // audit file can't be opened, fall back to main-only — telemetry setup must
    // never fail startup.
    let audit_layer = match build_audit_layer(audit_file) {
        Ok(layer) => layer,
        Err(e) => {
            eprintln!("macrdp: cannot open audit file {audit_file:?}: {e}; audit JSON disabled");
            init_main_only(filter, dir);
            return;
        }
    };

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;

    // Build the main layer (stdout or rotating file) as a boxed layer so both
    // sub-cases share one registry init. The `EnvFilter` is a per-layer filter on
    // the main layer only, so it governs operational verbosity without touching
    // the audit stream. A `Vec<Box<dyn Layer>>` collapses the heterogeneous
    // main+audit layers into one — the canonical way to compose a dynamic set.
    let mut main_is_file = false;
    let main_layer: Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync> = match &dir {
        None => tracing_subscriber::fmt::layer().with_filter(filter).boxed(),
        Some(d) => match make_file_writer(d) {
            Some(writer) => {
                main_is_file = true;
                tracing_subscriber::fmt::layer()
                    .with_writer(writer)
                    .with_ansi(false)
                    .with_filter(filter)
                    .boxed()
            }
            None => tracing_subscriber::fmt::layer().with_filter(filter).boxed(),
        },
    };

    let layers: Vec<Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>> =
        vec![main_layer, audit_layer];
    tracing_subscriber::registry().with(layers).init();

    if main_is_file {
        install_panic_hook();
        tracing::info!(dir = ?dir, "logging to rotating file ({BASE})");
    }
    tracing::info!(audit_file = ?audit_file, "JSON audit stream enabled (macrdp::audit)");
}

/// Resolve the main-sink directory (see [`init`]): explicit `--log-dir` wins,
/// else a file when headless (stdout not a TTY), else stdout (interactive).
fn resolve_main_dir(log_dir_override: Option<&Path>) -> Option<PathBuf> {
    match log_dir_override {
        Some(d) => Some(d.to_path_buf()),
        None => {
            if std::io::stdout().is_terminal() {
                None
            } else {
                default_log_dir()
            }
        }
    }
}

/// The prior audit-off setup: a single sink (stdout or rotating file) with the
/// `EnvFilter` global. Kept verbatim so the default path is byte-identical.
fn init_main_only(filter: EnvFilter, dir: Option<PathBuf>) {
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

/// Create the main rotating-file writer for the audit-on path, or `None` (with an
/// `eprintln!`) so the caller falls back to a stdout main layer.
fn make_file_writer(dir: &Path) -> Option<RotatingWriter> {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("macrdp: cannot create log dir {dir:?}: {e}; logging to stdout");
        return None;
    }
    match RotatingWriter::new(dir) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("macrdp: cannot open log file in {dir:?}: {e}; logging to stdout");
            None
        }
    }
}

/// Split an audit-file path into `(parent_dir, filename)`, defaulting a bare
/// filename to the current dir and a **directory** target (an existing dir, or a
/// path written with a trailing separator) to `<dir>/macrdp-audit.log`.
fn split_audit_path(path: &Path) -> (PathBuf, String) {
    const DEFAULT: &str = "macrdp-audit.log";
    // Treat the whole path as a directory when it's an existing dir or was written
    // with a trailing separator (e.g. `/var/log/`) — otherwise `file_name()` would
    // pick the last component (`log`) and write `/var/log/log`.
    let looks_like_dir = path.is_dir()
        || path
            .as_os_str()
            .to_string_lossy()
            .ends_with(std::path::MAIN_SEPARATOR);
    if looks_like_dir {
        return (path.to_path_buf(), DEFAULT.to_string());
    }
    let base = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT.to_string());
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    (dir, base)
}

/// Build the JSON `macrdp::audit` layer: a self-rotating file, one JSON object per
/// line, filtered to the `macrdp::audit` target at `INFO` **independent of
/// `RUST_LOG`**. Boxed so it composes with either main-sink variant.
fn build_audit_layer(
    audit_file: &Path,
) -> io::Result<Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>> {
    use tracing_subscriber::filter::{LevelFilter, Targets};
    use tracing_subscriber::Layer;

    let (dir, base) = split_audit_path(audit_file);
    std::fs::create_dir_all(&dir)?;
    let max_bytes = env_u64("MACRDP_AUDIT_LOG_MAX_BYTES", DEFAULT_MAX_BYTES);
    let max_files = env_usize("MACRDP_AUDIT_LOG_MAX_FILES", DEFAULT_MAX_FILES);
    let writer = RotatingWriter::with_base(&dir, base, max_bytes, max_files)?;

    let layer = tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true)
        .with_current_span(false)
        .with_span_list(false)
        .with_ansi(false)
        .with_writer(writer)
        .with_filter(Targets::new().with_target("macrdp::audit", LevelFilter::INFO))
        .boxed();
    Ok(layer)
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
    /// The live filename (e.g. `macrdp.log` or `macrdp-audit.log`); archives are
    /// `<base>.1`, `.2`, … . Parameterized so the same rotation code backs both
    /// the operational log and the dedicated JSON audit stream.
    base: String,
    file: File,
    written: u64,
    max_bytes: u64,
    max_files: usize,
}

impl Rotator {
    fn open(dir: PathBuf, base: String, max_bytes: u64, max_files: usize) -> io::Result<Self> {
        let path = dir.join(&base);
        // Append + seed the counter from the existing length so a KeepAlive
        // restart continues the same file instead of truncating it.
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            dir,
            base,
            file,
            written,
            max_bytes,
            max_files,
        })
    }

    /// `<base> → .1 → .2 → … → .max_files` (oldest dropped), then reopen a fresh
    /// empty `<base>`. `rename` replaces the destination atomically on unix, so
    /// the cascade alone drops the oldest.
    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let base = self.dir.join(&self.base);
        for n in (1..self.max_files).rev() {
            let from = self.dir.join(format!("{}.{n}", self.base));
            if from.exists() {
                std::fs::rename(&from, self.dir.join(format!("{}.{}", self.base, n + 1)))?;
            }
        }
        if self.max_files >= 1 {
            std::fs::rename(&base, self.dir.join(format!("{}.1", self.base)))?;
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
        Self::with_base(dir, BASE.to_string(), max_bytes, max_files)
    }

    /// Open `dir/<base>` (append) with explicit rotation limits — used for the
    /// dedicated JSON audit stream (`macrdp-audit.log`), independent of the
    /// operational log's size/count knobs.
    pub fn with_base(
        dir: &Path,
        base: String,
        max_bytes: u64,
        max_files: usize,
    ) -> io::Result<Self> {
        let rot = Rotator::open(dir.to_path_buf(), base, max_bytes, max_files)?;
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

    #[test]
    fn rust_log_gets_the_quiet_targets_unless_it_names_them() {
        assert_eq!(filter_directives("debug"), "debug,rustls=error");
        assert_eq!(
            filter_directives("info,macrdp::h264=trace"),
            "info,macrdp::h264=trace,rustls=error"
        );
        assert_eq!(filter_directives("info,rustls=debug"), "info,rustls=debug");
        assert_eq!(
            filter_directives("info,rustls::server=warn"),
            "info,rustls::server=warn"
        );
        assert_eq!(filter_directives("off"), "off");
        assert_eq!(filter_directives(""), "rustls=error");
    }

    #[test]
    fn an_invalid_rust_log_is_reported_and_not_used() {
        let mut notices = Vec::new();
        let f = build_filter(Some("info,macrdp=nonsense"), false, &mut notices);
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("invalid RUST_LOG"), "{notices:?}");
        assert_eq!(f.to_string(), EnvFilter::new(DEFAULT_FILTER).to_string());
        let mut notices = Vec::new();
        let f = build_filter(Some("info"), false, &mut notices);
        assert!(notices.is_empty());
        assert_eq!(
            f.to_string(),
            EnvFilter::new("info,rustls=error").to_string()
        );
        // Unset or empty: the default (debug with --verbose), no notice.
        for v in [None, Some(""), Some("  ")] {
            let f = build_filter(v, true, &mut notices);
            assert_eq!(f.to_string(), EnvFilter::new("debug").to_string());
        }
        assert!(notices.is_empty());
    }

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
        let mut r = Rotator::open(dir.clone(), BASE.to_string(), 100, 5).unwrap();
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
        let mut r = Rotator::open(dir.clone(), BASE.to_string(), 50, max_files).unwrap();
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
        let r = Rotator::open(dir.clone(), BASE.to_string(), 1024, 5).unwrap();
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

    #[test]
    fn custom_base_rotates_under_its_own_name() {
        let dir = unique_dir("audit-base");
        let mut r = Rotator::open(dir.clone(), "macrdp-audit.log".to_string(), 100, 5).unwrap();
        r.write_all(&[b'a'; 80]).unwrap();
        r.write_all(&[b'b'; 80]).unwrap(); // crosses 100 → rotates
        r.flush().unwrap();
        assert!(
            dir.join("macrdp-audit.log.1").exists(),
            "archives under base"
        );
        assert!(
            !dir.join("macrdp.log").exists(),
            "must not touch the main-log name"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_audit_path_variants() {
        let (d, b) = split_audit_path(Path::new("/var/log/macrdp-audit.log"));
        assert_eq!(d, PathBuf::from("/var/log"));
        assert_eq!(b, "macrdp-audit.log");
        // Bare filename → current dir.
        let (d, b) = split_audit_path(Path::new("audit.log"));
        assert_eq!(d, PathBuf::from("."));
        assert_eq!(b, "audit.log");
        // Trailing-separator directory → default filename inside it (not `/var/log/log`).
        let (d, b) = split_audit_path(Path::new("/var/log/"));
        assert_eq!(d, PathBuf::from("/var/log/"));
        assert_eq!(b, "macrdp-audit.log");
    }

    /// The load-bearing guarantee: the audit JSON sink receives `macrdp::audit`
    /// events at INFO **even when the operational filter is `warn`** — i.e.
    /// `RUST_LOG` can't suppress security events from the SIEM stream.
    #[test]
    fn audit_layer_bypasses_operational_filter() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::filter::{LevelFilter, Targets};
        use tracing_subscriber::fmt::MakeWriter;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::{EnvFilter, Layer};

        #[derive(Clone)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        struct BufW(Arc<Mutex<Vec<u8>>>);
        impl<'a> MakeWriter<'a> for Buf {
            type Writer = BufW;
            fn make_writer(&'a self) -> BufW {
                BufW(self.0.clone())
            }
        }
        impl Write for BufW {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let main_buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let audit_buf = Buf(Arc::new(Mutex::new(Vec::new())));

        let main_layer = tracing_subscriber::fmt::layer()
            .with_writer(main_buf.clone())
            .with_ansi(false)
            .with_filter(EnvFilter::new("warn"));
        let audit_layer = tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_writer(audit_buf.clone())
            .with_filter(Targets::new().with_target("macrdp::audit", LevelFilter::INFO));

        let subscriber = tracing_subscriber::registry()
            .with(main_layer)
            .with(audit_layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "macrdp::audit", event = "accept", src_ip = "203.0.113.5");
            tracing::info!("an operational info line");
        });

        let audit = String::from_utf8(audit_buf.0.lock().unwrap().clone()).unwrap();
        let main = String::from_utf8(main_buf.0.lock().unwrap().clone()).unwrap();
        // Reached the audit sink as JSON despite the `warn` operational filter.
        assert!(
            audit.contains("\"event\":\"accept\"") && audit.contains("203.0.113.5"),
            "audit sink should carry the event as JSON: {audit}"
        );
        // The `warn` main filter dropped both info lines from the operational sink.
        assert!(
            !main.contains("accept") && !main.contains("operational info line"),
            "main warn filter must drop info-level lines: {main}"
        );
    }
}
