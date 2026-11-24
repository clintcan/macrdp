//! App-switcher HUD IPC client.
//!
//! Pushes SHOW/ADVANCE/HIDE commands to the `macrdphud` helper process (which
//! draws a non-activating overlay panel the SCK capture picks up, so the remote
//! client sees a visual Cmd+Tab/Option+Tab switcher). The helper LISTENS on
//! `127.0.0.1:$MACRDP_HUD_PORT` (default 40243); we CONNECT and push.
//!
//! Strictly best-effort and non-blocking from the caller's side: the input/RDP
//! path calls [`show`]/[`advance`]/[`hide`], which only `try_send` onto a bounded
//! channel and return immediately. A dedicated worker thread owns the TCP socket
//! (lazy connect, reconnect on error) and does the blocking writes off the input
//! path. If the helper is down or the channel is full, commands are dropped — the
//! switch itself always proceeds (the HUD is purely cosmetic).
//!
//! Wire framing mirrors the smart-card bridge (opcode `u8` + big-endian lengths):
//!   SHOW(1):    [display_id:u32][cursor:u16][count:u16] then count×([pid:i32][name_len:u16][name])
//!   ADVANCE(2): [cursor:u16]
//!   HIDE(3):    (no payload)

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::OnceLock;
use std::time::Duration;

const DEFAULT_PORT: u16 = 40243;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
const WRITE_TIMEOUT: Duration = Duration::from_millis(200);

enum HudMsg {
    Show {
        display_id: u32,
        cursor: u16,
        apps: Vec<(i32, String)>,
    },
    Advance {
        cursor: u16,
    },
    Hide,
}

static SENDER: OnceLock<SyncSender<HudMsg>> = OnceLock::new();

/// The `CGDirectDisplayID` macrdp is capturing — the helper centers the panel on
/// it. Set once from `main` (the virtual display's id, or the main display's id on
/// the mirror-primary path). 0 = let the helper fall back to the main screen.
static DISPLAY_ID: AtomicU32 = AtomicU32::new(0);

/// Set the captured display id the HUD should center on. Called from `main`.
pub fn set_display_id(id: u32) {
    DISPLAY_ID.store(id, Ordering::Relaxed);
}

fn port() -> u16 {
    std::env::var("MACRDP_HUD_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

/// Get-or-start the worker thread, returning its sender. Spawns on first use, so
/// nothing happens unless the feature is enabled and a switch occurs.
fn sender() -> &'static SyncSender<HudMsg> {
    SENDER.get_or_init(|| {
        let (tx, rx) = sync_channel::<HudMsg>(64);
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port()));
        std::thread::Builder::new()
            .name("macrdp-hud".into())
            .spawn(move || {
                let mut stream: Option<TcpStream> = None;
                while let Ok(msg) = rx.recv() {
                    if stream.is_none() {
                        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                            Ok(s) => {
                                let _ = s.set_write_timeout(Some(WRITE_TIMEOUT));
                                let _ = s.set_nodelay(true);
                                stream = Some(s);
                            }
                            Err(_) => continue, // helper not up — drop, retry next msg
                        }
                    }
                    let frame = encode(&msg);
                    if let Some(s) = stream.as_mut() {
                        if s.write_all(&frame).is_err() {
                            stream = None; // reconnect on the next message
                        }
                    }
                }
            })
            .expect("spawn macrdp-hud thread");
        tx
    })
}

fn encode(msg: &HudMsg) -> Vec<u8> {
    let mut b = Vec::new();
    match msg {
        HudMsg::Show {
            display_id,
            cursor,
            apps,
        } => {
            b.push(1);
            b.extend_from_slice(&display_id.to_be_bytes());
            b.extend_from_slice(&cursor.to_be_bytes());
            let count = u16::try_from(apps.len()).unwrap_or(u16::MAX);
            b.extend_from_slice(&count.to_be_bytes());
            for (pid, name) in apps.iter().take(count as usize) {
                b.extend_from_slice(&pid.to_be_bytes());
                let nb = name.as_bytes();
                let nlen = u16::try_from(nb.len()).unwrap_or(u16::MAX);
                b.extend_from_slice(&nlen.to_be_bytes());
                b.extend_from_slice(&nb[..nlen as usize]);
            }
        }
        HudMsg::Advance { cursor } => {
            b.push(2);
            b.extend_from_slice(&cursor.to_be_bytes());
        }
        HudMsg::Hide => b.push(3),
    }
    b
}

fn send(msg: HudMsg) {
    // try_send: never block the caller; drop if the worker is backed up.
    let _ = sender().try_send(msg);
}

/// Show the switcher with `apps` (pid + display name) selecting `cursor`, centered
/// on the captured display (see [`set_display_id`]).
pub fn show(apps: Vec<(i32, String)>, cursor: usize) {
    send(HudMsg::Show {
        display_id: crate::virtual_display::current_id(DISPLAY_ID.load(Ordering::Relaxed)),
        cursor: u16::try_from(cursor).unwrap_or(0),
        apps,
    });
}

/// Move the selection highlight (snapshot stays frozen for the session).
pub fn advance(cursor: usize) {
    send(HudMsg::Advance {
        cursor: u16::try_from(cursor).unwrap_or(0),
    });
}

/// Hide the overlay (session committed / no candidates).
pub fn hide() {
    send(HudMsg::Hide);
}
