//! Wakes the displays when a client authenticates, ahead of its capture.
//!
//! With the Mac locked and its displays asleep, ScreenCaptureKit lists no
//! displays at all and display reconfigurations fail with CGError 1014, so a
//! session can't start. Waking them when the client authenticates gives them
//! time to come up before the capture starts; the capture start also wakes
//! and waits (`virtual_display::wake_displays_and_wait`). Waking is what a
//! local keypress does: it never unlocks the Mac.
//!
//! Nothing here keeps the displays awake afterwards. By default macrdp runs
//! `caffeinate -d` for its whole life, and `--allow-sleep` means the user
//! wants the Mac to sleep.

use ironrdp_server::ConnectionHandler;

/// Wakes the displays when a client authenticates.
pub struct AwakeHook;

impl ConnectionHandler for AwakeHook {
    fn on_authenticated(&mut self, success: bool, _reason: Option<&str>) {
        if success && crate::virtual_display::any_display_asleep() {
            tracing::info!("a client authenticated while the displays sleep; waking them");
            crate::virtual_display::wake_display();
        }
    }
}
