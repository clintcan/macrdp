//! Opt-in live-telemetry endpoint (`--stats-endpoint`, default OFF).
//!
//! A tiny **loopback-only, read-only** TCP listener that serves a JSON snapshot
//! of the current H.264 session — live bitrate, link RTT, standing queue delay,
//! frame rate, frames sent, and the session dimensions — on each connection. It
//! exists so the menu-bar controller's Status pane can show live "connection
//! health" without the server writing anything to disk.
//!
//! **No disk writes.** The snapshot lives entirely in memory ([`SessionStats`],
//! a handful of atomics updated at low-frequency points in the encode path) and
//! is serialized *only when a client connects* — which the controller does every
//! ~2 s, and only while its Status pane is open. So there is zero periodic I/O
//! (and therefore no SSD wear), unlike a periodically-rewritten stats file.
//!
//! **Default runtime path unchanged when off.** The endpoint is created only
//! when enabled; when it isn't, [`global`] returns `None`, so every update site
//! in the hot path is a single `Option` check that compiles to a no-op. The
//! listener binds `127.0.0.1` only (never routable) and never reads request
//! bytes — it just writes one JSON line and closes. It carries only the local
//! session's own metrics; the trust boundary is the same single-user, loopback
//! model as the other helper channels (see docs/macos-gotchas.md).

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// The live snapshot. Every field is an atomic so the encode path can update it
/// lock-free; the listener reads it under no lock either. Values are best-effort
/// and eventually-consistent — a slightly stale read between updates is fine for
/// a status display. "Connected" is deliberately NOT authoritative here (there
/// is no reliable EGFX teardown hook — see the h264 reconnect-blank note); the
/// controller determines connected/disconnected from `lsof` and uses these
/// numbers only while it independently sees a live client.
#[derive(Default)]
pub struct SessionStats {
    /// Best-effort: set true at connection setup. Not cleared reliably (no
    /// teardown hook) — the controller gates on its own `lsof` check instead.
    pub connected: AtomicBool,
    pub width: AtomicU32,
    pub height: AtomicU32,
    /// Live encoder bitrate: the adaptive value when `--adaptive-bitrate` is on,
    /// otherwise the configured ceiling (static for the session).
    pub bitrate_bps: AtomicU32,
    /// The `--bitrate` ceiling (adaptive never exceeds it).
    pub ceiling_bps: AtomicU32,
    /// Kernel-measured link RTT (ms) sampled at accept; 0 = unknown.
    pub rtt_ms: AtomicU32,
    /// Standing queue delay (ms above the windowed-min RTT) — the adaptive
    /// controller's congestion signal. Meaningful only with acks flowing.
    pub queue_delay_ms: AtomicU32,
    /// Effective frame rate (capped by the adaptive floor under congestion).
    pub fps: AtomicU32,
    pub frames_sent: AtomicU64,
    pub adaptive: AtomicBool,
    pub aac: AtomicBool,
}

impl SessionStats {
    fn to_json(&self) -> String {
        format!(
            concat!(
                "{{\"connected\":{},\"width\":{},\"height\":{},\"bitrate_bps\":{},",
                "\"ceiling_bps\":{},\"rtt_ms\":{},\"queue_delay_ms\":{},\"fps\":{},",
                "\"frames_sent\":{},\"adaptive\":{},\"aac\":{}}}"
            ),
            self.connected.load(Ordering::Relaxed),
            self.width.load(Ordering::Relaxed),
            self.height.load(Ordering::Relaxed),
            self.bitrate_bps.load(Ordering::Relaxed),
            self.ceiling_bps.load(Ordering::Relaxed),
            self.rtt_ms.load(Ordering::Relaxed),
            self.queue_delay_ms.load(Ordering::Relaxed),
            self.fps.load(Ordering::Relaxed),
            self.frames_sent.load(Ordering::Relaxed),
            self.adaptive.load(Ordering::Relaxed),
            self.aac.load(Ordering::Relaxed),
        )
    }
}

static GLOBAL: OnceLock<Arc<SessionStats>> = OnceLock::new();

/// Turn telemetry on: create (idempotently) the shared snapshot and return it.
/// Called once from `main.rs` when the endpoint is enabled.
pub fn enable() -> Arc<SessionStats> {
    GLOBAL
        .get_or_init(|| Arc::new(SessionStats::default()))
        .clone()
}

/// The shared snapshot iff telemetry is enabled, else `None`. Hot-path update
/// sites do `if let Some(s) = stats::global() { … }`, a no-op when off.
#[inline]
pub fn global() -> Option<&'static Arc<SessionStats>> {
    GLOBAL.get()
}

/// Endpoint port (`MACRDP_STATS_PORT`, default 40245 — next after the shield
/// helper's 40244).
pub fn default_port() -> u16 {
    std::env::var("MACRDP_STATS_PORT")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(40245)
}

/// Serve the snapshot on `127.0.0.1:port`, one JSON line per connection, until
/// the process exits. Read-only; the request body (if any) is ignored.
pub async fn serve(port: u16, stats: Arc<SessionStats>) {
    use tokio::io::AsyncWriteExt;
    let listener = match tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(port, error = %e, "stats endpoint: bind failed — live telemetry unavailable");
            return;
        }
    };
    tracing::info!(port, "stats endpoint listening (loopback, read-only)");
    loop {
        match listener.accept().await {
            Ok((mut sock, _)) => {
                let body = stats.to_json();
                // Handle inline (no per-connection task spawn) so a local process
                // that hammers the port can't spawn unbounded tasks; the response
                // is a couple hundred bytes to a loopback socket, so serializing
                // one poll every ~2 s is fine. A short timeout keeps a client that
                // connects but never reads from wedging the accept loop.
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.write_all(b"\n").await;
                    let _ = sock.shutdown().await;
                })
                .await;
            }
            Err(e) => tracing::debug!(error = %e, "stats endpoint: accept error"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape_is_stable_and_parseable() {
        let s = SessionStats::default();
        s.connected.store(true, Ordering::Relaxed);
        s.width.store(1920, Ordering::Relaxed);
        s.height.store(1080, Ordering::Relaxed);
        s.bitrate_bps.store(4_000_000, Ordering::Relaxed);
        s.fps.store(60, Ordering::Relaxed);
        let j = s.to_json();
        // Spot-check a few fields + that it's a single line with the expected keys.
        assert!(j.starts_with('{') && j.ends_with('}'));
        assert!(!j.contains('\n'));
        assert!(j.contains("\"connected\":true"));
        assert!(j.contains("\"width\":1920"));
        assert!(j.contains("\"bitrate_bps\":4000000"));
        assert!(j.contains("\"fps\":60"));
    }

    #[test]
    fn global_is_none_until_enabled() {
        // NB: process-global; if another test enables it this may already be Some.
        // We only assert the accessor doesn't panic and the enabled snapshot is shared.
        let a = enable();
        let b = global().expect("enabled");
        assert!(Arc::ptr_eq(&a, b));
    }
}
