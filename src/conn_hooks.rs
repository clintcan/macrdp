//! The server takes one [`ConnectionHandler`]; macrdp has several independent ones
//! (the auth guard, `--lock-on-disconnect`'s activity record, the keyboard-layout
//! and desktop-scale publishers, and with `--stats-endpoint` the session-stats reset). [`ConnectionHooks`]
//! runs them as one.
//!
//! Composing beats wrapping: a wrapper must forward every hook by hand, and a hook
//! it doesn't forward is silently swallowed by the trait's default no-op. That's a
//! real hazard here, since upstream adds hooks (`on_connection_info` arrived at the
//! e258f6a0 pin bump). This is the one place a new hook needs adding.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use ironrdp_server::{ConnectionHandler, ConnectionInfo, PostConnectionAction, ServerError};

use std::sync::Arc;

use crate::input::SharedKeyboardLayout;
use crate::stats::SessionStats;

/// Several handlers run in order, as one.
pub struct ConnectionHooks(Vec<Box<dyn ConnectionHandler>>);

impl ConnectionHooks {
    pub fn new(hooks: Vec<Box<dyn ConnectionHandler>>) -> Self {
        Self(hooks)
    }
}

impl ConnectionHandler for ConnectionHooks {
    /// Stops at the first rejection, so a later hook never sees a connection an
    /// earlier one refused (put the auth guard first).
    fn on_accept(&mut self, peer: SocketAddr) -> bool {
        self.0.iter_mut().all(|h| h.on_accept(peer))
    }

    fn on_connection_info(&mut self, info: &ConnectionInfo) {
        for h in &mut self.0 {
            h.on_connection_info(info);
        }
    }

    /// Every hook runs; the server stops if any asks it to.
    fn on_disconnected(
        &mut self,
        peer: SocketAddr,
        duration: Duration,
        error: Option<&ServerError>,
    ) -> PostConnectionAction {
        let mut action = PostConnectionAction::Continue;
        for h in &mut self.0 {
            if h.on_disconnected(peer, duration, error) == PostConnectionAction::Stop {
                action = PostConnectionAction::Stop;
            }
        }
        action
    }

    fn on_authenticated(&mut self, success: bool, reason: Option<&str>) {
        for h in &mut self.0 {
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
        for h in &mut self.0 {
            h.on_client_fingerprint(client_name, rdp_version, client_build, platform);
        }
    }
}

/// Asks the input handler to reset its per-connection state (held modifiers and
/// buttons) once per served connection. The server calls `on_connection_info`
/// once per connection, after validation and before the session loop, and not
/// on a Deactivation-Reactivation or for a preemption candidate that loses. See
/// `input::modifier_reset_handle` for why the display `updates()` path and
/// `on_accept` are the wrong places.
pub struct InputResetHook(pub Arc<AtomicBool>);

impl ConnectionHandler for InputResetHook {
    fn on_connection_info(&mut self, _info: &ConnectionInfo) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Releases the keys, buttons and modifiers a connection left pressed on the
/// Mac, when it ends. A dropped link or a killed client sends no key-up, so
/// without this a held key stays down on the Mac, for the local user too,
/// until the same key is pressed again. Windows, as an RDP server, also
/// releases held keys when the connection drops. `InputResetHook` releases
/// the same state again on the next connection.
pub struct InputReleaseHook(pub crate::input::InputRelease);

impl ConnectionHandler for InputReleaseHook {
    fn on_disconnected(
        &mut self,
        _peer: SocketAddr,
        _duration: Duration,
        _error: Option<&ServerError>,
    ) -> PostConnectionAction {
        self.0.release_all("connection ended");
        PostConnectionAction::Continue
    }
}

/// Clears `CaptureDisplay::suppress_next_adopt` once per served connection.
/// The capture loop arms that flag right before a reactivation it triggers, and
/// `request_initial_size` consumes it. If the client disconnects after the flag
/// is armed but before that reactivation reaches Confirm Active, the flag would
/// otherwise survive into the next connection, which would then skip both the
/// stale-resize cleanup and the adopt of its own size. The server calls
/// `on_connection_info` in `client_accepted`, before `request_initial_size`,
/// and not on a reactivation, so this never clears a flag armed for the live
/// session.
pub struct AdoptResetHook(pub Arc<AtomicBool>);

impl ConnectionHandler for AdoptResetHook {
    fn on_connection_info(&mut self, _info: &ConnectionInfo) {
        if self.0.swap(false, Ordering::Relaxed) {
            tracing::debug!("cleared a reactivation flag left by the previous connection");
        }
    }
}

/// Publishes the client's keyboard-layout id (KLID, from its GCC Client Core Data)
/// and whether the client runs on macOS, so the input handler can auto-select a
/// matching non-US layout when `--keyboard-layout` isn't given. The server
/// reports the fingerprint of the served connection just before its
/// `on_connection_info`, and not on a reactivation.
pub struct KeyboardLayoutHook(pub SharedKeyboardLayout);

/// Whether a formatted General-capset platform (`MajorPlatformType(0x06-OSX)/…`)
/// is macOS.
fn is_mac_platform(platform: &str) -> bool {
    platform
        .split('/')
        .next()
        .is_some_and(|major| major.ends_with("-OSX)"))
}

impl ConnectionHandler for KeyboardLayoutHook {
    fn on_client_fingerprint(&mut self, _: &str, _: u32, _: u32, platform: &str) {
        self.0
            .mac_client
            .store(is_mac_platform(platform), Ordering::Relaxed);
    }

    fn on_connection_info(&mut self, info: &ConnectionInfo) {
        self.0.klid.store(info.keyboard_layout, Ordering::Relaxed);
        tracing::debug!(
            klid = info.keyboard_layout,
            "client keyboard layout announced"
        );
    }
}

/// With `--virtual-display-hidpi=auto`, publishes the client's desktop scale
/// factor (percent, from its GCC Client Core Data; 0 when it sent none or an
/// invalid pair) so the virtual display can pick the UI scale for this
/// connection. The server reports it on the initial accept only, so a
/// reactivation never overwrites a newer scale the client announced
/// mid-session in a monitor layout (`HidpiPolicy::observe_client_scale` writes
/// the same cell).
pub struct DesktopScaleHook(pub Arc<AtomicU32>);

impl ConnectionHandler for DesktopScaleHook {
    fn on_connection_info(&mut self, info: &ConnectionInfo) {
        let scale = info.desktop_scale_factor.unwrap_or(0);
        self.0.store(scale, Ordering::Relaxed);
        tracing::debug!(scale, "client desktop scale factor announced");
    }
}

/// With `--multi-monitor`, records the monitor layout the connection adopted
/// (`ConnectionInfo::monitor_layout`, from acceptor divergence 8), or `None`
/// for a single-monitor client, for the display to serve each monitor. Runs at
/// `client_accepted`, before the display's `updates()`.
pub struct MonitorLayoutHook(pub crate::multi_monitor::SessionLayout);

impl ConnectionHandler for MonitorLayoutHook {
    fn on_connection_info(&mut self, info: &ConnectionInfo) {
        let layout = info
            .monitor_layout
            .as_deref()
            .and_then(crate::monitor_layout::Layout::from_client);
        match &layout {
            Some(l) => tracing::info!(
                monitors = l.monitors().len(),
                desktop = ?l.desktop(),
                layout = ?info.monitor_layout,
                "multi-monitor: the client's monitor layout was adopted"
            ),
            None if info.client_monitors.len() > 1 => tracing::info!(
                monitors = ?info.client_monitors,
                "multi-monitor: the client sent several monitors, but not a layout we can serve; one monitor"
            ),
            None => {}
        }
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = layout;
    }
}

/// With `--stats-endpoint`, clears the per-session stats when a connection ends,
/// so the endpoint reports `connected:false` instead of the last session's numbers.
/// Connections are served one at a time, and a preempting client is served only
/// after the session it replaced has reported its disconnect, so this never clears
/// a live session.
pub struct StatsHook(pub Arc<SessionStats>);

impl ConnectionHandler for StatsHook {
    fn on_disconnected(
        &mut self,
        _peer: SocketAddr,
        _duration: Duration,
        _error: Option<&ServerError>,
    ) -> PostConnectionAction {
        self.0.session_ended();
        PostConnectionAction::Continue
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    use super::*;

    /// Counts the hooks it sees; accepts unless told otherwise.
    struct Probe {
        accept: bool,
        stop: bool,
        calls: Arc<AtomicUsize>,
    }

    impl ConnectionHandler for Probe {
        fn on_accept(&mut self, _peer: SocketAddr) -> bool {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.accept
        }

        fn on_connection_info(&mut self, _info: &ConnectionInfo) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }

        fn on_disconnected(
            &mut self,
            _peer: SocketAddr,
            _duration: Duration,
            _error: Option<&ServerError>,
        ) -> PostConnectionAction {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.stop {
                PostConnectionAction::Stop
            } else {
                PostConnectionAction::Continue
            }
        }

        fn on_authenticated(&mut self, _success: bool, _reason: Option<&str>) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }

        fn on_client_fingerprint(&mut self, _: &str, _: u32, _: u32, _: &str) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn probe(accept: bool, stop: bool) -> (Box<dyn ConnectionHandler>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Box::new(Probe {
                accept,
                stop,
                calls: calls.clone(),
            }),
            calls,
        )
    }

    fn info(klid: u32) -> ConnectionInfo {
        ConnectionInfo::new(
            klid,
            ironrdp_pdu::gcc::KeyboardType::IBM_ENHANCED,
            String::new(),
        )
    }

    const PEER: SocketAddr = SocketAddr::V4(std::net::SocketAddrV4::new(
        std::net::Ipv4Addr::new(203, 0, 113, 5),
        51000,
    ));

    #[test]
    fn the_adopt_reset_hook_clears_a_flag_left_by_the_previous_connection() {
        let flag = Arc::new(AtomicBool::new(true));
        let mut hooks = ConnectionHooks::new(vec![Box::new(AdoptResetHook(Arc::clone(&flag)))]);
        hooks.on_connection_info(&info(0x0409));
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn the_stats_hook_reports_a_disconnect() {
        let stats = Arc::new(SessionStats::default());
        stats.connected.store(true, Ordering::Relaxed);
        stats.frames_sent.store(900, Ordering::Relaxed);
        let mut hooks = ConnectionHooks::new(vec![Box::new(StatsHook(Arc::clone(&stats)))]);
        assert_eq!(
            hooks.on_disconnected(PEER, Duration::from_secs(1), None),
            PostConnectionAction::Continue
        );
        assert!(!stats.connected.load(Ordering::Relaxed));
        assert_eq!(stats.frames_sent.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn every_hook_reaches_every_handler() {
        let (a, a_calls) = probe(true, false);
        let (b, b_calls) = probe(true, false);
        let mut hooks = ConnectionHooks::new(vec![a, b]);
        assert!(hooks.on_accept(PEER));
        hooks.on_connection_info(&info(0x0409));
        hooks.on_authenticated(true, None);
        hooks.on_client_fingerprint("pc", 0x0008_0004, 22621, "WINDOWS");
        assert_eq!(
            hooks.on_disconnected(PEER, Duration::from_secs(1), None),
            PostConnectionAction::Continue
        );
        assert_eq!(a_calls.load(Ordering::SeqCst), 5);
        assert_eq!(b_calls.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn a_rejection_stops_later_handlers_seeing_the_connection() {
        let (guard, _) = probe(false, false);
        let (later, later_calls) = probe(true, false);
        let mut hooks = ConnectionHooks::new(vec![guard, later]);
        assert!(!hooks.on_accept(PEER));
        assert_eq!(later_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn any_stop_stops_the_server_but_every_handler_still_runs() {
        let (stopper, _) = probe(true, true);
        let (later, later_calls) = probe(true, false);
        let mut hooks = ConnectionHooks::new(vec![stopper, later]);
        assert_eq!(
            hooks.on_disconnected(PEER, Duration::from_secs(1), None),
            PostConnectionAction::Stop
        );
        assert_eq!(later_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_keyboard_layout_reaches_the_shared_cell() {
        let cell: SharedKeyboardLayout = Arc::default();
        let mut hooks = ConnectionHooks::new(vec![Box::new(KeyboardLayoutHook(cell.clone()))]);
        hooks.on_client_fingerprint(
            "mac",
            0x0008_0005,
            68687,
            "MajorPlatformType(0x06-OSX)/MinorPlatformType(0x00-UNSPECIFIED)",
        );
        hooks.on_connection_info(&info(0x0809));
        assert_eq!(cell.klid.load(Ordering::SeqCst), 0x0809);
        assert!(cell.mac_client.load(Ordering::SeqCst));
        // The next connection is from a Linux client.
        hooks.on_client_fingerprint(
            "pc",
            0x0008_0011,
            18363,
            "MajorPlatformType(0x04-UNIX)/MinorPlatformType(0x07-NATIVE_XSERVER)",
        );
        hooks.on_connection_info(&info(0x040C));
        assert_eq!(cell.klid.load(Ordering::SeqCst), 0x040C);
        assert!(!cell.mac_client.load(Ordering::SeqCst));
    }

    #[test]
    fn the_desktop_scale_factor_reaches_the_shared_cell() {
        let cell = Arc::new(AtomicU32::new(0));
        let mut hooks = ConnectionHooks::new(vec![Box::new(DesktopScaleHook(cell.clone()))]);
        let mut retina = info(0x0409);
        retina.desktop_scale_factor = Some(200);
        retina.device_scale_factor = Some(100);
        hooks.on_connection_info(&retina);
        assert_eq!(cell.load(Ordering::SeqCst), 200);
        // A later client that sends no scale clears the previous one.
        hooks.on_connection_info(&info(0x0409));
        assert_eq!(cell.load(Ordering::SeqCst), 0);
    }
}
