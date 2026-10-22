//! Connection lifecycle work macrdp does on every served connection, on the
//! server's `ConnectionHandler` seam around whatever handler is installed.

use std::net::SocketAddr;
use std::time::Duration;

use ironrdp_server::{ConnectionHandler, PostConnectionAction};

/// Wraps the server's connection handler (or none), forwarding every hook unchanged.
pub struct SessionHooks {
    inner: Option<Box<dyn ConnectionHandler>>,
}

impl SessionHooks {
    pub fn wrap(inner: Option<Box<dyn ConnectionHandler>>) -> Box<dyn ConnectionHandler> {
        Box::new(Self { inner })
    }
}

impl ConnectionHandler for SessionHooks {
    fn on_accept(&mut self, peer: SocketAddr) -> bool {
        self.inner.as_mut().is_none_or(|h| h.on_accept(peer))
    }

    fn on_disconnected(
        &mut self,
        peer: SocketAddr,
        duration: Duration,
        error: Option<&anyhow::Error>,
    ) -> PostConnectionAction {
        // Connections are served one at a time, so this was the session the stats describe.
        if let Some(s) = crate::stats::global() {
            s.session_ended();
        }
        self.inner
            .as_mut()
            .map_or(PostConnectionAction::Continue, |h| {
                h.on_disconnected(peer, duration, error)
            })
    }

    fn on_authenticated(&mut self, success: bool, reason: Option<&str>) {
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

    #[test]
    fn every_hook_reaches_the_inner_handler() {
        struct Rejecting;
        impl ConnectionHandler for Rejecting {
            fn on_accept(&mut self, _peer: SocketAddr) -> bool {
                false
            }
            fn on_disconnected(
                &mut self,
                _peer: SocketAddr,
                _duration: Duration,
                _error: Option<&anyhow::Error>,
            ) -> PostConnectionAction {
                PostConnectionAction::Stop
            }
        }
        let peer = SocketAddr::from(([127, 0, 0, 1], 1));
        let mut hooks = SessionHooks::wrap(Some(Box::new(Rejecting)));
        assert!(!hooks.on_accept(peer));
        let action = hooks.on_disconnected(peer, Duration::from_secs(1), None);
        assert_eq!(action, PostConnectionAction::Stop);
    }
}
