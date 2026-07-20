//! Shield-window IPC client.
//!
//! Drives the `macrdpshield` helper process, which draws an opaque black window
//! over each physical panel for the headless blanking modes. The helper LISTENS
//! on `127.0.0.1:$MACRDP_SHIELD_PORT` (default 40244); we CONNECT and push.
//!
//! **This is deliberately NOT modelled on [`crate::switcher_hud`].** The HUD is
//! cosmetic, so it fires and forgets onto a bounded channel and drops commands
//! when the helper is down. A shield is a *privacy* mechanism: a dropped SHOW
//! means the physical panel keeps displaying the desktop while the operator
//! believes it is blanked. So every call here is **synchronous, waits for the
//! helper's reply, and returns the number of displays actually shielded**.
//!
//! Waiting for the reply is the whole point and was a real bug when it was
//! missing: a successful `write_all` proves only that bytes reached a kernel
//! socket buffer. It does *not* prove the helper read them (its accept loop can
//! be wedged by an unrelated peer), that it is even listening (a stale helper or
//! a squatter may hold the port), or that any display was covered (a display
//! absent from `NSScreen.screens` is skipped). All three of those return a
//! cheerful `Ok` from a write-only protocol while the desktop stays fully
//! visible. The achieved count closes that gap.
//!
//! Failure mode on helper death is *fail-open*: the panel becomes visible.
//! Fail-open is the right default — the alternative, a black screen with no live
//! process to dismiss it, would strand the machine. But be precise about what it
//! is **not** equivalent to: when a `--capture-primary` macrdp is SIGKILLed its
//! gamma reverts *and the session ends with it*. Here macrdp keeps serving the
//! remote user while the local panel goes clear, and **nothing supervises or
//! restarts the helper**. That asymmetry is a real gap, not a wash.
//!
//! Wire framing mirrors [`crate::switcher_hud`] (opcode `u8` + big-endian):
//!   SHOW(1): [exclude_count:u16] then exclude_count×[display_id:u32]
//!            → reply [status:u8][shielded_count:u16]
//!   HIDE(3): (no payload) → reply [status:u8][shielded_count:u16=0]
//!
//! SHOW carries an **exclude** list (the virtual display), not an include list,
//! so the helper can re-derive "everything else" whenever the screen layout
//! changes — which is what covers a monitor plugged in mid-session. An include
//! list is a snapshot and silently misses those.

use anyhow::{anyhow, Context, Result};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::time::Duration;

const DEFAULT_PORT: u16 = 40244;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const WRITE_TIMEOUT: Duration = Duration::from_millis(250);
/// Bounded so a wedged helper cannot hang the caller (which is a tokio worker).
const READ_TIMEOUT: Duration = Duration::from_millis(1000);

/// Retry budget for reaching the helper.
///
/// **Kept deliberately small because these calls block a tokio worker.**
/// `ShieldedPrimary::install` is invoked from `spawn_primary_overlay_watcher`,
/// which is a `tokio::spawn`ed task (see main.rs) — so every millisecond spent
/// here is a millisecond a runtime worker is not serving anything else. The
/// worst case is `CONNECT_ATTEMPTS × (CONNECT_TIMEOUT + CONNECT_RETRY_DELAY)`,
/// which at these values is ~1 s, in line with the `TX_SETTLE` sleeps the same
/// path already performs.
///
/// A generous budget would be pointless anyway: the helper is spawned at
/// **startup**, long before the first client connects, so by the time anything
/// calls `show` its listener has been up for seconds-to-hours. The retries only
/// cover a cold race, not a slow start.
const CONNECT_ATTEMPTS: usize = 3;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(100);

fn port() -> u16 {
    std::env::var("MACRDP_SHIELD_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

fn addr() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port()))
}

fn connect() -> Result<TcpStream> {
    let a = addr();
    let mut last: Option<std::io::Error> = None;
    for attempt in 0..CONNECT_ATTEMPTS {
        match TcpStream::connect_timeout(&a, CONNECT_TIMEOUT) {
            Ok(s) => {
                s.set_write_timeout(Some(WRITE_TIMEOUT)).ok();
                s.set_nodelay(true).ok();
                return Ok(s);
            }
            Err(e) => {
                last = Some(e);
                if attempt + 1 < CONNECT_ATTEMPTS {
                    std::thread::sleep(CONNECT_RETRY_DELAY);
                }
            }
        }
    }
    Err(anyhow!(
        "could not reach the macrdpshield helper on {a} after {CONNECT_ATTEMPTS} attempts: {}",
        last.map(|e| e.to_string())
            .unwrap_or_else(|| "unknown".into())
    ))
}

/// Send a frame and wait for the helper's `[status:u8][count:u16]` reply.
/// Returns the number of displays the helper reports it actually shielded.
fn send(frame: &[u8]) -> Result<u16> {
    let mut s = connect()?;
    s.set_read_timeout(Some(READ_TIMEOUT)).ok();
    s.write_all(frame)
        .context("writing to the macrdpshield helper")?;
    s.flush().context("flushing to the macrdpshield helper")?;

    let mut reply = [0u8; 3];
    s.read_exact(&mut reply).context(
        "no reply from the macrdpshield helper — it may be wedged, or another \
         process may hold its port",
    )?;
    if reply[0] != 0 {
        return Err(anyhow!("macrdpshield reported failure status {}", reply[0]));
    }
    Ok(u16::from_be_bytes([reply[1], reply[2]]))
}

/// Encode SHOW. Split out so it can be unit-tested without a live helper.
fn encode_show(exclude_ids: &[u32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(3 + exclude_ids.len() * 4);
    b.push(1);
    let count = u16::try_from(exclude_ids.len()).unwrap_or(u16::MAX);
    b.extend_from_slice(&count.to_be_bytes());
    for id in exclude_ids.iter().take(count as usize) {
        b.extend_from_slice(&id.to_be_bytes());
    }
    b
}

fn encode_hide() -> Vec<u8> {
    vec![3]
}

/// Shield every screen EXCEPT `exclude_ids` (the virtual display the client is
/// watching). Returns the number of displays actually shielded.
///
/// Idempotent — re-sending reconciles rather than stacking — so this doubles as
/// the "re-fit after a display change" call.
///
/// Refuses an empty exclude list: that would tell the helper to shield *every*
/// screen including the virtual display, blacking out the remote session too.
/// Every real caller has a vd id to exclude, so an empty list is a bug.
pub fn show(exclude_ids: &[u32]) -> Result<u16> {
    if exclude_ids.is_empty() {
        return Err(anyhow!(
            "shield show called with an empty exclude list — that would shield \
             the virtual display too and black out the remote session"
        ));
    }
    send(&encode_show(exclude_ids))
}

/// Tear every shield down. Best-effort by contract — the caller is usually a
/// `Drop`, and the helper exiting also removes the shields, so a failure here is
/// logged rather than propagated.
pub fn hide() -> Result<()> {
    send(&encode_hide()).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_frame_is_opcode_count_then_ids() {
        assert_eq!(
            encode_show(&[0x0102_0304, 0x0A0B_0C0D]),
            vec![1, 0, 2, 0x01, 0x02, 0x03, 0x04, 0x0A, 0x0B, 0x0C, 0x0D]
        );
    }

    #[test]
    fn show_frame_with_one_display() {
        assert_eq!(encode_show(&[7]), vec![1, 0, 1, 0, 0, 0, 7]);
    }

    #[test]
    fn hide_frame_is_bare_opcode() {
        assert_eq!(encode_hide(), vec![3]);
    }

    #[test]
    fn show_rejects_an_empty_display_list() {
        // Guards against silently "blanking nothing" and reporting success.
        assert!(show(&[]).is_err());
    }
}
