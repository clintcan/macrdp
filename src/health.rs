//! Health-check watchdog — turn a hung-but-alive process into a clean exit so
//! the launchd `KeepAlive` (or, under `--fork-workers`, the supervisor) revives
//! a fresh one.
//!
//! `launchd` only restarts macrdp when the process *dies*. A process that is
//! alive but **wedged** — the tokio runtime deadlocked, every worker thread
//! blocked on a poisoned mutex or a blocking syscall, the accept loop stuck —
//! keeps running and unreachable, and nothing revives it. That's the one
//! production gap `KeepAlive` can't close on its own; this watchdog closes it.
//!
//! ## Mechanism
//!
//! A dedicated **OS thread** (deliberately *not* a tokio task, so it keeps
//! ticking even when the runtime it watches is wedged) periodically submits a
//! trivial probe task onto the runtime and waits a bounded time for it to run.
//! A healthy runtime executes the probe in microseconds; a deadlocked one never
//! does. After `failures_before_exit` consecutive misses the watchdog logs and
//! [`std::process::exit`]s with a distinct code, so the supervisor/launchd
//! starts a clean process.
//!
//! It is intentionally conservative — a generous timeout plus a
//! multiple-consecutive-miss requirement — so a merely busy runtime (a heavy
//! encode burst, a GC pause) is never mistaken for a wedge. It only fires on a
//! genuine, sustained inability to run *any* task, which is exactly the
//! hung-but-alive failure mode.
//!
//! ## What it does NOT catch
//!
//! A logic bug that silently stops the *accept loop* while the runtime itself
//! stays healthy (tasks still run) is out of scope — detecting that without
//! false-positiving on a legitimately idle server needs a listener-level
//! heartbeat, a separate follow-up. This watchdog targets runtime-level hangs,
//! which are the common "alive but wedged" case.

use std::sync::mpsc;
use std::time::Duration;

use tracing::{error, info, warn};

/// Exit code used when the watchdog fires. `launchd`'s `KeepAlive` restarts on
/// any nonzero exit; a distinct, stable code (EX_SOFTWARE from `sysexits.h`)
/// makes "the watchdog bounced us" greppable in the logs / crash reports.
pub const WATCHDOG_EXIT_CODE: i32 = 70;

/// Watchdog timing. Defaults are deliberately generous (a wedge must persist
/// ~60–90 s before a bounce) so normal load spikes never trip it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HealthConfig {
    /// Delay between probes.
    pub probe_interval: Duration,
    /// Max time a single probe may take before it counts as a miss.
    pub timeout: Duration,
    /// Consecutive misses required before exiting.
    pub failures_before_exit: u32,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            probe_interval: Duration::from_secs(15),
            timeout: Duration::from_secs(30),
            failures_before_exit: 2,
        }
    }
}

impl HealthConfig {
    /// Read tunables from the environment, falling back to [`Self::default`] for
    /// any unset / unparsable / zero value:
    /// `MACRDP_HEALTHCHECK_INTERVAL_SECS`, `_TIMEOUT_SECS`, `_FAILURES`.
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            probe_interval: parse_secs(
                std::env::var("MACRDP_HEALTHCHECK_INTERVAL_SECS")
                    .ok()
                    .as_deref(),
                d.probe_interval,
            ),
            timeout: parse_secs(
                std::env::var("MACRDP_HEALTHCHECK_TIMEOUT_SECS")
                    .ok()
                    .as_deref(),
                d.timeout,
            ),
            failures_before_exit: parse_u32(
                std::env::var("MACRDP_HEALTHCHECK_FAILURES").ok().as_deref(),
                d.failures_before_exit,
            ),
        }
    }
}

/// Decide whether to arm the watchdog for this process.
///
/// - A fork **worker** never arms it: the worker is short-lived (one connection
///   then exits) and the supervisor owns its liveness.
/// - An explicit `MACRDP_HEALTHCHECK=1/0` (on/off/true/false/yes/no) wins.
/// - Otherwise the default is **on when headless** (stdout is not a TTY, i.e.
///   running under launchd, which will restart a bounced process) and **off**
///   interactively (`cargo run` in a terminal — a false bounce there just kills
///   a dev session, and nothing would restart it anyway).
pub fn should_arm(is_worker: bool, stdout_is_tty: bool, env_override: Option<&str>) -> bool {
    if is_worker {
        return false;
    }
    match env_override
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("1") | Some("on") | Some("true") | Some("yes") => true,
        Some("0") | Some("off") | Some("false") | Some("no") => false,
        _ => !stdout_is_tty,
    }
}

/// Spawn the watchdog OS thread against `handle` (a handle to the tokio runtime
/// whose liveness is probed). Returns immediately; the thread runs for the
/// process lifetime, or until it fires and exits the process.
pub fn spawn(handle: tokio::runtime::Handle, cfg: HealthConfig) {
    info!(
        interval_secs = cfg.probe_interval.as_secs(),
        timeout_secs = cfg.timeout.as_secs(),
        failures = cfg.failures_before_exit,
        "health-check watchdog armed"
    );
    let _ = std::thread::Builder::new()
        .name("macrdp-watchdog".into())
        .spawn(move || run(handle, cfg));
}

fn run(handle: tokio::runtime::Handle, cfg: HealthConfig) {
    let mut consecutive_misses: u32 = 0;
    loop {
        std::thread::sleep(cfg.probe_interval);
        if probe_ok(&handle, cfg.timeout) {
            if consecutive_misses > 0 {
                info!("health-check watchdog: tokio runtime responsive again");
            }
            consecutive_misses = 0;
            continue;
        }
        consecutive_misses += 1;
        warn!(
            miss = consecutive_misses,
            of = cfg.failures_before_exit,
            timeout_secs = cfg.timeout.as_secs(),
            "health-check watchdog: tokio runtime did not answer a liveness probe"
        );
        if consecutive_misses >= cfg.failures_before_exit {
            error!(
                exit_code = WATCHDOG_EXIT_CODE,
                misses = consecutive_misses,
                "health-check watchdog: tokio runtime wedged across consecutive probes — \
                 exiting so the supervisor / launchd restarts a fresh process"
            );
            // Flush is best-effort; the wedge is by definition unrecoverable
            // in-process, and a restart is the only cure.
            std::process::exit(WATCHDOG_EXIT_CODE);
        }
    }
}

/// Submit a trivial task onto the runtime and wait up to `timeout` for it to
/// run. Returns `false` only if the runtime could not execute it in time.
fn probe_ok(handle: &tokio::runtime::Handle, timeout: Duration) -> bool {
    let (tx, rx) = mpsc::sync_channel::<()>(1);
    // Spawning on a runtime that is being torn down (clean process shutdown)
    // can panic; catch it so the watchdog thread can never itself crash the
    // process on the exit path. A missing runtime means we're already exiting —
    // report healthy so we don't race a spurious bounce against a clean stop.
    let spawned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle.spawn(async move {
            let _ = tx.send(());
        });
    }));
    if spawned.is_err() {
        return true;
    }
    // Blocks this OS thread (not a runtime thread) until the probe runs or the
    // deadline passes. If the runtime is merely slow, a later-arriving probe
    // just sends on the already-dropped receiver and is discarded — no leak.
    rx.recv_timeout(timeout).is_ok()
}

/// Parse a positive-seconds env value, falling back to `default` for
/// unset/unparsable/zero.
fn parse_secs(raw: Option<&str>, default: Duration) -> Duration {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(default)
}

/// Parse a positive `u32` env value, falling back to `default` for
/// unset/unparsable/zero.
fn parse_u32(raw: Option<&str>, default: u32) -> u32 {
    raw.and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_never_arms_even_with_env_on() {
        assert!(!should_arm(true, false, Some("1")));
        assert!(!should_arm(true, false, None));
    }

    #[test]
    fn explicit_env_override_wins_over_tty() {
        // Forced on even in an interactive terminal.
        assert!(should_arm(false, true, Some("1")));
        assert!(should_arm(false, true, Some("on")));
        assert!(should_arm(false, true, Some(" TRUE ")));
        // Forced off even when headless.
        assert!(!should_arm(false, false, Some("0")));
        assert!(!should_arm(false, false, Some("off")));
    }

    #[test]
    fn default_is_on_headless_off_interactive() {
        assert!(should_arm(false, false, None)); // headless -> on
        assert!(!should_arm(false, true, None)); // TTY -> off
                                                 // An unrecognized override falls through to the TTY default.
        assert!(should_arm(false, false, Some("maybe")));
    }

    #[test]
    fn parse_secs_falls_back_on_bad_or_zero() {
        let d = Duration::from_secs(30);
        assert_eq!(parse_secs(Some("45"), d), Duration::from_secs(45));
        assert_eq!(parse_secs(Some("0"), d), d); // zero is nonsensical -> default
        assert_eq!(parse_secs(Some("nope"), d), d);
        assert_eq!(parse_secs(None, d), d);
    }

    #[test]
    fn parse_u32_falls_back_on_bad_or_zero() {
        assert_eq!(parse_u32(Some("3"), 2), 3);
        assert_eq!(parse_u32(Some("0"), 2), 2); // must fail at least once
        assert_eq!(parse_u32(Some(""), 2), 2);
        assert_eq!(parse_u32(None, 2), 2);
    }

    #[test]
    fn default_config_is_conservative() {
        let c = HealthConfig::default();
        // A wedge must persist well over a minute before a bounce.
        let worst = (c.probe_interval + c.timeout) * c.failures_before_exit;
        assert!(worst >= Duration::from_secs(60));
    }

    #[test]
    fn probe_reports_healthy_on_a_live_runtime() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build test runtime");
        // A healthy runtime runs the probe well within the deadline.
        assert!(probe_ok(&rt.handle().clone(), Duration::from_secs(5)));
    }
}
