# Two UDP tunnels: lossy audio and EGFX on UDP in one session

Status: **built, not yet live-tested** (2026-11-24): clintcan/IronRDP#4 on `macrdp-79e763a0`, points 1–7
below. Point 8 (the tunnel's inbound data after an in-place reactivation) is left for a follow-up: when it
bites, the EGFX watchdog moves video back to TCP. The bind timeout is 5 s.

## The gap

`--enable-lossy-audio` and `--udp-migrate-egfx` each move one dynamic channel onto a UDP tunnel, but a
connection gets one tunnel and one Soft-Sync. When both are on, lossy audio wins
(`mt_soft_sync_on_egfx` returns after the audio Soft-Sync) and EGFX stays on TCP. This is the known issue
listed in the release notes.

## What is already known

- **mstsc accepts a second tunnel.** The 2026-11-16 spike (local stash `spike: second reliable UDP
  offer`, branch `spike/two-udp-tunnels`) sent a second Initiate Multitransport Request (`UdpFecR`) on the
  message channel after mstsc's S_OK to the first (`UdpFecL`). mstsc answered S_OK in 51 ms and bound the
  reliable tunnel 250 ms later, next to the lossy one; lossy audio kept playing.
- **One Soft-Sync can name both tunnels.** The Soft-Sync Request carries a list of
  `SoftSyncChannelList` (tunnel type + channel ids); `ironrdp-dvc`'s `SoftSyncRequest::new` already takes
  a `Vec`. Our `DrdynvcServer::request_soft_sync` (dvc divergence (3)) takes one tunnel today.
- **The listener needs no change.** It keys peers by source address and binds each by cookie, so two
  flows from one client are two peers.
- **Not yet shown:** that mstsc honours a Soft-Sync naming two tunnels in one request. The spike only
  proved both tunnels bind. This is the first thing to test.

## Design

Off by default: it engages only when both `--enable-lossy-audio` and `--udp-migrate-egfx` are on, with
`MACRDP_UDP_SECOND_TUNNEL=0` as a kill switch. With either flag off, the code path is unchanged.

1. **Offers.** The first offer stays as today (`UdpFecL` when lossy audio is on). On the client's S_OK
   to it, send a second request (`UdpFecR`) on the message channel, with its own CSPRNG cookie registered
   in the `CookieRegistry`, as the spike did. Same RTT gate and tunnel-death cooldown as the first: if the
   first offer was withheld, so is the second.
2. **Per-tunnel state.** Replace `MtConnection`'s single `tunnel_bound` / `tunnel_inbound_rx` / migration
   cookie with one record per tunnel:
   `struct Tunnel { protocol, cookie, bound: Arc<AtomicBool>, inbound_rx, registration }`, held as
   `lossy: Option<Tunnel>` and `reliable: Option<Tunnel>`.
3. **One Soft-Sync, two lists.** Add `DrdynvcServer::request_soft_sync_lists(Vec<SoftSyncChannelList>)`
   (same open-channel and one-shot checks; the existing method becomes a one-list call). The gate in
   `mt_soft_sync_on_egfx` waits until both tunnels are bound and both channels (`AudioLossyDvc`,
   `GfxDvcBridge`) are open, then sends `LOSSY_UDP → [audio]` and `RELIABLE_UDP → [egfx]` together.
4. **Fallback if the second tunnel never binds.** After a deadline (about 5 s from the second offer's
   S_OK, or a failed or missing response), Soft-Sync audio alone, exactly as today. EGFX stays on TCP. The
   one-Soft-Sync rule means the gate must decide once; it never sends a second Soft-Sync later.
5. **Routing.** `route_dvc_over_udp` takes the tunnel: lossy-audio waves go to the lossy cookie, EGFX to
   the reliable cookie. `lossy_audio_target`'s per-wave bound check reads the lossy tunnel's flag; the
   EGFX path reads the reliable one's.
6. **Failure handling, per tunnel.** The EGFX watchdog and the de-migrate on resume from minimize act on
   EGFX only, as now. A dead lossy tunnel drops audio back to TCP without touching EGFX, and a dead reliable
   tunnel triggers the EGFX watchdog without touching audio. Check that the tunnel-death cooldown
   (`CookieRegistry` suppression) isn't tripped for both by one tunnel's death.
7. **Teardown.** `MtConnection`'s `Drop` retires both tunnels: lower both bound flags and evict both
   cookies, or the listener reads the idle one as dead and starts the cooldown for the next connection.
8. **Fold in a related pre-existing gap.** The tunnel's inbound receiver is taken once per client loop
   (`TunnelInbound::take`), so after an in-place Deactivation-Reactivation (blank recovery) data the client
   sends over a tunnel is dropped. Keep the receivers in the connection state so a reactivation re-attaches
   them. With two tunnels this matters more, since EGFX frame acks ride the reliable one.

## Tests

- Fork unit tests (`mt/session.rs`): two offers; Soft-Sync with two lists only when both are bound and
  open; audio-only fallback at the deadline; routing by tunnel; `Drop` retires both; one tunnel's death
  leaves the other's channel alone.
- `ironrdp-dvc`: `request_soft_sync_lists` rejects unopened channels and a second call.
- macrdp `conn_test`: with both flags on, the client sees two offers and one Soft-Sync with two lists;
  with either off, today's single offer (regression guard).

## Live checks (mstsc only)

FreeRDP has no UDP and Windows App for macOS is TCP-only. Windows App on Windows is untested for UDP.

- LAN: audio on the lossy tunnel and EGFX on the reliable one (log both), sound and picture normal.
- Minimize and restore (EGFX de-migrates to TCP on resume; audio stays on UDP).
- A blank-recovery reactivation: frame acks keep arriving over the reliable tunnel (point 8).
- Kill one flow (block the reliable port): EGFX falls back, audio continues; and the reverse.
- Disconnect and reconnect: no cooldown tripped by the idle tunnel.
- ZeroTier: the RTT gate still withholds both offers.

## Value

Modest. On a LAN, EGFX over UDP gains little over TCP, which is why this was parked. It removes a
documented limitation for mstsc users who turn on both options, and it is a self-contained piece of the
fork with a clear test plan.
