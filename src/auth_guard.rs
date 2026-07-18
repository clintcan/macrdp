//! Connection-level auth hardening (Tier 1.2 of the production-readiness
//! roadmap): per-source-IP **rate-limiting**, escalating auto-expiring
//! **failed-attempt lockout**, and a greppable **auth audit log**.
//!
//! This sits in front of macrdp's existing NLA/CredSSP pre-auth gate. It does
//! NOT replace authentication — it bounds how fast and how often a remote peer
//! may *attempt* to connect, and records who connected from where.
//!
//! ## Design
//! - [`AuthGuardCore`] is a **pure, platform-independent** decision core
//!   (`IpAddr`/`Instant`/`Duration` only — no macOS deps), so it unit-tests on
//!   Linux CI exactly like [`crate::reaper`]. It holds per-IP state and answers
//!   two questions: [`AuthGuardCore::decide`] (accept this attempt?) and
//!   [`AuthGuardCore::record_outcome`] (was the finished connection a success or
//!   a failure?). Time is passed in as `now: Instant` so tests time-travel
//!   deterministically.
//! - [`AuthGuardHandler`] is the thin `ironrdp_server::ConnectionHandler` adapter
//!   that wires [`AuthGuardCore`] into the server's per-connection
//!   pre-handshake / post-disconnect seam.
//!
//! ## Heuristic lockout (deliberate)
//! `on_disconnected` only sees `Option<&anyhow::Error>`, which can't cleanly
//! distinguish a wrong-password CredSSP failure from a benign disconnect. Rather
//! than carry a vendored-server signal, we classify by a **fail-fast heuristic**
//! (see [`classify_outcome`]): only a connection that errored *and* ended within
//! the fail-fast window — i.e. never got past the handshake, the brute-force
//! signature — counts as a `Failure`. Any connection that ran longer (even if it
//! later errored) is a `Success` that resets the counter, because it authenticated
//! and is a legitimate client with session trouble, not a login attack. This is
//! forgiving by design: a reconnecting real client (e.g. the mstsc reconnect-blank
//! or a flaky link, which errors a few seconds *into* a session) is never locked
//! out — the pre-fix `errored || short` rule was too aggressive and locked out
//! legit clients (observed in a soak 2026-07-01).
//!
//! ## Scope
//! Loopback (`127.0.0.1`/`::1`, incl. IPv4-mapped) is **exempt** (the default
//! `BIND` is `127.0.0.1:3390`, so the operator can never lock themselves out
//! locally). The auxiliary **UDP multitransport** listener is out of scope: a UDP
//! peer only binds *after* a full TCP+TLS+cookie-bound session already exists, so
//! it is unreachable without first passing this guarded TCP accept.
//!
//! Everything is **on by default** with conservative thresholds, tunable via
//! `MACRDP_*` env vars and fully disable-able (see [`GuardConfig::from_env`]).

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Hard cap on distinct tracked IPs, to bound memory under a spoofed-source-IP
/// flood. When exceeded, the entries with the oldest `last_touch` are evicted.
const MAX_TRACKED_IPS: usize = 50_000;

/// Interpret an on/off env toggle by *value*, not mere presence. `true` unless
/// set to a falsey spelling. Mirrors `crate::multitransport::env_truthy`; kept
/// local so this module stays self-contained and platform-independent.
fn env_on(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
        Err(_) => default,
    }
}

/// Parse a `u64` env var, falling back to `default` on unset/garbage. `0` is a
/// legal value (it disables the corresponding lever), so it is NOT filtered out.
fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

/// Canonicalize an IP for keying + loopback checks: fold IPv4-mapped IPv6
/// (`::ffff:a.b.c.d`) down to its IPv4 form so a mapped address isn't tracked
/// under two keys and a mapped loopback is still recognized as loopback.
fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

/// Resolved thresholds (read once at startup from the environment).
#[derive(Debug, Clone, Copy)]
pub struct GuardConfig {
    /// Sliding window over which connection attempts are counted.
    window: Duration,
    /// Max attempts per `window` per IP before rate-limiting kicks in.
    /// `0` disables rate-limiting.
    max_attempts: usize,
    /// Consecutive failures before the first lockout. `0` disables lockout.
    failure_threshold: u32,
    /// Fail-fast window: an errored connection counts as a lockout **failure**
    /// only if it ended *within* this window — i.e. it never got past the
    /// TLS/CredSSP handshake, the brute-force / port-scan signature. A connection
    /// that ran *longer* than this (even if it later errored) is assumed to have
    /// authenticated — a legitimate client with session trouble — and resets the
    /// counter instead of accruing toward a lockout. Keep it short (a few
    /// seconds), just past the handshake.
    failfast_window: Duration,
    /// First lockout length; doubles per failure past the threshold.
    base_cooldown: Duration,
    /// Cap on the escalated cooldown.
    max_cooldown: Duration,
}

impl GuardConfig {
    /// Resolve thresholds from `MACRDP_GUARD_*` env vars, falling back to the
    /// conservative defaults documented in `docs/cli.md` / `config.env.example`.
    pub fn from_env() -> Self {
        Self {
            window: Duration::from_secs(env_u64("MACRDP_GUARD_RL_WINDOW_SECS", 60)),
            max_attempts: env_u64("MACRDP_GUARD_RL_MAX", 10) as usize,
            failure_threshold: env_u64("MACRDP_GUARD_FAIL_THRESHOLD", 5) as u32,
            failfast_window: Duration::from_secs(env_u64("MACRDP_GUARD_FAILFAST_SECS", 3)),
            base_cooldown: Duration::from_secs(env_u64("MACRDP_GUARD_COOLDOWN_BASE_SECS", 30)),
            max_cooldown: Duration::from_secs(env_u64("MACRDP_GUARD_COOLDOWN_MAX_SECS", 900)),
        }
    }

    fn rate_limit_enabled(&self) -> bool {
        self.max_attempts > 0
    }

    fn lockout_enabled(&self) -> bool {
        self.failure_threshold > 0
    }
}

/// Per-IP tracking state.
#[derive(Debug)]
struct PerIp {
    /// Attempt timestamps within the sliding window (oldest first).
    attempts: VecDeque<Instant>,
    /// Consecutive failures; reset to 0 on a successful outcome.
    consecutive_failures: u32,
    /// Active lockout expiry, if any (auto-expires when `now >= cooldown_until`).
    cooldown_until: Option<Instant>,
    /// Last time this entry was touched, for stale-entry eviction.
    last_touch: Instant,
}

impl PerIp {
    fn new(now: Instant) -> Self {
        Self {
            attempts: VecDeque::new(),
            consecutive_failures: 0,
            cooldown_until: None,
            last_touch: now,
        }
    }

    /// True once this entry carries no useful state and may be evicted.
    fn is_idle(&self, now: Instant) -> bool {
        self.attempts.is_empty()
            && self.consecutive_failures == 0
            && self.cooldown_until.is_none_or(|t| now >= t)
    }
}

/// The decision returned by [`AuthGuardCore::decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Allow the connection to proceed to the handshake.
    Accept,
    /// Reject: too many attempts within the sliding window.
    RejectRateLimit { window_attempts: usize },
    /// Reject: the IP is in an active lockout cooldown.
    RejectCooldown { retry_after: Duration },
}

/// The classified result of a finished connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failure,
}

/// Classify a finished connection for lockout accounting.
///
/// A [`Outcome::Failure`] (which accrues toward a lockout) is recorded **only**
/// when the connection both errored *and* ended within `failfast_window` — i.e.
/// it never got past the TLS/CredSSP handshake, the brute-force / port-scan
/// signature. Anything else is a [`Outcome::Success`] that resets the counter:
/// a clean disconnect, or — crucially — a connection that ran *longer* than the
/// window before erroring, which is a legitimate client that authenticated and
/// then hit session trouble (e.g. the mstsc reconnect-blank or a flaky link),
/// not a login attack. This is what stops a reconnecting real client from being
/// locked out (the pre-fix heuristic counted any errored connection as a
/// failure, which locked out legit clients — observed in a soak 2026-07-01).
///
/// `errored` is the error signal from the connection's `on_disconnected`
/// (`Some(err)` ⇒ the connection ended with an error).
pub fn classify_outcome(errored: bool, duration: Duration, failfast_window: Duration) -> Outcome {
    if errored && duration < failfast_window {
        Outcome::Failure
    } else {
        Outcome::Success
    }
}

/// Pure per-IP rate-limit + lockout decision core. See the module docs.
pub struct AuthGuardCore {
    ips: HashMap<IpAddr, PerIp>,
    cfg: GuardConfig,
}

impl AuthGuardCore {
    /// Build a core from the environment, or `None` if the master switch
    /// (`MACRDP_CONN_GUARD`) is off — callers then skip the guard entirely
    /// (zero overhead, vendored default accept-all path).
    pub fn from_env() -> Option<Self> {
        if !env_on("MACRDP_CONN_GUARD", true) {
            return None;
        }
        Some(Self::with_config(GuardConfig::from_env()))
    }

    fn with_config(cfg: GuardConfig) -> Self {
        Self {
            ips: HashMap::new(),
            cfg,
        }
    }

    /// The fail-fast window used to classify outcomes (see [`classify_outcome`]).
    pub fn failfast_window(&self) -> Duration {
        self.cfg.failfast_window
    }

    /// Decide whether to accept a fresh attempt from `ip` at `now`.
    pub fn decide(&mut self, now: Instant, ip: IpAddr) -> Decision {
        let ip = canonical_ip(ip);
        if ip.is_loopback() {
            return Decision::Accept;
        }

        self.evict_stale(now);

        let window = self.cfg.window;
        let cfg = self.cfg;
        let entry = self.ips.entry(ip).or_insert_with(|| PerIp::new(now));
        entry.last_touch = now;

        // Drop attempts that have aged out of the sliding window.
        let cutoff = now.checked_sub(window);
        while let Some(&front) = entry.attempts.front() {
            match cutoff {
                Some(c) if front < c => {
                    entry.attempts.pop_front();
                }
                _ => break,
            }
        }

        // Active lockout?
        if cfg.lockout_enabled() {
            if let Some(until) = entry.cooldown_until {
                if now < until {
                    return Decision::RejectCooldown {
                        retry_after: until.saturating_duration_since(now),
                    };
                }
                // Expired — clear it so a later success/failure starts fresh.
                entry.cooldown_until = None;
            }
        }

        // Rate-limit: a rejected attempt is NOT pushed, so a reject can't
        // self-extend the window.
        if cfg.rate_limit_enabled() && entry.attempts.len() >= cfg.max_attempts {
            return Decision::RejectRateLimit {
                window_attempts: entry.attempts.len(),
            };
        }

        entry.attempts.push_back(now);
        Decision::Accept
    }

    /// Record the classified outcome of a finished connection from `ip`.
    pub fn record_outcome(&mut self, now: Instant, ip: IpAddr, outcome: Outcome) {
        let ip = canonical_ip(ip);
        if ip.is_loopback() {
            return;
        }
        let cfg = self.cfg;
        let entry = self.ips.entry(ip).or_insert_with(|| PerIp::new(now));
        entry.last_touch = now;

        match outcome {
            Outcome::Success => {
                entry.consecutive_failures = 0;
                entry.cooldown_until = None;
            }
            Outcome::Failure => {
                entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
                if cfg.lockout_enabled() && entry.consecutive_failures >= cfg.failure_threshold {
                    let cooldown = escalated_cooldown(
                        entry.consecutive_failures,
                        cfg.failure_threshold,
                        cfg.base_cooldown,
                        cfg.max_cooldown,
                    );
                    entry.cooldown_until = Some(now + cooldown);
                }
            }
        }
    }

    /// Lazily drop fully-idle entries, and hard-cap total tracked IPs.
    fn evict_stale(&mut self, now: Instant) {
        // Prune every entry's window first (the per-decision prune only touches
        // the IP being decided), so an IP whose attempts have all aged out — and
        // which carries no failure/cooldown state — becomes idle and is dropped.
        let cutoff = now.checked_sub(self.cfg.window);
        self.ips.retain(|_, v| {
            if let Some(c) = cutoff {
                while let Some(&front) = v.attempts.front() {
                    if front < c {
                        v.attempts.pop_front();
                    } else {
                        break;
                    }
                }
            }
            !v.is_idle(now)
        });

        if self.ips.len() >= MAX_TRACKED_IPS {
            // Evict ~10% of the oldest-touched entries so this doesn't run every
            // call once at the cap.
            let to_drop = self.ips.len() / 10 + 1;
            let mut by_age: Vec<(IpAddr, Instant)> =
                self.ips.iter().map(|(k, v)| (*k, v.last_touch)).collect();
            by_age.sort_by_key(|(_, t)| *t);
            for (ip, _) in by_age.into_iter().take(to_drop) {
                self.ips.remove(&ip);
            }
        }
    }

    #[cfg(test)]
    fn tracks(&self, ip: IpAddr) -> bool {
        self.ips.contains_key(&canonical_ip(ip))
    }
}

/// `base << (n - threshold)`, saturating, capped at `max`.
fn escalated_cooldown(n: u32, threshold: u32, base: Duration, max: Duration) -> Duration {
    let steps = n.saturating_sub(threshold);
    // Saturate the shift so a huge failure count doesn't overflow.
    let factor = 1u64.checked_shl(steps).unwrap_or(u64::MAX);
    let scaled = base
        .checked_mul(factor.min(u32::MAX as u64) as u32)
        .unwrap_or(max);
    scaled.min(max)
}

// ---------------------------------------------------------------------------
// Audit log
// ---------------------------------------------------------------------------

/// Version of the audit-event **field schema** (the `macrdp::audit` records). It
/// is stamped on every audit line so a SIEM/collector can pin a stable contract;
/// **bump it only on a breaking field change** (a rename/removal/semantic shift),
/// not for additive fields. See `docs/siem-forwarding.md`.
pub const AUDIT_SCHEMA_VERSION: u32 = 1;

/// The host's name, cached once, for the audit records (so the JSON stream is
/// self-describing even before a collector adds its own host field). Best-effort:
/// `"unknown"` if `gethostname` fails. Cross-platform (POSIX `gethostname`).
fn host() -> &'static str {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        // `c_char` is `i8` on x86_64 but `u8` on aarch64 (Linux) — use the alias
        // so this compiles on every target, not just the CI host.
        let mut buf = [0 as libc::c_char; 256];
        // SAFETY: `buf` is a valid, correctly-sized writable buffer; gethostname
        // writes at most `len` bytes and NUL-terminates on success.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) };
        if rc != 0 {
            return "unknown".to_string();
        }
        // Read up to the first NUL. `c as u8` is an identity on u8 targets and a
        // bit-reinterpret on i8 targets — correct for the raw byte either way.
        let bytes: Vec<u8> = buf
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    })
    .as_str()
}

/// Whether the audit log is enabled (`MACRDP_AUDIT_LOG`, default on). Independent
/// of the guard so audit lines flow even with enforcement disabled.
pub fn audit_enabled() -> bool {
    env_on("MACRDP_AUDIT_LOG", true)
}

/// Audit-log an accepted connection.
pub fn audit_accept(ip: IpAddr, port: u16) {
    if audit_enabled() {
        tracing::info!(
            target: "macrdp::audit",
            schema_version = AUDIT_SCHEMA_VERSION,
            macrdp_version = env!("CARGO_PKG_VERSION"),
            host = host(),
            event = "accept",
            src_ip = %ip,
            src_port = port,
        );
    }
}

/// Audit-log a rejected connection (the `Decision` carries the reason).
pub fn audit_reject(ip: IpAddr, decision: Decision) {
    if !audit_enabled() {
        return;
    }
    match decision {
        Decision::RejectRateLimit { window_attempts } => {
            tracing::warn!(
                target: "macrdp::audit",
                schema_version = AUDIT_SCHEMA_VERSION,
                macrdp_version = env!("CARGO_PKG_VERSION"),
                host = host(),
                event = "reject",
                reason = "rate_limit",
                src_ip = %ip,
                window_attempts,
            );
        }
        Decision::RejectCooldown { retry_after } => {
            tracing::warn!(
                target: "macrdp::audit",
                schema_version = AUDIT_SCHEMA_VERSION,
                macrdp_version = env!("CARGO_PKG_VERSION"),
                host = host(),
                event = "reject",
                reason = "lockout",
                src_ip = %ip,
                retry_after_secs = retry_after.as_secs(),
            );
        }
        Decision::Accept => {}
    }
}

/// Audit-log a finished connection and its classified outcome. `port` is the
/// peer's source port, carried so a collector can correlate this `disconnect`
/// with its matching `accept` on the `(src_ip, src_port)` tuple.
pub fn audit_disconnect(ip: IpAddr, port: u16, duration: Duration, outcome: Outcome) {
    if audit_enabled() {
        tracing::info!(
            target: "macrdp::audit",
            schema_version = AUDIT_SCHEMA_VERSION,
            macrdp_version = env!("CARGO_PKG_VERSION"),
            host = host(),
            event = "disconnect",
            src_ip = %ip,
            src_port = port,
            duration_ms = duration.as_millis() as u64,
            outcome = match outcome {
                Outcome::Success => "success",
                Outcome::Failure => "failure",
            },
        );
    }
}

/// Cap on the auth-failure `reason` length (chars). The reason is an sspi error
/// description (error kind + a generic message like "logon denied") — never the
/// client's NTLM response or password — but bound it as defense-in-depth against
/// an unexpectedly verbose error chain landing in the audit stream.
const MAX_AUDIT_REASON: usize = 200;

/// Sanitize (and default/bound) the auth-failure reason for the audit record.
/// The reason derives from `accept_credssp`'s error string — i.e. content
/// produced while handling an *unauthenticated* client's CredSSP/NTLM exchange —
/// so it must be treated as untrusted for the log sink. Two hardening steps:
///  - **Strip control characters** (newlines, CR, tab, ANSI ESC, …). The JSON
///    audit sink escapes them (serde), but the human-readable logfmt `macrdp.log`
///    sink writes a display-recorded field verbatim, so an embedded `\n` could
///    forge/split an audit line (log injection into a security log) and an ESC
///    could smuggle ANSI into a terminal viewing it. Replace each with a space so
///    the field stays a single printable token in both sinks.
///  - **Cap the length** at [`MAX_AUDIT_REASON`] chars (char boundary, never
///    mid-UTF-8) against an unexpectedly verbose error chain.
///
/// `None` becomes `"unknown"`. Returns borrowed when the input is already clean
/// and short (the common case), owned only when it had to be rewritten.
fn bound_reason(reason: Option<&str>) -> std::borrow::Cow<'_, str> {
    let r = match reason {
        None => return std::borrow::Cow::Borrowed("unknown"),
        Some(r) => r,
    };
    // Fast path: already a single short printable token — borrow it unchanged.
    if r.chars().take(MAX_AUDIT_REASON + 1).count() <= MAX_AUDIT_REASON
        && !r.chars().any(|c| c.is_control())
    {
        return std::borrow::Cow::Borrowed(r);
    }
    std::borrow::Cow::Owned(
        r.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(MAX_AUDIT_REASON)
            .collect(),
    )
}

/// Audit-log the CredSSP/NLA authentication verdict for a connection, emitted once
/// per connection when the exchange resolves. `success` is whether the client's
/// credentials validated; `reason` is a short failure description (only used when
/// `success` is false — auth did not complete). Correlates with the preceding
/// `accept` on the `(src_ip, src_port)` tuple.
pub fn audit_auth(ip: IpAddr, port: u16, success: bool, reason: Option<&str>) {
    if !audit_enabled() {
        return;
    }
    if success {
        tracing::info!(
            target: "macrdp::audit",
            schema_version = AUDIT_SCHEMA_VERSION,
            macrdp_version = env!("CARGO_PKG_VERSION"),
            host = host(),
            event = "auth",
            src_ip = %ip,
            src_port = port,
            outcome = "success",
        );
    } else {
        // "did_not_complete" (not "failure"): an Err is dominated by bad
        // credentials but also covers a client aborting the credential dialog or
        // a rare mid-CredSSP transport error — the `reason` disambiguates.
        tracing::warn!(
            target: "macrdp::audit",
            schema_version = AUDIT_SCHEMA_VERSION,
            macrdp_version = env!("CARGO_PKG_VERSION"),
            host = host(),
            event = "auth",
            src_ip = %ip,
            src_port = port,
            outcome = "did_not_complete",
            reason = %bound_reason(reason),
        );
    }
}

/// Audit-log the client fingerprint for a connection, emitted once per
/// connection when the capability exchange completes (after `auth`). The
/// identity fields come from the client's GCC Client Core Data + General capset
/// and are **informational fingerprinting only** — a client can claim anything.
/// Known signatures (see `docs/audit-log.md`): mstsc = the real Windows build
/// (e.g. 26100 = Win11 24H2) + platform `WINDOWS/WINDOWS_NT`; FreeRDP family =
/// build 2600; Thincast = build 18363 + platform `UNSPECIFIED/UNSPECIFIED`.
/// `client_name` is client-controlled → control-char-stripped + length-bounded
/// (same log-injection defense as the auth `reason`). `platform` is
/// server-formatted (safe).
pub fn audit_fingerprint(
    ip: IpAddr,
    port: u16,
    client_name: &str,
    rdp_version: u32,
    client_build: u32,
    platform: &str,
) {
    if !audit_enabled() {
        return;
    }
    tracing::info!(
        target: "macrdp::audit",
        schema_version = AUDIT_SCHEMA_VERSION,
        macrdp_version = env!("CARGO_PKG_VERSION"),
        host = host(),
        event = "fingerprint",
        src_ip = %ip,
        src_port = port,
        client_name = %bound_reason(Some(client_name)),
        rdp_version = format_args!("{rdp_version:#x}"),
        client_build,
        platform = %platform,
    );
}

// ---------------------------------------------------------------------------
// Single-process ConnectionHandler adapter
// ---------------------------------------------------------------------------

/// `ironrdp_server::ConnectionHandler` adapter wrapping an [`AuthGuardCore`], for
/// the single-process server path. Constructed via [`AuthGuardHandler::from_env`].
pub struct AuthGuardHandler {
    core: AuthGuardCore,
    /// The most recently accepted peer, stashed in `on_accept` so `on_authenticated`
    /// (whose vendored hook takes no peer) can correlate the auth event. Reliable
    /// because the single-process accept loop is serial — `on_accept` always runs
    /// immediately before the connection it belongs to.
    last_peer: Option<std::net::SocketAddr>,
}

impl AuthGuardHandler {
    /// Build the boxed handler, or `None` when the guard is disabled (so the
    /// builder gets `None` and the vendored default accept-all path runs).
    pub fn from_env() -> Option<Box<dyn ironrdp_server::ConnectionHandler>> {
        AuthGuardCore::from_env().map(|core| {
            Box::new(Self {
                core,
                last_peer: None,
            }) as Box<dyn ironrdp_server::ConnectionHandler>
        })
    }
}

impl ironrdp_server::ConnectionHandler for AuthGuardHandler {
    fn on_accept(&mut self, peer: std::net::SocketAddr) -> bool {
        self.last_peer = Some(peer);
        match self.core.decide(Instant::now(), peer.ip()) {
            Decision::Accept => {
                audit_accept(peer.ip(), peer.port());
                true
            }
            reject => {
                audit_reject(peer.ip(), reject);
                false
            }
        }
    }

    fn on_authenticated(&mut self, success: bool, reason: Option<&str>) {
        // `last_peer` is always set here in single-process operation (on_accept
        // precedes the connection); guard defensively rather than assume it.
        if let Some(peer) = self.last_peer {
            audit_auth(peer.ip(), peer.port(), success, reason);
        }
    }

    fn on_client_fingerprint(
        &mut self,
        client_name: &str,
        rdp_version: u32,
        client_build: u32,
        platform: &str,
    ) {
        if let Some(peer) = self.last_peer {
            audit_fingerprint(
                peer.ip(),
                peer.port(),
                client_name,
                rdp_version,
                client_build,
                platform,
            );
        }
    }

    fn on_disconnected(
        &mut self,
        peer: std::net::SocketAddr,
        duration: Duration,
        error: Option<&anyhow::Error>,
    ) -> ironrdp_server::PostConnectionAction {
        let outcome = classify_outcome(error.is_some(), duration, self.core.failfast_window());
        self.core.record_outcome(Instant::now(), peer.ip(), outcome);
        audit_disconnect(peer.ip(), peer.port(), duration, outcome);
        // The guard must never halt the server.
        ironrdp_server::PostConnectionAction::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn test_cfg() -> GuardConfig {
        GuardConfig {
            window: Duration::from_secs(60),
            max_attempts: 10,
            failure_threshold: 5,
            failfast_window: Duration::from_secs(3),
            base_cooldown: Duration::from_secs(30),
            max_cooldown: Duration::from_secs(900),
        }
    }

    #[test]
    fn classify_outcome_only_counts_fast_errored_connections() {
        let w = Duration::from_secs(3);
        // Fast + errored = the brute-force/scan signature → Failure.
        assert_eq!(
            classify_outcome(true, Duration::from_millis(1), w),
            Outcome::Failure
        );
        assert_eq!(
            classify_outcome(true, Duration::from_millis(900), w),
            Outcome::Failure
        );
        // Errored but ran past the window = a client that authenticated then hit
        // session trouble (the soak false-lockout case) → Success (resets).
        assert_eq!(
            classify_outcome(true, Duration::from_secs(7), w),
            Outcome::Success
        );
        assert_eq!(
            classify_outcome(true, Duration::from_millis(3001), w),
            Outcome::Success
        );
        // Clean disconnects never count, short or long.
        assert_eq!(
            classify_outcome(false, Duration::from_millis(1), w),
            Outcome::Success
        );
        assert_eq!(
            classify_outcome(false, Duration::from_secs(3600), w),
            Outcome::Success
        );
    }

    #[test]
    fn reconnecting_client_with_session_errors_is_not_locked_out() {
        // Regression for the 2026-07-01 soak: a legit client whose sessions
        // establish then error at ~7s must NOT accrue toward a lockout.
        let mut c = core();
        let t0 = Instant::now();
        let peer = ip(20);
        let w = c.failfast_window();
        for k in 0..20 {
            let dur = Duration::from_secs(7); // errored after authenticating
            c.record_outcome(
                t0 + Duration::from_secs(k * 10),
                peer,
                classify_outcome(true, dur, w),
            );
            assert_eq!(
                c.decide(t0 + Duration::from_secs(k * 10 + 1), peer),
                Decision::Accept,
                "iteration {k}: a reconnecting client must stay accepted"
            );
        }
    }

    fn core() -> AuthGuardCore {
        AuthGuardCore::with_config(test_cfg())
    }

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, n))
    }

    #[test]
    fn rate_limit_trips_at_cap_then_recovers_after_window() {
        let mut c = core();
        let t0 = Instant::now();
        let peer = ip(1);
        // First 10 attempts accepted.
        for i in 0..10 {
            assert_eq!(
                c.decide(t0 + Duration::from_secs(i), peer),
                Decision::Accept,
                "attempt {i} should be accepted"
            );
        }
        // 11th within the window is rate-limited.
        assert_eq!(
            c.decide(t0 + Duration::from_secs(10), peer),
            Decision::RejectRateLimit {
                window_attempts: 10
            }
        );
        // After the window has fully elapsed, accepted again.
        assert_eq!(
            c.decide(t0 + Duration::from_secs(61), peer),
            Decision::Accept
        );
    }

    #[test]
    fn rejected_attempt_not_counted_in_window() {
        let mut c = core();
        let t0 = Instant::now();
        let peer = ip(2);
        // 10 accepts spread 1s apart (t0..t0+9) so timestamps age out one at a time.
        for i in 0..10 {
            assert_eq!(
                c.decide(t0 + Duration::from_secs(i), peer),
                Decision::Accept
            );
        }
        // Several rejects at t0+9 — if any of these pushed a timestamp the window
        // would hold 11+ entries and the recovery check below would still reject.
        for _ in 0..5 {
            assert!(matches!(
                c.decide(t0 + Duration::from_secs(9), peer),
                Decision::RejectRateLimit { .. }
            ));
        }
        // At t0+61 only the t0 attempt has aged out (t0 < t0+1), freeing exactly
        // ONE slot → exactly one accept, then reject again. This only holds if the
        // rejects above were NOT counted.
        assert_eq!(
            c.decide(t0 + Duration::from_secs(61), peer),
            Decision::Accept
        );
        assert!(matches!(
            c.decide(t0 + Duration::from_secs(61), peer),
            Decision::RejectRateLimit { .. }
        ));
    }

    #[test]
    fn cooldown_escalates_and_auto_expires() {
        let mut c = core();
        let t0 = Instant::now();
        let peer = ip(3);
        // 5 consecutive failures → first lockout (base = 30s).
        for _ in 0..5 {
            c.record_outcome(t0, peer, Outcome::Failure);
        }
        match c.decide(t0, peer) {
            Decision::RejectCooldown { retry_after } => {
                assert_eq!(retry_after, Duration::from_secs(30));
            }
            other => panic!("expected cooldown, got {other:?}"),
        }
        // One more failure → escalate to 60s (measured from the new failure time).
        c.record_outcome(t0, peer, Outcome::Failure);
        match c.decide(t0, peer) {
            Decision::RejectCooldown { retry_after } => {
                assert_eq!(retry_after, Duration::from_secs(60));
            }
            other => panic!("expected escalated cooldown, got {other:?}"),
        }
        // After it expires, accepted again.
        assert_eq!(
            c.decide(t0 + Duration::from_secs(61), peer),
            Decision::Accept
        );
    }

    #[test]
    fn cooldown_is_capped() {
        // 5 + 20 extra failures would be base << 20 ≈ 30s * 1M, far over the cap.
        let dur = escalated_cooldown(25, 5, Duration::from_secs(30), Duration::from_secs(900));
        assert_eq!(dur, Duration::from_secs(900));
    }

    #[test]
    fn success_resets_failures() {
        let mut c = core();
        let t0 = Instant::now();
        let peer = ip(4);
        // 4 failures (below threshold), then a success.
        for _ in 0..4 {
            c.record_outcome(t0, peer, Outcome::Failure);
        }
        c.record_outcome(t0, peer, Outcome::Success);
        // 4 more failures still below threshold → no lockout.
        for _ in 0..4 {
            c.record_outcome(t0, peer, Outcome::Failure);
        }
        assert_eq!(c.decide(t0, peer), Decision::Accept);
    }

    #[test]
    fn benign_single_error_then_success_never_locks() {
        let mut c = core();
        let t0 = Instant::now();
        let peer = ip(5);
        // The documented mstsc pattern: one broken-pipe failure, then a clean
        // session, repeated many times. Must never reach lockout.
        for k in 0..50 {
            let t = t0 + Duration::from_secs(k * 20);
            c.record_outcome(t, peer, Outcome::Failure);
            c.record_outcome(t + Duration::from_secs(1), peer, Outcome::Success);
            assert_eq!(
                c.decide(t + Duration::from_secs(2), peer),
                Decision::Accept,
                "iteration {k} must stay accepted"
            );
        }
    }

    #[test]
    fn loopback_is_exempt_and_untracked() {
        let mut c = core();
        let t0 = Instant::now();
        for lo in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            // IPv4-mapped loopback.
            IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped()),
        ] {
            for _ in 0..100 {
                c.record_outcome(t0, lo, Outcome::Failure);
            }
            for _ in 0..100 {
                assert_eq!(c.decide(t0, lo), Decision::Accept);
            }
            assert!(!c.tracks(lo), "loopback must never be tracked: {lo}");
        }
    }

    #[test]
    fn ipv4_mapped_keys_to_v4() {
        let mut c = core();
        let t0 = Instant::now();
        let v4 = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7));
        let mapped = IpAddr::V6(Ipv4Addr::new(198, 51, 100, 7).to_ipv6_mapped());
        // Failures recorded under the mapped form must count toward the V4 key.
        for _ in 0..5 {
            c.record_outcome(t0, mapped, Outcome::Failure);
        }
        assert!(matches!(c.decide(t0, v4), Decision::RejectCooldown { .. }));
    }

    #[test]
    fn stale_entries_are_evicted() {
        let mut c = core();
        let t0 = Instant::now();
        let peer = ip(6);
        assert_eq!(c.decide(t0, peer), Decision::Accept);
        assert!(c.tracks(peer));
        // Decide for a different IP well after the window → the first entry is
        // now idle (attempt aged out, no failures, no cooldown) and evicted.
        let _ = c.decide(t0 + Duration::from_secs(120), ip(7));
        assert!(!c.tracks(peer), "idle entry should have been evicted");
    }

    #[test]
    fn disabled_lockout_threshold_zero() {
        let mut cfg = test_cfg();
        cfg.failure_threshold = 0;
        let mut c = AuthGuardCore::with_config(cfg);
        let t0 = Instant::now();
        let peer = ip(8);
        for _ in 0..50 {
            c.record_outcome(t0, peer, Outcome::Failure);
        }
        assert_eq!(c.decide(t0, peer), Decision::Accept);
    }

    #[test]
    fn disabled_rate_limit_max_zero() {
        let mut cfg = test_cfg();
        cfg.max_attempts = 0;
        let mut c = AuthGuardCore::with_config(cfg);
        let t0 = Instant::now();
        let peer = ip(9);
        for _ in 0..1000 {
            assert_eq!(c.decide(t0, peer), Decision::Accept);
        }
    }

    #[test]
    fn bound_reason_caps_and_defaults() {
        assert_eq!(bound_reason(None), "unknown");
        assert_eq!(bound_reason(Some("logon denied")), "logon denied");
        // Over-long ASCII truncates to the cap.
        let long = "x".repeat(500);
        assert_eq!(bound_reason(Some(&long)).chars().count(), MAX_AUDIT_REASON);
        // Truncation lands on a char boundary (multi-byte input, no panic).
        let multibyte = "é".repeat(500);
        assert_eq!(
            bound_reason(Some(&multibyte)).chars().count(),
            MAX_AUDIT_REASON
        );
    }

    #[test]
    fn bound_reason_length_boundary() {
        // Exactly at the cap: unchanged, still MAX chars.
        let at = "a".repeat(MAX_AUDIT_REASON);
        assert_eq!(bound_reason(Some(&at)), at.as_str());
        // One over: truncated to the cap.
        let over = "a".repeat(MAX_AUDIT_REASON + 1);
        assert_eq!(bound_reason(Some(&over)).chars().count(), MAX_AUDIT_REASON);
        // A control char past the cap is truncated away; the output has none, and
        // truncation never panics on a char boundary.
        let mut long = "a".repeat(MAX_AUDIT_REASON + 50);
        long.push('\n');
        let out = bound_reason(Some(&long));
        assert_eq!(out.chars().count(), MAX_AUDIT_REASON);
        assert!(!out.chars().any(|c| c.is_control()));
    }

    #[test]
    fn bound_reason_strips_control_chars_log_injection() {
        // A newline/CR/tab in the (untrusted) error string must not survive into
        // the log — otherwise it could forge/split an audit line in the logfmt
        // sink. Each control char becomes a space; the field stays single-line.
        assert_eq!(
            bound_reason(Some("logon denied\n2026 INFO forged event=\"auth\"")),
            "logon denied 2026 INFO forged event=\"auth\""
        );
        assert_eq!(bound_reason(Some("a\r\n\tb")), "a   b");
        // ANSI ESC (0x1b) is a control char → neutralized.
        assert_eq!(bound_reason(Some("x\u{1b}[31mred")), "x [31mred");
        // The sanitized result contains no control characters at all.
        let cleaned = bound_reason(Some("m\u{0}i\u{7}x\u{1b}ed\nline"));
        assert!(!cleaned.chars().any(|c| c.is_control()));
    }

    /// A cloneable `MakeWriter` that captures emitted log lines into a shared
    /// buffer, so we can assert the auth audit event's fields.
    #[derive(Clone, Default)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
        type Writer = SharedBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn on_authenticated_emits_auth_audit_event() {
        use ironrdp_server::ConnectionHandler;

        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();

        let peer = std::net::SocketAddr::from((Ipv4Addr::new(203, 0, 113, 5), 51000));
        tracing::subscriber::with_default(subscriber, || {
            let mut handler = AuthGuardHandler {
                core: AuthGuardCore::with_config(test_cfg()),
                last_peer: None,
            };
            // on_accept stashes the peer for on_authenticated to correlate.
            assert!(handler.on_accept(peer));
            handler.on_authenticated(true, None);
            handler.on_authenticated(false, Some("logon denied"));
        });

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        // Success event, correlated to the accepted peer.
        assert!(out.contains("event=\"auth\""), "no auth event:\n{out}");
        assert!(out.contains("outcome=\"success\""), "{out}");
        assert!(out.contains("src_ip=203.0.113.5"), "{out}");
        assert!(out.contains("src_port=51000"), "{out}");
        // Failure event carries the (bounded) reason.
        assert!(out.contains("outcome=\"did_not_complete\""), "{out}");
        assert!(out.contains("reason=logon denied"), "{out}");
    }

    #[test]
    fn on_client_fingerprint_emits_fingerprint_event() {
        use ironrdp_server::ConnectionHandler;

        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();

        let peer = std::net::SocketAddr::from((Ipv4Addr::new(203, 0, 113, 5), 51000));
        tracing::subscriber::with_default(subscriber, || {
            let mut handler = AuthGuardHandler {
                core: AuthGuardCore::with_config(test_cfg()),
                last_peer: None,
            };
            assert!(handler.on_accept(peer));
            // A hostile client name with a control char (log-injection attempt)
            // must come out stripped.
            handler.on_client_fingerprint("GENMACWIN\nevil", 0x80011, 26100, "WINDOWS/WINDOWS_NT");
        });

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            out.contains("event=\"fingerprint\""),
            "no fingerprint event:\n{out}"
        );
        assert!(out.contains("src_ip=203.0.113.5"), "{out}");
        assert!(out.contains("client_build=26100"), "{out}");
        assert!(out.contains("rdp_version=0x80011"), "{out}");
        assert!(out.contains("platform=WINDOWS/WINDOWS_NT"), "{out}");
        // Control char stripped, name still present.
        assert!(
            out.contains("GENMACWIN evil"),
            "control char not stripped:\n{out}"
        );
        assert!(
            !out.contains("GENMACWIN\nevil"),
            "raw control char leaked:\n{out}"
        );
    }

    #[test]
    fn on_authenticated_without_accept_does_not_panic() {
        use ironrdp_server::ConnectionHandler;
        let mut handler = AuthGuardHandler {
            core: AuthGuardCore::with_config(test_cfg()),
            last_peer: None,
        };
        // No preceding on_accept → last_peer is None; must be a graceful no-op.
        handler.on_authenticated(true, None);
        handler.on_authenticated(false, Some("x"));
    }
}
