//! The server takes one [`ConnectionHandler`]; macrdp has several independent ones
//! (the auth guard, `--lock-on-disconnect`'s activity record, the keyboard-layout
//! publisher). [`ConnectionHooks`] runs them as one.
//!
//! Composing beats wrapping: a wrapper must forward every hook by hand, and a hook
//! it doesn't forward is silently swallowed by the trait's default no-op. That's a
//! real hazard here, since upstream adds hooks (`on_connection_info` arrived at the
//! e258f6a0 pin bump). This is the one place a new hook needs adding.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ironrdp_server::{ConnectionHandler, ConnectionInfo, PostConnectionAction, ServerError};

use crate::input::SharedKeyboardLayout;

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

/// Publishes the client's keyboard-layout id (KLID, from its GCC Client Core Data)
/// so the input handler can auto-select a matching non-US layout when
/// `--keyboard-layout` isn't given.
pub struct KeyboardLayoutHook(pub SharedKeyboardLayout);

impl ConnectionHandler for KeyboardLayoutHook {
    fn on_connection_info(&mut self, info: &ConnectionInfo) {
        self.0.store(info.keyboard_layout, Ordering::Relaxed);
        tracing::debug!(
            klid = info.keyboard_layout,
            "client keyboard layout announced"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, AtomicUsize};
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
        let cell: SharedKeyboardLayout = Arc::new(AtomicU32::new(0));
        let mut hooks = ConnectionHooks::new(vec![Box::new(KeyboardLayoutHook(cell.clone()))]);
        hooks.on_connection_info(&info(0x040C));
        assert_eq!(cell.load(Ordering::SeqCst), 0x040C);
    }
}
