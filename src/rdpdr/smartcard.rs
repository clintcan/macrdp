//! Smart-card redirection bridge (macOS, `--enable-smartcard-redirection`).
//!
//! The connecting RDP client redirects its physical smart-card reader as an
//! RDPDR `Smartcard` device; this bridge makes that reader usable by **macOS**
//! apps. macrdp's own PC/SC IFD handler (the `ifd-macrdp.bundle` cdylib loaded by
//! `com.apple.ifdreader.slotd`) dials this bridge over loopback TCP and speaks a
//! tiny request/reply protocol; the bridge translates each request into an
//! MS-RDPESC call to the client's reader via [`RdpdrHandle`]'s `scard_*` methods
//! and hands the result back.
//!
//! Wire protocol (handler → bridge request / bridge → handler reply), multi-byte
//! lengths big-endian — must stay in lockstep with `ifd-handler/src/lib.rs`:
//!   POWER_ON  (1)                → `[status:u8]`; if 0: `[atr_len:u8][atr…]`  (1 = no card)
//!   POWER_OFF (2)                → `[status:u8]`
//!   TRANSMIT  (3)[send_len:u32][apdu…][recv_len:u32] → `[status:u8]`; if 0: `[resp_len:u32][resp…]`
//!   PRESENCE  (4)                → `[present:u8]`  (1 = card present)
//!
//! One [`SmartcardBridge`] per connection (the device id comes from the announced
//! `Smartcard` device); dropping it aborts the listener and every in-flight
//! session, so a client disconnect tears the bridge down. Each accepted handler
//! connection gets its own [`Session`] holding the established PC/SC context, the
//! chosen reader, and the connected card handle.

use anyhow::{anyhow, Result};
use ironrdp_rdpdr::pdu::esc::{
    CardProtocol, CardStateFlags, ScardContext, ScardHandle as ScardCardHandle,
};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use ironrdp_server::{RdpdrHandle, SCARD_LEAVE_CARD, SCARD_SHARE_SHARED};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{debug, info, warn};

/// Default loopback port the IFD handler dials (overridable, in lockstep with the
/// handler, via `MACRDP_SCARD_PORT`).
const DEFAULT_PORT: u16 = 40242;

// Protocol opcodes — must match `ifd-handler/src/lib.rs`.
const CMD_POWER_ON: u8 = 1;
const CMD_POWER_OFF: u8 = 2;
const CMD_TRANSMIT: u8 = 3;
const CMD_PRESENCE: u8 = 4;

/// Upper bound on a `CMD_TRANSMIT` command-APDU length. The bridge listens on
/// loopback with no authentication (see the unauthenticated-loopback-IPC note in
/// `docs/macos-gotchas.md`), so an untrusted local process could send an
/// arbitrary 32-bit `send_len`; cap it before allocating so a bogus length can't
/// force a multi-gigabyte allocation. No real APDU exceeds the ISO 7816
/// extended-APDU maximum (4-byte header + 3-byte Lc + 65535 data + 2-byte Le).
const MAX_APDU_LEN: usize = 65_544;

fn port() -> u16 {
    std::env::var("MACRDP_SCARD_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

/// A running smart-card bridge. Holds the listener task; dropping it aborts the
/// task (and, via the task's `JoinSet`, every in-flight session).
#[derive(Debug)]
pub struct SmartcardBridge {
    task: JoinHandle<()>,
}

impl SmartcardBridge {
    /// Bind the loopback listener and start serving the IFD handler. `device_id`
    /// is the announced `Smartcard` device the calls are routed to.
    pub fn start(handle: RdpdrHandle, device_id: u32) -> Self {
        let listen_port = port();
        let task = tokio::spawn(async move {
            if let Err(e) = serve(handle, device_id, listen_port).await {
                warn!(error = %e, "smart card: IFD bridge listener stopped");
            }
        });
        Self { task }
    }
}

impl Drop for SmartcardBridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(handle: RdpdrHandle, device_id: u32, listen_port: u16) -> Result<()> {
    // SO_REUSEADDR so a quick client reconnect can rebind the fixed port even if
    // the previous bridge's listener is still winding down (or in TIME_WAIT).
    let socket = TcpSocket::new_v4().map_err(|e| anyhow!("socket: {e}"))?;
    socket
        .set_reuseaddr(true)
        .map_err(|e| anyhow!("set_reuseaddr: {e}"))?;
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, listen_port));
    socket.bind(addr).map_err(|e| anyhow!("bind {addr}: {e}"))?;
    let listener = socket.listen(16).map_err(|e| anyhow!("listen: {e}"))?;
    info!(
        port = listen_port,
        device_id, "smart card: IFD bridge listening"
    );

    // Sessions live in a JoinSet so aborting this task (bridge Drop) cancels them.
    let mut sessions: JoinSet<()> = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|e| anyhow!("accept: {e}"))?;
                debug!("smart card: IFD handler connected");
                let handle = handle.clone();
                sessions.spawn(async move {
                    let mut session = Session::new(handle, device_id);
                    if let Err(e) = session.run(stream).await {
                        debug!(error = %e, "smart card: session ended with error");
                    }
                    session.teardown().await;
                });
            }
            // Reap finished sessions so the set doesn't grow unbounded.
            Some(_) = sessions.join_next(), if !sessions.is_empty() => {}
        }
    }
}

/// How long a presence result is cached. macOS CryptoTokenKit/slotd polls
/// `IFDHICCPresence` extremely fast (tens of times per second) — with a hardware
/// reader the I/O latency throttles that, but our loopback bridge answers
/// instantly, so without a cache every poll becomes a `GetStatusChange`
/// round-trip to the client over RDP, flooding the shared channel and competing
/// with EGFX video / RDPSND audio. Card insert/remove is a human-timescale event,
/// so caching presence for this long is invisible to the user.
const PRESENCE_TTL: Duration = Duration::from_millis(300);

/// Per-handler-connection PC/SC state. The IFD handler keeps one connection and
/// serializes its calls, so a `Session` is single-threaded in practice.
struct Session {
    handle: RdpdrHandle,
    device_id: u32,
    context: Option<ScardContext>,
    reader: Option<String>,
    card: Option<(ScardCardHandle, CardProtocol)>,
    /// Last presence result + when it was observed (see [`PRESENCE_TTL`]).
    presence_cache: Option<(Instant, bool)>,
}

impl Session {
    fn new(handle: RdpdrHandle, device_id: u32) -> Self {
        Self {
            handle,
            device_id,
            context: None,
            reader: None,
            card: None,
            presence_cache: None,
        }
    }

    async fn run(&mut self, mut stream: TcpStream) -> Result<()> {
        loop {
            // A read error here is the handler closing the connection (EOF) — a
            // clean end, not a failure.
            let cmd = match stream.read_u8().await {
                Ok(c) => c,
                Err(_) => return Ok(()),
            };
            match cmd {
                CMD_PRESENCE => {
                    let present = match self.presence().await {
                        Ok(p) => p,
                        Err(e) => {
                            debug!(error = %e, "smart card: presence failed (reporting no card)");
                            false
                        }
                    };
                    stream.write_u8(u8::from(present)).await?;
                }
                CMD_POWER_ON => match self.power_on().await {
                    Ok(atr) => {
                        let len = u8::try_from(atr.len()).unwrap_or(0);
                        stream.write_u8(0).await?;
                        stream.write_u8(len).await?;
                        stream.write_all(&atr[..len as usize]).await?;
                    }
                    Err(e) => {
                        debug!(error = %e, "smart card: power_on failed (reporting no card)");
                        stream.write_u8(1).await?;
                    }
                },
                CMD_TRANSMIT => {
                    let send_len = stream.read_u32().await? as usize;
                    // Bound the wire-supplied length before allocating — an
                    // unauthenticated local process could otherwise request a
                    // huge allocation. Anything over a real APDU is bogus.
                    if send_len > MAX_APDU_LEN {
                        warn!(
                            send_len,
                            "smart card: TRANSMIT length exceeds the APDU cap; closing"
                        );
                        return Ok(());
                    }
                    let mut apdu = vec![0u8; send_len];
                    stream.read_exact(&mut apdu).await?;
                    let recv_len = stream.read_u32().await?; // caller's recv-buffer size
                    match self.transmit(&apdu, recv_len).await {
                        Ok(resp) => {
                            let n = u32::try_from(resp.len()).unwrap_or(u32::MAX);
                            stream.write_u8(0).await?;
                            stream.write_u32(n).await?;
                            stream.write_all(&resp[..n as usize]).await?;
                        }
                        Err(e) => {
                            debug!(error = %e, "smart card: transmit failed");
                            stream.write_u8(1).await?;
                        }
                    }
                }
                CMD_POWER_OFF => {
                    let _ = self.power_off().await;
                    stream.write_u8(0).await?;
                }
                other => {
                    warn!(opcode = other, "smart card: unknown bridge opcode; closing");
                    return Ok(());
                }
            }
        }
    }

    /// Lazily establish a PC/SC context and pick the first redirected reader.
    async fn ensure_session(&mut self) -> Result<(ScardContext, String)> {
        if self.context.is_none() {
            debug!("smart card: establishing context");
            let ctx = self.handle.scard_establish_context(self.device_id).await?;
            debug!(context = ctx.value, "smart card: established context");
            self.context = Some(ctx);
        }
        let context = self.context.expect("context just set");
        if self.reader.is_none() {
            debug!("smart card: listing readers");
            let readers = self
                .handle
                .scard_list_readers(self.device_id, context)
                .await?;
            debug!(count = readers.len(), readers = ?readers, "smart card: list_readers result");
            let reader = readers
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("client redirected no smart-card readers"))?;
            info!(reader = %reader, "smart card: using redirected reader");
            self.reader = Some(reader);
        }
        Ok((context, self.reader.clone().expect("reader just set")))
    }

    async fn presence(&mut self) -> Result<bool> {
        // Serve rapid repeat polls from cache to avoid flooding RDP (see PRESENCE_TTL).
        if let Some((observed, present)) = self.presence_cache {
            if observed.elapsed() < PRESENCE_TTL {
                return Ok(present);
            }
        }
        let (context, reader) = self.ensure_session().await?;
        let states = self
            .handle
            .scard_get_status_change(self.device_id, context, std::slice::from_ref(&reader), 0)
            .await?;
        for s in &states {
            debug!(
                current_state = ?s.current_state,
                event_state = ?s.event_state,
                atr_length = s.atr_length,
                "smart card: get_status_change state"
            );
        }
        let present = states
            .iter()
            .any(|s| s.event_state.contains(CardStateFlags::SCARD_STATE_PRESENT));
        self.presence_cache = Some((Instant::now(), present));
        Ok(present)
    }

    async fn power_on(&mut self) -> Result<Vec<u8>> {
        let (context, reader) = self.ensure_session().await?;
        if self.card.is_none() {
            let (mut card, protocol) = self
                .handle
                .scard_connect(
                    self.device_id,
                    context,
                    &reader,
                    SCARD_SHARE_SHARED,
                    CardProtocol::SCARD_PROTOCOL_T0 | CardProtocol::SCARD_PROTOCOL_T1,
                )
                .await?;
            // Connect_Return carries the handle with an EMPTY embedded context
            // (Windows omits it — it already knows which context we connected on).
            // But a REDIR_SCARDHANDLE in a *request* (Transmit/Status/Disconnect)
            // must carry the real context, or the client-side redirector faults on
            // the missing context and tears down the whole channel. Fill it in.
            card.context = context;
            debug!(protocol = ?protocol, "smart card: connected to card");
            self.card = Some((card, protocol));
        }
        // SCardStatus's parameters are finicky over MS-RDPESC — real Windows
        // rejects some combinations with SCARD_E_INVALID_PARAMETER — and we only
        // need the ATR. GetStatusChange reliably carries the ATR for a present
        // card, so read it from there.
        let states = self
            .handle
            .scard_get_status_change(self.device_id, context, std::slice::from_ref(&reader), 0)
            .await?;
        let atr = states
            .iter()
            .find(|s| {
                s.event_state.contains(CardStateFlags::SCARD_STATE_PRESENT) && s.atr_length > 0
            })
            .map(|s| {
                let n = usize::try_from(s.atr_length).unwrap_or(0).min(s.atr.len());
                s.atr[..n].to_vec()
            })
            .ok_or_else(|| anyhow!("card present but no ATR reported"))?;
        debug!(atr = ?atr, "smart card: ATR");
        Ok(atr)
    }

    async fn transmit(&mut self, apdu: &[u8], recv_len: u32) -> Result<Vec<u8>> {
        let (card, protocol) = self
            .card
            .clone()
            .ok_or_else(|| anyhow!("no card connected"))?;
        self.handle
            .scard_transmit(self.device_id, card, protocol, apdu, recv_len)
            .await
    }

    async fn power_off(&mut self) -> Result<()> {
        if let Some((card, _)) = self.card.take() {
            self.handle
                .scard_disconnect(self.device_id, card, SCARD_LEAVE_CARD)
                .await?;
        }
        Ok(())
    }

    /// Best-effort release of the card + context when the handler disconnects
    /// cleanly. (On a hard RDP disconnect the session task is aborted before this
    /// runs; the client's resource manager reaps the orphaned context when the
    /// channel closes.)
    async fn teardown(&mut self) {
        if let Some((card, _)) = self.card.take() {
            let _ = self
                .handle
                .scard_disconnect(self.device_id, card, SCARD_LEAVE_CARD)
                .await;
        }
        if let Some(context) = self.context.take() {
            let _ = self
                .handle
                .scard_release_context(self.device_id, context)
                .await;
        }
    }
}
