//! (vendored, feature=multitransport, Phase 2) Sans-I/O **DTLS 1.2 server** for
//! the LOSSY (`UdpFecL`) RDPEUDP flow.
//!
//! The reliable (`UdpFecR`) flow secures its MS-RDPEMT tunnel with **rustls**
//! over the reliable byte-stream (see `listener.rs`). The lossy flow instead
//! carries **DTLS** records inside its RDPEUDP payloads — mstsc was observed
//! (P2.0 spike) opening a `SYN_LOSSY` flow and sending a **DTLS 1.2** ClientHello
//! (`0x16 0xFE 0xFD`). This module drives BoringSSL's DTLS server handshake
//! sans-I/O: macrdp owns the UDP socket + RDPEUDP demux, so we feed inbound
//! datagram bytes in and pull outbound datagram bytes out, rather than handing
//! BoringSSL a socket.
//!
//! ## The load-bearing rule (datagram boundaries)
//!
//! boring's [`SslStream<S>`] installs a *custom BIO* whose read callback calls
//! `S::read` exactly once per `BIO_read` and returns that call's bytes verbatim.
//! BoringSSL's DTLS record layer expects **one whole datagram per `BIO_read`**.
//! So [`MemTransport::read`] MUST return exactly one inbound datagram per call —
//! never coalesce two received datagrams, never split one. We enforce this by
//! staging a single datagram into `inbound` before each `do_handshake` pump and
//! draining it in one `read`. (We do NOT use a BoringSSL memory BIO
//! `BIO_s_mem` — that's stream-oriented and corrupts DTLS record boundaries; the
//! custom-BIO-over-`Read`/`Write` bridge is the correct sans-I/O mechanism.)
//!
//! ## No cookie exchange
//!
//! BoringSSL removed `DTLSv1_listen` and performs no HelloVerifyRequest by
//! default (we never set `SSL_OP_COOKIE_EXCHANGE`). macrdp already demuxes peers
//! by RDPEUDP flow, so the first ClientHello drives a full handshake straight
//! away — exactly what we want.
//!
//! **Scope (P2.1a): handshake only.** Reaching "established" proves
//! MS-RDPEMT-over-DTLS is reachable. The post-handshake app-record path (the EMT
//! tunnel + lossy audio DVC) is P2.4+.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::Arc;

use boring::pkey::PKey;
use boring::ssl::{
    HandshakeError, MidHandshakeSslStream, Ssl, SslContext, SslContextBuilder, SslMethod, SslOptions, SslStream,
    SslVerifyMode,
};
use boring::x509::X509;

/// Datagram MTU handed to BoringSSL so it fragments the Certificate handshake
/// message to fit inside one RDPEUDP source payload. Kept comfortably under the
/// RDPEUDP MTU (1232) minus RDPUDP/FEC/source-header overhead, so a DTLS
/// datagram is never split across two RDPEUDP datagrams.
const DTLS_MTU: u32 = 1100;

fn stack(e: boring::error::ErrorStack) -> io::Error {
    io::Error::other(e.to_string())
}

/// Built once from the server's cert+key (the same self-signed cert as the TCP
/// TLS path), cheaply cloned per peer.
#[derive(Clone)]
pub struct DtlsServerContext {
    ctx: Arc<SslContext>,
}

impl DtlsServerContext {
    /// Build the DTLS 1.2 server context from DER-encoded cert + private key (the
    /// bytes macrdp already holds for the rustls config).
    pub fn from_der(cert_der: &[u8], key_der: &[u8]) -> io::Result<Self> {
        let cert = X509::from_der(cert_der).map_err(stack)?;
        let key = PKey::private_key_from_der(key_der).map_err(stack)?;

        let mut b = SslContextBuilder::new(SslMethod::dtls()).map_err(stack)?;
        // Pin DTLS 1.2 (mstsc's ClientHello is 0xFEFD). boring has no DTLS
        // `SslVersion` constants — DTLS version is selected via SslOptions, so
        // disable DTLS 1.0, leaving 1.2 as the only enabled version.
        b.set_options(SslOptions::NO_DTLSV1);
        b.set_certificate(&cert).map_err(stack)?;
        b.set_private_key(&key).map_err(stack)?;
        b.check_private_key().map_err(stack)?;
        // Server-authenticated only; the client presents no cert (TOFU on the
        // same self-signed cert as the TCP path). Deliberately NOT setting
        // SSL_OP_COOKIE_EXCHANGE — we demux peers ourselves.
        b.set_verify(SslVerifyMode::NONE);

        Ok(Self {
            ctx: Arc::new(b.build()),
        })
    }

    /// One DTLS endpoint per RDPEUDP lossy peer/flow.
    pub fn new_conn(&self) -> io::Result<DtlsConn> {
        let mut ssl = Ssl::new(&self.ctx).map_err(stack)?;
        ssl.set_mtu(DTLS_MTU).map_err(stack)?; // wired to BIO_CTRL_DGRAM_QUERY_MTU
        // `setup_accept` puts the SSL in server-accept state and wraps the
        // transport, performing no I/O (it returns a mid-handshake stream we
        // drive datagram-by-datagram).
        let mid = ssl.setup_accept(MemTransport::default());
        Ok(DtlsConn {
            state: State::Handshaking(Some(mid)),
        })
    }
}

/// Handshake state. `Option<>` lets us move the mid-handshake stream out of
/// `&mut self` across `handshake()` (which consumes it).
enum State {
    Handshaking(Option<MidHandshakeSslStream<MemTransport>>),
    Established(SslStream<MemTransport>),
    Failed,
}

/// In-memory datagram transport backing boring's custom BIO. `inbound` holds the
/// ONE datagram we want BoringSSL to read next (staged right before each pump);
/// `outbound` queues the datagrams BoringSSL produced (one per `write` call).
#[derive(Default)]
struct MemTransport {
    inbound: Vec<u8>,
    outbound: VecDeque<Vec<u8>>,
}

impl Read for MemTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.inbound.is_empty() {
            // No staged datagram → tell BoringSSL WANT_READ; `do_handshake`
            // then reports `would_block()`.
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        // Hand back exactly ONE datagram (truncate rather than split if it
        // somehow exceeds buf — a malformed record the DTLS layer drops, never a
        // silent mis-frame).
        let n = self.inbound.len().min(buf.len());
        buf[..n].copy_from_slice(&self.inbound[..n]);
        self.inbound.clear();
        Ok(n)
    }
}

impl Write for MemTransport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // One BIO_write == one outbound datagram; queue it whole.
        self.outbound.push_back(buf.to_vec());
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// One peer's DTLS handshake state.
pub struct DtlsConn {
    state: State,
}

fn drain_outbound(t: &mut MemTransport) -> Vec<Vec<u8>> {
    t.outbound.drain(..).collect()
}

impl DtlsConn {
    /// Feed ONE inbound DTLS datagram (the bytes pulled from an RDPEUDP lossy
    /// payload) and return the outbound datagrams to send back (wrap each in an
    /// RDPEUDP payload). Returns several when a flight spans multiple records
    /// (ServerHello + Certificate fragments + ServerHelloDone, or the final
    /// Finished flight).
    pub fn read_datagram(&mut self, input: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        if !input.is_empty() {
            // Stage exactly this one datagram (drained in one `read`).
            if let Some(t) = self.transport_mut() {
                t.inbound = input.to_vec();
            }
        }
        match core::mem::replace(&mut self.state, State::Failed) {
            State::Handshaking(Some(mid)) => match mid.handshake() {
                Ok(mut stream) => {
                    let out = drain_outbound(stream.get_mut());
                    self.state = State::Established(stream);
                    Ok(out)
                }
                // Consumed the staged datagram, produced part of a flight; wait
                // for the next inbound datagram.
                Err(HandshakeError::WouldBlock(mut mid)) => {
                    let out = drain_outbound(mid.get_mut());
                    self.state = State::Handshaking(Some(mid));
                    Ok(out)
                }
                Err(HandshakeError::Failure(mid)) => {
                    self.state = State::Failed;
                    Err(io::Error::other(format!("DTLS handshake failed: {}", mid.error())))
                }
                Err(HandshakeError::SetupFailure(e)) => {
                    self.state = State::Failed;
                    Err(io::Error::other(e.to_string()))
                }
            },
            State::Handshaking(None) => {
                self.state = State::Failed;
                Err(io::Error::other("DTLS conn in invalid state"))
            }
            // Already established: post-handshake app records are P2.4+.
            State::Established(s) => {
                self.state = State::Established(s);
                Ok(Vec::new())
            }
            State::Failed => {
                self.state = State::Failed;
                Err(io::Error::other("DTLS conn already failed"))
            }
        }
    }

    /// Whether the DTLS handshake has completed (the peer is established).
    pub fn is_handshake_done(&self) -> bool {
        matches!(self.state, State::Established(_))
    }

    /// (Post-handshake) Decrypt ONE inbound DTLS application datagram (pulled
    /// from an RDPEUDP lossy payload) → plaintext (the MS-RDPEMT tunnel PDU
    /// bytes). `Ok(None)` when the record yielded no plaintext (e.g. a
    /// retransmit/alert consumed internally). Errors if not yet established.
    pub fn recv(&mut self, input: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let State::Established(s) = &mut self.state else {
            return Err(io::Error::other("DTLS not established"));
        };
        if !input.is_empty() {
            s.get_mut().inbound = input.to_vec();
        }
        let mut buf = vec![0u8; 16 * 1024];
        match s.ssl_read(&mut buf) {
            Ok(n) => {
                buf.truncate(n);
                Ok(Some(buf))
            }
            Err(e) if e.would_block() => Ok(None),
            Err(e) => Err(io::Error::other(format!("DTLS read: {e}"))),
        }
    }

    /// (Post-handshake) Encrypt `plaintext` (an MS-RDPEMT tunnel PDU) → the
    /// outbound DTLS datagram(s) to send back (wrap each in an RDPEUDP payload).
    pub fn send(&mut self, plaintext: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        let State::Established(s) = &mut self.state else {
            return Err(io::Error::other("DTLS not established"));
        };
        s.ssl_write(plaintext)
            .map_err(|e| io::Error::other(format!("DTLS write: {e}")))?;
        Ok(drain_outbound(s.get_mut()))
    }

    fn transport_mut(&mut self) -> Option<&mut MemTransport> {
        match &mut self.state {
            State::Handshaking(Some(mid)) => Some(mid.get_mut()),
            State::Established(s) => Some(s.get_mut()),
            _ => None,
        }
    }
}
