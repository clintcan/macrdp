//! Startup reaper — sweep leftovers from a PRIOR macrdp process that died
//! uncleanly.
//!
//! The graceful SIGINT/SIGTERM path (`main`'s signal handler →
//! `file_promise_lazy::shutdown_cleanup` + `rdpdr::shutdown_cleanup`) already
//! unmounts NFS volumes and removes paste temp dirs. But **SIGKILL / panic /
//! power-loss skip all of it**, stranding:
//!   - RDPDR NFS mounts + `$TMPDIR/macrdp-rdpdr-<pid>/<label>` mountpoints,
//!   - eager-paste `$TMPDIR/macrdp-paste-<pid>-<nanos>` dirs,
//!   - lazy-paste `$TMPDIR/macrdp-lazy-paste-<pid>-<nanos>` dirs.
//!
//! These are all named with the owning **pid** and live under
//! `std::env::temp_dir()` (= `$TMPDIR`, stable across relaunches for a user), so
//! a fresh start can find and reap them. The per-module reapers live next to
//! each module's `shutdown_cleanup` (so each owns its own path scheme); this
//! module holds the shared, platform-independent primitives.
//!
//! **Safety:** a path is only reaped when its embedded pid is **dead** (and never
//! our own pid). A reused pid reads as alive → we skip (a harmless leftover, not
//! a wrong delete), so the reaper is safe to run while another macrdp instance
//! is live.

use std::path::Path;

/// True if a process with `pid` currently exists.
///
/// `kill(pid, 0)` does the permission/existence check without sending a signal:
/// `0` = exists (ours), `EPERM` = exists (not ours → still alive), `ESRCH` = no
/// such process (dead). Only `ESRCH` means "reap"; everything else is treated as
/// alive (conservative — we never want a false "dead").
#[cfg(unix)]
pub fn process_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return true; // never reap on a nonsense pid
    }
    // SAFETY: signal 0 performs error checking only; no signal is delivered.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    !matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    )
}

#[cfg(not(unix))]
pub fn process_is_alive(_pid: i32) -> bool {
    true
}

/// Parse the owning pid out of a macrdp temp-dir name.
///
/// `pid_is_last` distinguishes the two schemes:
/// - RDPDR `macrdp-rdpdr-<pid>` → `true` (pid is the whole remainder),
/// - paste `macrdp-{paste,lazy-paste}-<pid>-<nanos>` → `false` (pid is the first
///   `-`-separated field of the remainder).
///
/// Prefixes are distinct (`macrdp-paste-` is not a prefix of
/// `macrdp-lazy-paste-`), so a `strip_prefix` match never cross-classifies.
pub fn pid_from_tagged(name: &str, prefix: &str, pid_is_last: bool) -> Option<i32> {
    let rest = name.strip_prefix(prefix)?;
    let pid_str = if pid_is_last {
        rest
    } else {
        rest.split('-').next()?
    };
    pid_str.parse::<i32>().ok().filter(|&p| p > 0)
}

/// For each immediate child of `base` whose name is `<prefix><pid>…` for a
/// **dead** pid other than `self_pid`, invoke `action(path)`.
///
/// `is_alive` is injected for testability; production callers pass
/// [`process_is_alive`]. Best-effort: an unreadable `base` is silently skipped.
pub fn for_each_stale(
    base: &Path,
    prefix: &str,
    pid_is_last: bool,
    self_pid: i32,
    is_alive: &dyn Fn(i32) -> bool,
    action: &mut dyn FnMut(&Path),
) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(pid) = pid_from_tagged(name, prefix, pid_is_last) else {
            continue;
        };
        if pid == self_pid || is_alive(pid) {
            continue;
        }
        action(&entry.path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn unique_base(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "macrdp-reaptest-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pid_from_tagged_parses_both_schemes() {
        assert_eq!(
            pid_from_tagged("macrdp-rdpdr-1234", "macrdp-rdpdr-", true),
            Some(1234)
        );
        assert_eq!(
            pid_from_tagged("macrdp-paste-1234-99", "macrdp-paste-", false),
            Some(1234)
        );
        assert_eq!(
            pid_from_tagged("macrdp-lazy-paste-1234-99", "macrdp-lazy-paste-", false),
            Some(1234)
        );
        // Cross-classification guard: a lazy dir must not match the eager prefix
        // with pid_is_last semantics (and even if it did, "lazy" doesn't parse).
        assert_eq!(
            pid_from_tagged("macrdp-lazy-paste-1234-99", "macrdp-paste-", false),
            None
        );
        // Garbage / wrong prefix / nonpositive pid.
        assert_eq!(
            pid_from_tagged("macrdp-rdpdr-abc", "macrdp-rdpdr-", true),
            None
        );
        assert_eq!(
            pid_from_tagged("something-else", "macrdp-rdpdr-", true),
            None
        );
        assert_eq!(
            pid_from_tagged("macrdp-rdpdr-0", "macrdp-rdpdr-", true),
            None
        );
    }

    #[test]
    fn for_each_stale_only_actions_dead_non_self_dirs() {
        let base = unique_base("stale");
        // dead pid 100, alive pid 200, self pid 300.
        for tag in [
            "macrdp-paste-100-1",
            "macrdp-paste-200-2",
            "macrdp-paste-300-3",
        ] {
            std::fs::create_dir_all(base.join(tag)).unwrap();
        }
        let is_alive = |pid: i32| pid == 200;
        let mut reaped: Vec<String> = Vec::new();
        for_each_stale(&base, "macrdp-paste-", false, 300, &is_alive, &mut |p| {
            reaped.push(p.file_name().unwrap().to_string_lossy().into_owned());
        });
        assert_eq!(reaped, vec!["macrdp-paste-100-1".to_string()]);
        let _ = std::fs::remove_dir_all(&base);
    }
}
