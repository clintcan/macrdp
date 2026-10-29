//! Keeps the displays awake while a client connects and a headless session runs, even with
//! `--allow-sleep`.
//!
//! A sleeping display (the lock screen turns it off after 30 s idle) makes WindowServer
//! re-register the virtual display on wake, which stops the capture under a live session.
//! The hold starts at authentication, so the displays are up before the capture starts.

use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ironrdp_server::{ConnectionHandler, PostConnectionAction};

#[cfg(target_os = "macos")]
mod iokit {
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPMAssertionCreateWithName(
            kind: CFStringRef,
            level: u32,
            name: CFStringRef,
            id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    const LEVEL_ON: u32 = 255;

    pub fn hold() -> Option<u32> {
        let kind = CFString::from_static_string("PreventUserIdleDisplaySleep");
        let name = CFString::from_static_string("macrdp: remote session active");
        let mut id = 0;
        let rc = unsafe {
            IOPMAssertionCreateWithName(
                kind.as_concrete_TypeRef(),
                LEVEL_ON,
                name.as_concrete_TypeRef(),
                &mut id,
            )
        };
        (rc == 0).then_some(id)
    }

    pub fn release(id: u32) {
        unsafe { IOPMAssertionRelease(id) };
    }
}

/// How long an authenticated client holds the displays before its session starts.
const CONNECT_HOLD: Duration = Duration::from_secs(30);
/// How long the hold outlasts a session, so a capture rebuild's 1 -> 0 -> 1 flap keeps it.
const LINGER: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Holds {
    session: bool,
    until: Option<Instant>,
    assertion: Option<u32>,
}

impl Holds {
    fn wanted(&self, now: Instant) -> bool {
        self.session || self.until.is_some_and(|t| now < t)
    }

    fn extend(&mut self, now: Instant, by: Duration) {
        let t = now + by;
        self.until = Some(self.until.map_or(t, |u| u.max(t)));
    }
}

static HOLDS: Mutex<Holds> = Mutex::new(Holds {
    session: false,
    until: None,
    assertion: None,
});

fn update(f: impl FnOnce(&mut Holds, Instant)) {
    let mut holds = HOLDS.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    f(&mut holds, now);
    let wanted = holds.wanted(now);
    #[cfg(target_os = "macos")]
    match (wanted, holds.assertion) {
        (true, None) => {
            holds.assertion = iokit::hold();
            match holds.assertion {
                Some(id) => {
                    tracing::info!(assertion = id, "keeping the displays awake for the session")
                }
                None => tracing::warn!("could not create the display-awake assertion"),
            }
        }
        (false, Some(id)) => {
            iokit::release(id);
            holds.assertion = None;
            tracing::info!(assertion = id, "released the display-awake assertion");
        }
        _ => {}
    }
    #[cfg(not(target_os = "macos"))]
    let _ = wanted;
}

fn recheck_after(delay: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        update(|_, _| {});
    });
}

/// Called once a client authenticates, ahead of its capture: holds the displays
/// awake and starts waking them, so the capture finds them up.
pub fn client_authenticated() {
    update(|h, now| h.extend(now, CONNECT_HOLD));
    recheck_after(CONNECT_HOLD);
    if crate::virtual_display::any_display_asleep() {
        crate::virtual_display::wake_display();
    }
}

/// Called on the first client's arrival.
pub fn session_started() {
    update(|h, _| h.session = true);
}

/// Called when the last client leaves.
pub fn session_ended() {
    update(|h, now| {
        h.session = false;
        h.extend(now, LINGER);
    });
    recheck_after(LINGER);
}

/// Wraps the server's connection handler (or none) to call [`client_authenticated`], forwarding
/// every hook unchanged.
pub struct AwakeHandler {
    inner: Option<Box<dyn ConnectionHandler>>,
}

impl AwakeHandler {
    pub fn wrap(inner: Option<Box<dyn ConnectionHandler>>) -> Box<dyn ConnectionHandler> {
        Box::new(Self { inner })
    }
}

impl ConnectionHandler for AwakeHandler {
    fn on_accept(&mut self, peer: SocketAddr) -> bool {
        self.inner.as_mut().is_none_or(|h| h.on_accept(peer))
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
            client_authenticated();
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

    #[test]
    fn a_hold_lasts_while_a_session_runs_or_until_its_deadline() {
        let t0 = Instant::now();
        let mut h = Holds::default();
        assert!(!h.wanted(t0));
        h.extend(t0, CONNECT_HOLD);
        assert!(h.wanted(t0 + CONNECT_HOLD - Duration::from_millis(1)));
        assert!(!h.wanted(t0 + CONNECT_HOLD));
        h.session = true;
        assert!(h.wanted(t0 + CONNECT_HOLD * 10));
    }

    #[test]
    fn the_wrapper_forwards_every_hook() {
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
        let mut h = AwakeHandler::wrap(Some(Box::new(Rejecting)));
        assert!(!h.on_accept(peer));
        let action = h.on_disconnected(peer, Duration::from_secs(1), None);
        assert_eq!(action, PostConnectionAction::Stop);
    }

    #[test]
    fn a_shorter_extension_never_cuts_a_longer_hold() {
        let t0 = Instant::now();
        let mut h = Holds::default();
        h.extend(t0, CONNECT_HOLD);
        h.extend(t0, LINGER);
        assert!(h.wanted(t0 + LINGER + Duration::from_secs(1)));
    }
}
