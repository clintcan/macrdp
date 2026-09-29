//! Reconnect awareness for `--lock-on-disconnect`.
//!
//! The lock fires once no session has been live for the safety buffer. But a
//! session only counts as live once it is FULLY connected, which over a
//! VPN/ZeroTier link takes ~10 s — so a client that reconnects late in the
//! buffer is locked underneath its own handshake (live-observed on #181; with
//! `--auto-unlock` it self-corrects, without it the remote user is stranded).
//!
//! This module notices connection activity that happens AFTER the disconnect
//! and lets the lock wait for it to resolve:
//!
//! - an **authenticated** reconnect (CredSSP succeeded) holds the lock until it
//!   finishes connecting — only someone who knows the password can do that;
//! - a merely **accepted** connection (still in TLS/CredSSP) holds it briefly,
//!   because the timer can end mid-CredSSP too.
//!
//! Every hold is capped by [`MAX_HOLD`] from the moment the lock would have
//! fired, so an unauthenticated peer opening connections can delay the lock
//! by at most that much — never prevent it.
//!
//! Only installed with `--lock-on-disconnect`; the default connection path is
//! unchanged (no wrapper, no timestamps).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ironrdp_server::{ConnectionHandler, PostConnectionAction};

/// Upper bound on how long the lock may be held past its normal firing time.
pub const MAX_HOLD: Duration = Duration::from_secs(30);
/// How long after a successful authentication the client is presumed to be
/// finishing its connection (capability exchange, channel setup). ~10 s was
/// observed over ZeroTier; this leaves headroom.
pub const AUTHED_CONNECT_WINDOW: Duration = Duration::from_secs(30);
/// How long after an accepted connection it may still be in TLS/CredSSP. Kept
/// short: nothing about this connection is authenticated yet.
pub const PREAUTH_WINDOW: Duration = Duration::from_secs(10);

/// Timestamps of the latest accepted connection and the latest successful
/// authentication, as milliseconds since `epoch` (0 = never).
pub struct ConnectionActivity {
    epoch: Instant,
    last_accept_ms: AtomicU64,
    last_auth_ms: AtomicU64,
}

impl Default for ConnectionActivity {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
            last_accept_ms: AtomicU64::new(0),
            last_auth_ms: AtomicU64::new(0),
        }
    }
}

impl ConnectionActivity {
    /// Milliseconds since `epoch`, never 0 (0 means "never").
    pub fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis())
            .unwrap_or(u64::MAX)
            .max(1)
    }

    fn mark(cell: &AtomicU64, at: u64) {
        cell.fetch_max(at, Ordering::SeqCst);
    }

    /// Time since the latest accept that happened after `since_ms`, if any.
    fn accept_age(&self, since_ms: u64, now_ms: u64) -> Option<Duration> {
        age(self.last_accept_ms.load(Ordering::SeqCst), since_ms, now_ms)
    }

    /// Time since the latest successful auth that happened after `since_ms`.
    fn auth_age(&self, since_ms: u64, now_ms: u64) -> Option<Duration> {
        age(self.last_auth_ms.load(Ordering::SeqCst), since_ms, now_ms)
    }

    /// Decide what the pending lock should do now. `disconnect_ms` is when the
    /// disconnect that armed it was observed; `due_ms` is when it would have
    /// fired without any hold.
    pub fn decide(&self, session_live: bool, disconnect_ms: u64, due_ms: u64) -> LockDecision {
        let now = self.now_ms();
        decide(
            session_live,
            self.accept_age(disconnect_ms, now),
            self.auth_age(disconnect_ms, now),
            Duration::from_millis(now.saturating_sub(due_ms)),
        )
    }
}

fn age(at_ms: u64, since_ms: u64, now_ms: u64) -> Option<Duration> {
    (at_ms != 0 && at_ms > since_ms).then(|| Duration::from_millis(now_ms.saturating_sub(at_ms)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockDecision {
    /// A session is live again — don't lock.
    Skip,
    /// A reconnect is in progress — check again shortly.
    Hold,
    /// Lock now.
    Lock,
}

/// The pure policy. Ages are measured from the latest event of each kind
/// since the disconnect (`None` = none since); `held` is how long past the
/// normal firing time we already are.
pub fn decide(
    session_live: bool,
    accept_age: Option<Duration>,
    auth_age: Option<Duration>,
    held: Duration,
) -> LockDecision {
    if session_live {
        return LockDecision::Skip;
    }
    if held >= MAX_HOLD {
        return LockDecision::Lock;
    }
    let reconnecting = auth_age.is_some_and(|a| a < AUTHED_CONNECT_WINDOW)
        || accept_age.is_some_and(|a| a < PREAUTH_WINDOW);
    if reconnecting {
        LockDecision::Hold
    } else {
        LockDecision::Lock
    }
}

/// Wraps the server's connection handler (or none) to record activity for
/// [`ConnectionActivity`], forwarding every hook unchanged.
pub struct ActivityHandler {
    inner: Option<Box<dyn ConnectionHandler>>,
    activity: std::sync::Arc<ConnectionActivity>,
}

impl ActivityHandler {
    pub fn wrap(
        inner: Option<Box<dyn ConnectionHandler>>,
        activity: std::sync::Arc<ConnectionActivity>,
    ) -> Box<dyn ConnectionHandler> {
        Box::new(Self { inner, activity })
    }
}

impl ConnectionHandler for ActivityHandler {
    fn on_accept(&mut self, peer: SocketAddr) -> bool {
        let accepted = self.inner.as_mut().is_none_or(|h| h.on_accept(peer));
        // Only connections the inner handler let through count: one the auth
        // guard rejected (rate limit / lockout) is dropped at once.
        if accepted {
            ConnectionActivity::mark(&self.activity.last_accept_ms, self.activity.now_ms());
        }
        accepted
    }

    fn on_disconnected(
        &mut self,
        peer: SocketAddr,
        duration: Duration,
        error: Option<&anyhow::Error>,
    ) -> PostConnectionAction {
        self.inner
            .as_mut()
            .map_or(PostConnectionAction::Continue, |h| {
                h.on_disconnected(peer, duration, error)
            })
    }

    fn on_authenticated(&mut self, success: bool, reason: Option<&str>) {
        if success {
            ConnectionActivity::mark(&self.activity.last_auth_ms, self.activity.now_ms());
        }
        if let Some(h) = self.inner.as_mut() {
            h.on_authenticated(success, reason);
        }
    }

    fn on_client_fingerprint(
        &mut self,
        client_name: &str,
        rdp_version: u32,
        client_build: u32,
        platform: &str,
    ) {
        if let Some(h) = self.inner.as_mut() {
            h.on_client_fingerprint(client_name, rdp_version, client_build, platform);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const S: fn(u64) -> Duration = Duration::from_secs;

    #[test]
    fn a_live_session_always_skips_the_lock() {
        assert_eq!(decide(true, None, None, S(0)), LockDecision::Skip);
        assert_eq!(decide(true, None, None, MAX_HOLD), LockDecision::Skip);
    }

    #[test]
    fn no_activity_since_the_disconnect_locks_on_time() {
        assert_eq!(decide(false, None, None, S(0)), LockDecision::Lock);
    }

    /// The #181 follow-up: an authenticated reconnect still finishing its
    /// connection holds the lock.
    #[test]
    fn an_authenticated_reconnect_holds_the_lock() {
        assert_eq!(
            decide(false, Some(S(8)), Some(S(6)), S(0)),
            LockDecision::Hold
        );
        assert_eq!(decide(false, None, Some(S(29)), S(5)), LockDecision::Hold);
    }

    /// The timer can also end while the client is still in TLS/CredSSP.
    #[test]
    fn an_accepted_connection_holds_the_lock_briefly() {
        assert_eq!(decide(false, Some(S(2)), None, S(0)), LockDecision::Hold);
        assert_eq!(
            decide(false, Some(PREAUTH_WINDOW), None, S(0)),
            LockDecision::Lock
        );
    }

    /// An authenticated connection that then never finishes (the client gave
    /// up) doesn't hold the lock for ever.
    #[test]
    fn a_stale_authentication_no_longer_holds() {
        assert_eq!(
            decide(false, None, Some(AUTHED_CONNECT_WINDOW), S(0)),
            LockDecision::Lock
        );
    }

    /// Security bound: however fresh the activity, the lock fires once the
    /// hold reaches MAX_HOLD — a peer opening connections can only delay it.
    #[test]
    fn the_hold_is_capped() {
        assert_eq!(
            decide(false, Some(S(0)), Some(S(0)), MAX_HOLD),
            LockDecision::Lock
        );
    }

    /// Activity from BEFORE the disconnect (the departing session's own
    /// accept + auth) must not hold the lock.
    #[test]
    fn activity_before_the_disconnect_is_ignored() {
        let activity = ConnectionActivity::default();
        activity.last_accept_ms.store(100, Ordering::SeqCst);
        activity.last_auth_ms.store(200, Ordering::SeqCst);
        assert_eq!(activity.accept_age(200, 1_000), None);
        assert_eq!(activity.auth_age(200, 1_000), None);
        assert_eq!(
            activity.auth_age(150, 1_000),
            Some(Duration::from_millis(800))
        );
    }

    struct Rejecting;
    impl ConnectionHandler for Rejecting {
        fn on_accept(&mut self, _peer: SocketAddr) -> bool {
            false
        }
    }

    #[test]
    fn the_wrapper_records_only_what_the_inner_handler_allows() {
        let peer = SocketAddr::from(([203, 0, 113, 5], 51000));
        let activity = Arc::new(ConnectionActivity::default());

        let mut rejecting = ActivityHandler::wrap(Some(Box::new(Rejecting)), activity.clone());
        assert!(!rejecting.on_accept(peer), "the inner verdict is preserved");
        rejecting.on_authenticated(false, Some("bad password"));
        assert_eq!(activity.last_accept_ms.load(Ordering::SeqCst), 0);
        assert_eq!(activity.last_auth_ms.load(Ordering::SeqCst), 0);

        let mut open = ActivityHandler::wrap(None, activity.clone());
        assert!(open.on_accept(peer), "no inner handler accepts all");
        open.on_authenticated(true, None);
        assert_ne!(activity.last_accept_ms.load(Ordering::SeqCst), 0);
        assert_ne!(activity.last_auth_ms.load(Ordering::SeqCst), 0);
        assert_eq!(
            open.on_disconnected(peer, S(1), None),
            PostConnectionAction::Continue
        );
    }
}
