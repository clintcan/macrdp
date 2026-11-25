# IronRDP divergences

macrdp takes every `ironrdp-*` crate from [clintcan/IronRDP](https://github.com/clintcan/IronRDP),
branch `macrdp-79e763a0`, at the rev in the root `Cargo.toml` `[patch.crates-io]`. That branch is
upstream Devolutions/IronRDP@79e763a0 plus macrdp's divergences in `ironrdp-server`,
`ironrdp-acceptor`, `ironrdp-rdpdr` and `ironrdp-dvc`, one commit per divergence, and one commit that
drops the workspace lints from two crates (see "Lints" below). The subject has the crate and the
number, for example `server: (35) measure bandwidth at connect time and report it`.

The old branch `macrdp` (upstream e258f6a0 plus the divergences, last commit fd3798d3) stays: v0.10.7
to v0.10.9 build from it, and v0.10.10 onward from `macrdp-79e763a0`. Its log is in this file's git
history. The fork keeps only `master` and the `macrdp*` branches; a feature branch is deleted once its PR
is merged (the PR keeps a "Restore branch" button).

To change a divergence: make the commit on the branch, then bump the rev in `Cargo.toml` (every
`ironrdp-*` entry, so that all crates come from one source), and update the log below. The same rev
goes in `vendor/macrdp-rdpeudp/Cargo.toml` and `vendor/macrdp-rdpeudp/fuzz/Cargo.toml`, which pull
`ironrdp-core` when built on their own.

## Keeping the fork safe

macrdp builds whatever the pinned rev contains, so the fork is part of the supply chain:

- **Never delete, rename or make private clintcan/IronRDP.** Every release from v0.10.8 on is
  built from it, so a release tag stops building the day the repo goes.
- **The `macrdp` and `macrdp-*` branches are protected by two repository rulesets:** nobody can
  force-push or delete them (no bypass, not even the owner), and only an admin can create or update
  them. A contributor's change arrives as a pull request into the branch, which the owner merges.
  After a rebase onto a newer upstream, the new branch is named `macrdp-<upstream-rev>` and the old
  one stays, so older releases still build.
- **GitHub Actions are disabled on the fork.** It would otherwise run IronRDP's own workflows,
  including PR automation that holds a write token. macrdp's CI tests the fork's server crate.
- **`scripts/check-ironrdp-pin.sh`** checks that every IronRDP git dependency in a tracked
  `Cargo.toml` names clintcan/IronRDP with a full 40-hex rev, that all of them are the same commit,
  that `Cargo.lock` agrees, and that the commit is on a `macrdp*` branch. The branch check matters
  because GitHub serves any commit of the IronRDP fork network through clintcan/IronRDP's URL, and
  cargo-deny checks only the URL. It runs in CI, in the daily security workflow (from `main`'s
  copy, so a bad pin that got past review is still caught) and before every release build. A rev
  that is on a branch but isn't its tip passes with a warning: older tags must stay buildable, but
  in review an older commit may mean a missing fix.

Paths below are in clintcan/IronRDP (`crates/ironrdp-<crate>/src/...`) unless they name macrdp's
`src/`. The reasoning behind the server divergences (live-debugging notes, dead ends, what NOT to
do) is in [`ironrdp-divergence-history.md`](ironrdp-divergence-history.md), frozen at the old
vendored fork. The numbers are shared, so (N) here is (N) there. Read the history entry before you
change a divergence.

Gaps found in the review of this rebase that were already on the old branch (they are not
regressions) are listed in `TODO.md`, "Pre-existing gaps found reviewing the 79e763a0 rebase".

### Lints

The last commit on the branch removes `[lints] workspace = true` from `ironrdp-server` and
`ironrdp-rdpdr`. Under IronRDP's workspace lints, macrdp's code in them warns (server: mostly
`mt/listener.rs`, `mt/audio_dvc.rs`, `rdpdr_drive.rs`; rdpdr: `multiple_inherent_impl` from the
`server_direction.rs` shape). `ironrdp-acceptor` and `ironrdp-dvc` have no such warnings and keep the
lints. Upstream's own manifests set `[lib] test = false` for `ironrdp-server`, `ironrdp-acceptor` and
`ironrdp-dvc`; the fork does not change that. `cargo test -p <crate> --lib` still runs the lib tests.

## ironrdp-server

### Shape

Upstream's files, plus macrdp's own files, wired in by hook edits marked `(macrdp N)`:

- **Our files:** `macrdp.rs`, which `server.rs` declares as its child module (`#[path]`), so it can
  use `RdpServer` and `ConnectionState` private fields. It holds `MacrdpServerState`,
  `MacrdpChannelFactories`, `install_macrdp_channels`, the dispatch helpers, and the code of (2)(3)(8),
  (4), (15), (18), (20), (30) and (35). Also `audin.rs` (25), `rdcamera.rs` (19), `rdpdr_drive.rs`
  (11), `rdpeusb.rs` (16), and `mt/` (12): `mod.rs`, `listener.rs`, `dtls.rs`, `audio_dvc.rs`, and
  `session.rs`, which `macrdp.rs` declares as its child module.
- **Hook edits in upstream files:** `server.rs` (one `macrdp` field on `RdpServer`, one on
  `ConnectionState` with `multitransport`, short calls at each hook point, the `ServerEvent::RdpdrDrive`
  and `ServerEvent::Urbdrc` variants, the `on_authenticated` and `on_client_fingerprint` trait methods),
  `builder.rs` (one `macrdp_channels` field, the `with_*` methods, one `install_macrdp_channels` call
  in `build()`), `lib.rs` (module declarations and exports), `gfx.rs` (14), `sound.rs` (2/8),
  `autodetect.rs` (35).
- **Channels attach through upstream's factories.** `install_macrdp_channels` pushes (11)'s RDPDR
  processor as an upstream `StaticChannelFactory`, and the audio DVCs (12), URBDRC (16), camera (19)
  and microphone (25) as upstream `DynamicChannelFactory`s, in that DVC order (as before). Per-device
  channels (16)(19) open with upstream's `ServerEvent::CreateDynamicChannel`. `attach_channels` has no
  macrdp edit.
- **Cargo deviations:** the `multitransport` feature + `macrdp-rdpeudp` (git, clintcan/macrdp; macrdp
  patches it to `vendor/macrdp-rdpeudp`) / `getrandom` / `boring`; `anyhow` (our modules' error type);
  macOS `libc` (15); `usb` drops `dep:ironrdp-rdpeusb`, which is a regular dependency here because
  `rdpeusb.rs` uses it; no `[lints]` (see "Lints").

**Coexistence rule.** Upstream has its own RDPDR server (`with_rdpdr_factory`), microphone
(`with_rdpeai_factory`), USB (`usb` feature, `with_usb_factory`) and UDP multitransport
(`with_udp_transport`). They stay inert because macrdp never sets them. Never set one together with
macrdp's counterpart. For URBDRC and AUDIO_INPUT, both register the same DVC and the client sees it
twice. For RDPDR, upstream's processor is attached first and the acceptor refuses ours ("Static channel
name is already registered"), so macrdp's drive and smart card stop working. `install_macrdp_channels`
logs a warning at build time for each such pair (RDPDR, AUDIO_INPUT, and URBDRC with `usb`), and
`set_multitransport_provider` warns when `with_udp_transport` is also set. Our clashing names were
renamed for this (11): `RdpdrDriveServerFactory`, `RdpdrDriveMessage`, `RdpdrDriveServer`,
`ServerEvent::RdpdrDrive`, `with_rdpdr_drive_factory`; USB's builder method is `with_urbdrc_factory`.

### Kept divergences

| # | What | Where | macrdp's API |
|---|---|---|---|
| (2)(3)(8) | Audio on its own bounded channel (50); drop-stale lag model with resync after a writer stall; per-wave duration for AAC | `macrdp.rs` `AudioDispatch` (one more arm in `server.rs` `client_loop_queued`), `AudioLagModel`, `discard_stale_macrdp_audio` (called from `discard_stale_session_events`); `sound.rs` `AudioWave`, `set_audio_sender` | `SoundServerFactory::set_audio_sender` |
| (4) | Batch priority: CLIPRDR, then EGFX + rest, then RDPDR | `macrdp.rs` `prioritize_batch`, called in `dispatch_server_events` before upstream's deferral loop | — |
| (11) | Our RDPDR drive + smart card processor | `rdpdr_drive.rs` (`RdpdrDriveChannelFactory`); the `ServerEvent::RdpdrDrive` arm → `macrdp::dispatch_rdpdr_drive` | `with_rdpdr_drive_factory`, `RdpdrHandle` |
| (12) | Our UDP multitransport (RDPEUDP + DTLS lossy audio + Soft-Sync + watchdog de-migration) | `mt/` (`session.rs` holds the server side); hooks in `prepare_pending_connection`, `accept_finalize`, `handle_message_channel_data`, `process_x224`, the `Egfx` arm of `dispatch_server_events`, a tunnel arm in `client_loop_queued` | `set_multitransport_*`, `set_egfx_on_*_handle`, `set_demigrate_request_handle`, `set_migrate_egfx` |
| (14) | EGFX decline flag (no CapabilitiesConfirm for a no-AVC client) | `gfx.rs` `GfxDvcBridge::with_decline_flag`, `process` | `GfxDvcBridge::with_decline_flag`, called in macrdp's `GfxServerFactory::build_server_with_handle(GfxContext)` |
| (15) | Kernel TCP RTT at accept | `macrdp.rs` `tcp_srtt_ms`, `store_link_rtt`; `TcpConnectionSource::accept` → `IncomingConnection.link_rtt_ms`; stored in `serve_connection` (fresh) and `finalize_negotiated` (preemption winner) | `set_link_rtt_handle` |
| (16) | Our URBDRC USB processor | `rdpeusb.rs` (`UrbdrcChannelFactory`); the `ServerEvent::Urbdrc` arm → `macrdp::dispatch_urbdrc` | `with_urbdrc_factory`, `UsbHandle` |
| (18) | `ConnectionHandler::on_authenticated` | `complete_security_upgrade`, through `ServerCredentialsHandler.auth_observer` and `macrdp::report_authentication`; candidates through `NegotiationContext.connection_handler` | trait method |
| (19) | MS-RDPECAM camera | `rdcamera.rs` (`RdCameraChannelFactory`) | `with_camera_factory` |
| (20) | Client-fingerprint log + `on_client_fingerprint` | top of `client_accepted` (not on reactivation), `macrdp::report_client_fingerprint` | trait method |
| (25) | Our MS-RDPEAI microphone | `audin.rs` (`AudinChannelFactory`) | `with_audin_factory` |
| (30) | An auto-reconnect cookie that fails verification falls back to a normal logon instead of denying the connection | `client_accepted`, the auto-reconnect check; `macrdp.rs` `is_stale_reconnect_cookie` | — |
| (35) | Optional Connect-Time Auto-Detection when auto-detect is enabled: an RTT probe, a 256 KiB bandwidth burst and a Network Characteristics Result between Client Info and licensing; later results report this connect-time bandwidth instead of the continuous figure. Based on clintcan/macrdp#235 by @JulyStar-Lv. Pairs with acceptor divergence (7) | `macrdp.rs` `connect_time_autodetect`, `await_autodetect_reply`, `connect_time_autodetect_hook`; the `accept_finalize_with_hooks` call in `accept_finalize`; `autodetect.rs` `connect_time_*`, `PendingBandwidth::ConnectTime`, `build_netchar_result` | `--network-autodetect` (macrdp: `enable_autodetect()` + a 1 s `ServerEvent::AutoDetectRttRequest` ticker) |
| (36) | Pass the monitor-layout option to every connection and report the adopted layout (multi-monitor, with acceptor (8)); before a reactivation, ask the display for the layout it now serves | `server.rs` `PendingConnection::new`, `RdpServerOptions::honor_client_monitor_layout`, `ConnectionInfo::monitor_layout`, the `DeactivationReactivation` arm, `BoundDisplaySlot::monitor_layout`; `builder.rs`; `display.rs` | `with_honor_client_monitor_layout(bool)`, `ConnectionInfo::monitor_layout`, `RdpServerDisplay::monitor_layout()` |

Notes on the port to 79e763a0. Upstream replaced the per-connection `Rc<Mutex<Self>>` and the
parallel dispatch futures with one `select!` loop over a FIFO outbound queue (#2349, #2431, #2312),
and moved per-connection state into `ConnectionState` (#2059). So every hook moved, and the server
code of (2)(3)(8) and (12) was rewritten for that shape, with the same wire behaviour unless a note
says otherwise.

- **(2)(3)(8)** The audio arm takes the next wave only after the previous one is written (it holds
  a permit that upstream's `WriteCompletion::Rdpsnd` releases), and it is not gated on `queue_idle`.
  The lag model and its constants (200 ms, 300 ms resync) are unchanged. `discard_stale_session_events`
  also drains the audio channel, so the evicted session's waves don't play into the winner's. Waves
  are dropped quietly until RDPSND negotiates (`is_ready()`).
  **Behaviour changes:** (a) A closed audio channel no longer ends the session: the arm switches off
  and the connection continues. The channel exists only when a sound factory is set. macrdp's
  `MacRdpsnd` keeps the sender for the life of the server, so production is not affected. (b) A wave
  now waits behind every byte already in upstream's FIFO outbound queue, which can be a whole EGFX
  batch dispatched just before it. The old per-PDU writer lock let a wave go out between two frames
  of one batch. A wave is never queued behind frames queued after it. The latency effect has not been
  measured live yet (see the live-test checklist in `TODO.md`).
- **(4)** A stable `sort_by_key` by tier. Upstream's own `Rdpdr` events and our `RdpdrDrive` sort
  last; `Urbdrc` and `CreateDynamicChannel` are in the middle tier. The sort runs before upstream's
  deferral gate, which holds validity-guarded events and waves while bytes are queued.
  **Behaviour change:** a `ClipboardWithValidity` sorted first holds the rest of the batch until the
  writer drains. macrdp sends only plain `ServerEvent::Clipboard` today, so nothing sends that variant.
  If macrdp starts to use `ClipboardWithCompletion` or `ClipboardWithValidity`, add them to the first
  tier, or a plain `Clipboard` can overtake them on the CLIPRDR channel.
- **(11)** `rdpdr_drive.rs` is the old file plus the factory wrapper and tests;
  `RdpdrDriveServerFactory` now requires `Send` (upstream's `StaticChannelFactory` needs it).
  Device I/O requests keep our own `ServerEvent::RdpdrDrive` arm, which sends them with no channel
  flags. Do not move them to upstream's `StaticChannelWithCompletion`: it adds `SHOW_PROTOCOL` above
  1600 bytes, and Windows drops a multi-chunk RDPDR PDU that carries it. `decode_write_response` keeps
  `DR_WRITE_RSP`'s padding byte optional, as MS-RDPEFS 2.2.1.5.4 says; upstream's decoder requires it.
  Upstream-issue candidate. Upstream's `RdpdrServer` has no smart card path, so it cannot replace ours.
- **(12)** Configuration stays on `RdpServer` (`MtState`); each connection's state is an
  `MtConnection` in `ConnectionState`, and its `Drop` retires the tunnel (lowers the handles and the
  bound flag, evicts the cookie) on every entry point. The offer uses acceptor (5): `OfferRng`
  (`MultitransportSecurityRng`) registers each cookie with the listener as it is generated; the offer
  is `flag | SOFT_SYNC_TCP_TO_UDP` plus `set_multitransport_requested_protocol` for lossy;
  `MigrationState` comes from `acceptor.multitransport_request()` after each finalize. Soft-Sync goes
  through dvc (3)'s `request_soft_sync` (same wire bytes, `SHOW_PROTOCOL` included). Upstream's
  message-channel handler consumes the Initiate Multitransport Response first, so ours is
  `mt_on_response` in its arm, and the owed empty Soft-Sync is a `ConnectionOutput::ResponseFrame`
  appended in `process_x224`. The two audio DVCs have distinct types (`AudioReliableDvc`,
  `AudioLossyDvc`), because the DRDYNVC server's by-type lookup keys on the Rust type. Our
  `egfx_on_udp` lives in the `mt` state; upstream has its own field of that name, which stays false.
  Upstream's own EGFX Soft-Sync stays inert, because it needs a `udp_transport` that only
  `with_udp_transport` creates.
  **Two tunnels (clintcan/IronRDP#4, 2026-11-24):** with lossy audio and EGFX migration both on
  (`MtConfig::second_tunnel`), the client's S_OK to the first (`UdpFecL`) offer owes a second
  Initiate Multitransport Request (`UdpFecR`), sent on the message channel from
  `mt_with_owed_soft_sync` (which now takes `message_channel_id`) with its own CSPRNG cookie,
  registered with the listener and sharing the first tunnel's inbound sender (`ReliableTunnel`).
  The Soft-Sync gate waits for it to bind (`SECOND_TUNNEL_BIND_TIMEOUT`, 5 s) and then sends one
  request with two lists (dvc (3)'s `request_soft_sync_lists`): lossy audio on the lossy tunnel,
  EGFX on the reliable one. A refusal or a timeout falls back to audio alone, as before. EGFX data
  and replies to tunnel data use the reliable tunnel's cookie (`egfx_tunnel_cookie`); `Drop`
  retires both tunnels. `MACRDP_UDP_SECOND_TUNNEL=0` turns it off.
  **clintcan/IronRDP#5 (2026-11-24):** the client loop hands the tunnel's inbound receiver back when it
  ends (`TunnelInbound::restore`), so a reactivation keeps reading the tunnel (a monitor move left the
  picture blank with EGFX on the tunnel); and the two-tunnel Soft-Sync goes out from the client's
  dynamic-channel data as soon as it can (`mt_soft_sync_after_dvc_input`, one hook line in
  `process_x224`), since waiting for the first EGFX batch stalled mstsc for about 4 s.
  The multitransport offer needs the MCS message channel (since e258f6a0; the older fork fell back to
  the I/O channel). mstsc sends it; a client that doesn't gets no UDP offer.
  **The one Soft-Sync (fix of 2026-11-13, silent lossy audio since v0.10.0):** mstsc answers the
  offer with `S_OK` on the message channel. When a channel is configured to migrate
  (`has_channel_to_migrate`: lossy audio or EGFX), the response leaves the connection's one Soft-Sync
  to the migration gate; otherwise it gets the empty one. Waves stay on TCP (`mt_route_lossy_audio`
  returns them to the static RDPSND path) until the Soft-Sync naming the lossy channel has been sent
  AND acknowledged. Live-verified on mstsc at e258f6a0; the fork's lib tests now cover both rules
  (`mt_session::tests`).
  Known, not changed by the port: upstream brackets bandwidth only on its own UDP writes
  (#2031, `cb7ca376`), so no measurement runs while EGFX rides macrdp's tunnel.
- **(14)** Unchanged. Upstream's EGFX server still always confirms, and its handler can't veto.
- **(15)** A fresh connection is now sampled in `TcpConnectionSource::accept`, a preemption candidate
  at its accept too (a little earlier than before). The custom-stream entry points (`run_connection*`,
  `run_connection_handshake`) don't touch the cell. The macOS `getsockopt` body is the old one.
  Upstream's `AutoDetectHandles::rtt` has no value yet when the offer is decided, so it can't replace
  this.
- **(16)** Upstream keeps a completion's raw `TS_URB_RESULT` bytes private, so
  `urb_completion_no_data_result` re-reads SelectConfiguration's result from the PDU, only after
  upstream's decode has validated the layout (now a lib test against upstream's encoder). Drop it once
  upstream exposes the bytes or a typed result.
  **Stricter decode:** upstream decodes every `URB_COMPLETION` result as isoch (`into_isoch()`), so a
  *non-isoch* completion carrying a non-empty result payload fails the whole PDU. Watch the USB live
  test for decode errors. Transfers keep our own `ServerEvent::Urbdrc` arm, which drops a transfer on
  an encode error instead of ending the session. `UrbdrcServerMessage::OpenDeviceChannel` is gone
  (upstream's `CreateDynamicChannel` replaces it).
- **(18)** Fires for preemption candidates too. Under HYBRID_EX, a credential-validator rejection
  fails `accept_credssp_with`, so it is reported as `on_authenticated(false, ..)` (macrdp sets no
  validator).
- **(19)** Upstream's new camera server (`ironrdp-rdpecam`, #2445) is not used: it keeps one
  SampleRequest in flight (ours keeps 4, or the stream stalls) and does not prefer H.264. Each device's
  sample sink is built when its channel is created, as before. `CameraServerMessage` and
  `ServerEvent::Camera` are gone.
- **(20)** On 79e763a0 the log also runs before upstream's connection-binder and "credentials were
  not prepared" denials, so those attempts are fingerprinted too.
- **(25)** Protocol version 1, and it opens the mic by itself from `process()`. Upstream's
  `RdpeaiServer` (version 2, opened by an event from the embedder) is not used.
- **(30)** Added 2026-10-28 after a live FreeRDP test: restarting macrdp with a client connected made
  it auto-reconnect with the old process's cookie, which never verifies. Upstream denied it, FreeRDP
  retried about three times a second, and `auth_guard` locked out the client's address. MS-RDPBCGR
  3.3.5.3.11: "If logon with the cookie fails, the credentials supplied in the Client Info PDU SHOULD
  be used". On 79e763a0 the acceptor skips the credential validator for Client Info credentials when
  the client sent a cookie. So:
  - with a credential validator, the fallback is denied ("credentials were not prepared before
    activation") unless CredSSP under HYBRID_EX already validated the credentials (for example
    TLS-only security + a validator: denied);
  - with a connection binder, the cookie is still denied, as upstream does (the binder never runs for
    a reconnect attempt);
  - macrdp sets neither, so for macrdp the fallback logs on normally, after CredSSP.
  Tests: `ironrdp-testsuite-core` `tests/server/macrdp_auto_reconnect.rs` (fallback, validator,
  binder) and macrdp's `conn_test::a_stale_auto_reconnect_cookie_falls_back_to_a_normal_logon`.
  Upstream candidate (Devolutions/IronRDP#2465 is a fix for our #2280).
- **(35)** Added 2026-11-21 (clintcan/IronRDP#2, from clintcan/macrdp#235). Before upstream #2021,
  upstream measured bandwidth only by bracketing ordinary traffic. On a server that sends less than the
  link carries, that reports the send rate: live on a 1 ms LAN (at e258f6a0), mstsc showed 2.90 Mbps
  and Windows App 3.60 Mbps, and both warned "Your network might be slow". The connect-time burst
  (32 × 8 KiB; 16 KiB payloads made mstsc fail with 0x1304) measured 95.42 and 91.27 Mbps, and both
  clients then reported "good". Safety: the connection's own `AutoDetectManager`, reset first; each
  reply awaited at most 2 s (`CONNECT_TIME_AUTODETECT_REPLY_TIMEOUT`); other frames skipped; inside
  `FINALIZE_TIMEOUT`; an encoding failure skips the measurement, only a failed write ends the logon.
  The connect-time Stop needs a non-empty payload. On 79e763a0 the figure comes from upstream's
  `measured_bandwidth_kbps`, and the connect-time measurement does not update `AutoDetectHandles`.
  Tests: `autodetect::connect_time_tests` (run by the CI server-test step) and macrdp's
  `conn_test::network_autodetect_reports_the_connect_time_bandwidth`. Upstream candidate (together
  with acceptor (7)). Live on this base (2026-11-24, v0.10.10 on the mini): mstsc and Windows App on
  Windows showed 91.27 and 95.42 Mbps, "good", with no noticeable login delay.
  **Open follow-up:** results still always report the connect-time figure. Upstream #2021 (in
  79e763a0) now brackets a single large EGFX write, so in-session measurements are better than before,
  and always preferring the connect-time figure hides them. Changing the rule to "connect-time until a
  bracketed measurement exists" is a separate change (agreed in clintcan/IronRDP#3).

### Upstream changes at 79e763a0 that macrdp sees

- **Logon notification (#2478):** at its first activation, each connection now gets a Save Session
  Info PDU (logon notification), before the auto-reconnect cookie. macrdp does not call
  `with_logon_notification_on_request`, so it is sent at activation.
- **`RdpServerDisplay::updates(DisplayContext)` (#2073):** the server makes a new SuppressOutput flag
  for each connection and hands it in `DisplayContext::display_suppressed`; the builder's
  `with_display_suppressed_handle` (retired (5) below) is gone. macrdp's `capture::SuppressedFlag`
  holds the current connection's flag for the capture and audio readers.
- **`GfxServerFactory` methods take a `GfxContext` (#2104).** macrdp's `h264.rs` takes it and ignores it.

### Retired at 79e763a0 (upstream has it; what macrdp calls instead)

- **(24) input-reset flag** → macrdp's `conn_hooks::InputResetHook`, in
  `ConnectionHandler::on_connection_info`, which the server calls once per served connection (not on a
  Deactivation-Reactivation, not for a losing preemption candidate). `set_input_reset_handle` is gone.
  The flag is now raised after finalize, not at the top of the serving path. Input that arrives during
  finalize is buffered by the acceptor and handed over after it, so no input reaches the handler first.
- **(26) client input read off the `Self` lock** → nothing to call. Upstream's single select loop
  reads input while the outbound queue writes (#2349, #2431, #2312), and the acceptor buffers input in
  flight during a reactivation and replays it (#2439). Reads pause while queued replies exceed 1 MiB.
- **(28) zstd-safe 8.2** → nothing in the fork. Upstream still requires `zstd-safe = "7.2"`; macrdp's
  `Cargo.lock` has 7.4.1, which the RustSec advisories list as patched ("^7.4.0 OR >=8.2.0").
- **(29) AND mask for 32 bpp pointers** → upstream #2108 (`fb25b47c`).
- **(31) zero-payload fast-path update as one empty frame** → upstream #2274 (`904a7da6`). macrdp's
  `conn_test::hidden_and_default_pointer_updates_reach_the_client` stays as the regression check.
- **(32) serving-path negotiation timeout** → upstream #2432 (`e482e340`), `NEGOTIATION_TIMEOUT` (60 s)
  around `negotiate_and_authenticate`, on every entry point. macrdp's
  `conn_test::a_silent_peer_cannot_hold_the_serving_connection` stays as the regression check.
- **(33) client scale factors on `ConnectionInfo`** → upstream #2107 (`335d6712`),
  `ConnectionInfo::{desktop,device}_scale_factor`. macrdp's `conn_hooks::DesktopScaleHook` is
  unchanged.
- **(34) RemoteFX in a Stream Surface Bits command** → upstream #2229 (`e20a2147`). Windows 10 mstsc
  shows a black session for RemoteFX in Set Surface Bits; the history is in `known-quirks.md`.

### Retired at e258f6a0 (upstream has it; what macrdp calls instead)

- **(5) SuppressOutput** → builder `with_display_suppressed_handle` (gone at 79e763a0, see above).
  **Behaviour change:** upstream's capabilities ADVERTISE refresh-rect/suppress-output support,
  which our fork never did, so spec-following clients (FreeRDP, Thincast) now send
  minimize signals too, and macrdp's mute + video pause applies to them.
- **(6) NSCodec** → upstream's `ironrdp-nscodec` behind the `nscodec` feature, which
  the root `Cargo.toml` enables. Without it the macOS Windows App silently drops to
  raw bitmaps (`conn_test.rs`'s NSCodec negotiation is the canary).
- **(9) honor client desktop size** → builder `with_honor_client_desktop_size(Option<DesktopSize>)`.
- **(10) keyboard-layout cell** → `ConnectionHandler::on_connection_info`.
- **(13) auto-reconnect cookie** → builder `with_auto_reconnect_cookie`. **Behaviour
  change:** upstream verifies a returning cookie (HMAC), rotates it on every
  reactivation and hourly, and invalidates it on eviction. A client reconnecting after
  a macrdp *process* restart was denied, which looped the client into an auth lockout;
  divergence (30) turns that denial into a normal logon.
  The cookie is also re-sent and rotated on every Deactivation–Reactivation pass (each
  live resize and each blank-recovery heal). That is safe because the acceptor `take()`s
  the received cookie, so a reactivation never re-verifies, but it's more wire traffic
  than before.
- **(21) mouse position before button** → `MouseEvent::Button { x, y, button, pressed }`
  (upstream #1769, fixing our report #1466). Breaking: the old positionless variants are gone.
- **(22)/(23) preemption** → builder `with_connection_policy(ConnectionPolicy::Preempt)`.
  The default is `Queue`, so omitting the call silently brings back the
  second-client hang, with no compile error. Upstream also invalidates the evicted
  client's cookie.
- **`ServerEvent::ClipboardFileCopy`** (was undocumented) →
  `ServerEvent::Clipboard(ClipboardMessage::SendInitiateFileCopy(files))`, whose arm
  is identical, including not ending the session on failure.
- **(27) `TCP_NODELAY`** (main, #222) → nothing to call: e258f6a0 sets it at both
  accept sites that serve a connection (the idle listener and the preemption race).
  The `Reject` policy's accept drops the socket unserved.
- Earlier: (1) PR #1276, (7) QOI at a5d1c682, (17) wheel decode at a5d1c682.

### Tests

The fork's server lib tests cover most kept divergences: `server::macrdp::tests` (audio arm, batch
priority, RDPDR drive, URBDRC, camera, mic, fingerprint, link RTT, authentication),
`server::macrdp::mt_session::tests` (with `multitransport`), `autodetect::connect_time_tests`, and the
tests in `gfx.rs`, `rdpdr_drive.rs`, `rdpeusb.rs`, `rdcamera.rs` and `audin.rs`. macrdp's CI runs them
(`cargo test --lib --features egfx` on the pinned checkout; `multitransport` is left out to keep the
job fast). (30) is tested in `ironrdp-testsuite-core`. Not covered by an automated test: a live
multitransport client, a successful NLA for (18), and (20)'s placement in `client_accepted`.

### Porting notes for the next rebase

- Keep upstream code as upstream wrote it. Put macrdp code in macrdp's files (`macrdp.rs` and the
  channel modules) and mark each hook edit in an upstream file with `(macrdp N)`.
- Prefer upstream extension points (channel factories, `CreateDynamicChannel`, `ConnectionHandler`
  methods) over edits to `server.rs` when they give the same wire behaviour.
- `ServerEvent` matches: our variants `RdpdrDrive` and `Urbdrc` need an arm in each exhaustive match;
  check new upstream variants against `prioritize_batch`'s tiers.

## ironrdp-acceptor

`src/` is upstream 79e763a0 except `connection.rs`, which carries (4), (5), (7) and (8), and `lib.rs`,
which carries (7)'s finalize driver. Every insertion is marked `(macrdp divergence N)`. The crate keeps
IronRDP's workspace lints and is lint-clean under them.

### Divergences

(4) **Client-fingerprint identity fields** (NOT upstreamed; added 2026-07-18).
    `Acceptor` captures `client_name: String`, `client_version: u32` (raw
    `RdpVersion.0`) and `client_build: u32` from the GCC Client Core Data in
    `BasicSettingsWaitInitial`, next to upstream's own `keyboard_layout` /
    `keyboard_type` / `ime_file_name` captures, carries them across
    deactivation–reactivation, and surfaces them as `pub` fields on `AcceptorResult`.
    Consumed by server divergence (20)'s client-fingerprint log. Same
    additive shape as #1397 (keyboard layout) — cleanly upstreamable. Heuristics: mstsc
    sends the real Windows build + its hostname; FreeRDP hardcodes build 2600.
    Informational only — a client can claim anything.

(5) **`Acceptor::set_multitransport_requested_protocol(RequestedProtocol)`** (NOT
    upstreamed; added 2026-10-01). Upstream's offer (#1951) only ever requests reliable
    UDP: `MultitransportBootstrapping` gates on `TRANSPORT_TYPE_UDP_FECR` and hard-codes
    `RequestedProtocol::UdpFecR`. macrdp's `--enable-lossy-audio` needs a lossy (`UdpFecL`)
    request. The setter (default `UdpFecR`, so upstream behaviour is unchanged when it isn't
    called) makes both gates — our advertised offer and the client's flags — and the request
    use the chosen protocol's transport flag. Upstream candidate: a natural extension of #1951.

(7) **Window for connect-time auto-detect** (NOT upstreamed; added 2026-11-21 in
    clintcan/IronRDP#2, from clintcan/macrdp#235). `Acceptor::connect_time_autodetect_window()`
    returns the message and user channel ids once the Client Info PDU has been received
    and before licensing, when the client advertised `SUPPORT_NET_CHAR_AUTODETECT` and
    joined the MCS message channel, never during a deactivation-reactivation.
    `accept_finalize_with_hooks()` is upstream's `accept_finalize_with_handlers()` plus a
    handler called once at that window, after the Client Info and credential handlers,
    inside the finalize future, so a timeout around finalize bounds it.
    `accept_finalize_with_handlers()` now calls it with a no-op handler, so its behaviour is
    unchanged. A client whose credentials are refused gets no burst. Used by server
    divergence (35). Upstream candidate.

(8) **Adopt a multi-monitor client's layout** (NOT upstreamed; added 2026-11-25 in
    clintcan/IronRDP#6, for macrdp's multi-monitor sessions, `docs/multi-monitor-design.md`).
    `Acceptor::set_honor_client_monitor_layout(bool)`, off by default. With it on and
    `set_honor_client_desktop_size` set, a client whose Client Monitor Data holds a usable
    layout (2 to 16 monitors, exactly one primary at (0,0), each 200 pixels to the operator
    maximum, no overlaps, a bounding box that is a valid desktop size; `adopt_monitor_layout`)
    gets a desktop the size of the bounding box, and a Monitor Layout PDU listing its monitors
    as it sent them, primary-relative and possibly negative, as xrdp and FreeRDP's server do.
    The operator maximum then applies per monitor. `AcceptorResult::monitor_layout` reports
    the adopted layout; server divergence (36) passes the option in and reports it in
    `ConnectionInfo`. If a later resize moves the desktop off the bounding box, the layout
    PDU falls back to one monitor. Tests: three unit tests in `connection.rs`
    (`a_side_by_side_layout_is_adopted_with_its_bounding_box` and two more) and macrdp's
    `conn_test::a_two_monitor_client_gets_its_monitor_layout`. Upstream candidate.
    **Mid-session layouts (2026-11-25, branch `macrdp-mm-live-layout`):**
    `Acceptor::set_monitor_layout(Option<Vec<Monitor>>)` gives a deactivation-reactivation
    acceptor the layout the session now serves (same checks, `reactivation_monitor_layout`;
    test `a_reactivation_keeps_a_new_layout_only_when_it_can_be_served`). Server (36) calls it
    before each reactivation with `RdpServerDisplay::monitor_layout()` (a new trait method,
    default `None`). Without it, a reactivation after the client changed its layout sent the
    connect-time layout, and mstsc drew both monitors on one screen.

### Testing

(7) has three tests in `ironrdp-testsuite-core` `tests/server/acceptor.rs`
(`connect_time_autodetect_window_*`). (4) and (5) have no test in the fork: the `macrdp_*` acceptor
tests of the 2026-10-01 rebase lived on a local scratch branch that was never pushed. (4) is used by
the server's fingerprint test, and (5) by `mt_session::tests` and macrdp's `conn_test` lossy offer.

### Retired at 79e763a0

(6) Client desktop and device scale factors → upstream #2107 (`335d6712`):
    `AcceptorResult::{desktop,device}_scale_factor`, reported only as a valid pair.

### Retired earlier

(1) Honor the client's requested desktop size — upstream #1373 + #1404
    (`set_honor_client_desktop_size(Option<DesktopSize>)`), retired at a5d1c682.
    *Why it has to live in the acceptor at all* (the client's size is only in the GCC
    Client Core Data, before Demand Active) is recorded in `docs/known-quirks.md`.
(2) Expose the client's keyboard-layout id — upstream #1397, retired at a5d1c682.
(3) UDP multitransport — read side (client flags) upstream #1453, retired at a5d1c682;
    **offer side retired 2026-10-01** (e258f6a0) in favour of upstream's own offer (#1951):
    `set_multitransport_offer(Option<MultiTransportFlags>)` advertises;
    `MultitransportBootstrapping` (right after licensing) sends the request;
    `multitransport_request()` exposes what was sent; `set_multitransport_security_rng()`
    supplies the cookie and request ID; and `is_late_multitransport_response` tolerates the
    client's reply during capabilities confirmation and finalization. With no MCS message
    channel, upstream doesn't offer; that is the spec-correct behaviour (2.2.15.1: the
    request travels on the message channel).

## ironrdp-rdpdr

### Shape: upstream verbatim + two new files

`src/` is upstream 79e763a0 **verbatim** except three lines in two files, marked
`(macrdp divergence N)`:

- `pdu/mod.rs`: `mod server_direction;` + `pub use self::server_direction::ScardControlRequest;`
- `pdu/esc/mod.rs`: `mod server_direction;`

Everything else lives in two new files: `pdu/server_direction.rs` and
`pdu/esc/server_direction.rs`. As child modules they can implement traits on, and add
methods to, their parent's types and read private fields, so no upstream line needs to
change. **Keep it that way:** a rebase is "re-apply three lines, fix what no longer
compiles in our two files".

Cost of this shape: under IronRDP's workspace lints, `clippy::multiple_inherent_impl`
fires for every type that gains a second `impl` block here, so the fork drops the lints
from this crate (see "Lints"). If any of this is upstreamed, it folds into upstream's
existing `impl` blocks.

### Why a fork at all

macrdp's RDPDR server processor is its own (`ironrdp-server`'s `rdpdr_drive.rs`, server
divergence (11)), and it needs to build requests and parse replies in the server's
direction. Upstream's EFS (drive) PDU layer is two-way (#1779) and it has its own
`RdpdrServer` (#1783/#1784), but that server frames requests with a private type and has no
smart card path; its ESC (smart card) layer is still client-only. Moving macrdp onto
upstream's `RdpdrServer` would be a separate change.

### Divergences

(1) **Drive (EFS) server direction** — nearly all retired at e258f6a0 (see below). What's left:
    `pdu/server_direction.rs`: `impl Encode + SvcEncode for ServerDriveIoRequest` —
    `PAKID_CORE_DEVICE_IOREQUEST` header + the request's own (upstream) `encode`. Upstream's
    equivalent (`DriveRequestBody` in `server.rs`) is private. Covers every variant except
    `DeviceControlRequest`, which carries no input buffer (upstream's server excludes it
    too). The `match` is exhaustive on purpose: a new upstream variant is a compile error,
    not a silent gap.

(2) **Smart card (ESC) server direction** — `pdu/esc/server_direction.rs`, for
    `--enable-smartcard-redirection`. Upstream decodes each `*Call` and encodes each
    `*Return`; macrdp needs the mirror halves:
    - `rpce::HeaderlessEncode` for the calls the server sends: `EstablishContextCall`,
      `ContextCall`, `ListReadersCall`, `GetStatusChangeCall`, `ConnectCall`,
      `HCardAndDispositionCall`, `StatusCall`, `TransmitCall`; plus `ScardCall::encode`/`size`.
    - `decode` + `rpce::HeaderlessDecode` for the replies: `LongReturn`,
      `EstablishContextReturn`, `ListReadersReturn`, `GetStatusChangeReturn`,
      `ConnectReturn`, `StatusReturn`, `TransmitReturn`. Accessors `return_code()` on
      `LongReturn` and `return_code()`/`context()` on `EstablishContextReturn` (upstream's
      fields are private).
    - `ndr::Encode` for `ConnectCommon` and `ReaderState`; a private NDR string writer
      (`write_ndr_string`/`ndr_string_size`, the mirror of `ndr::read_string_from_cursor`).
    - `TryFrom<u32>` for `ReturnCode` and `CardState`; `From<Scope> for u32`.
    - `ScardControlRequest` (in `pdu/server_direction.rs`): a `DR_CONTROL_REQ` carrying a
      `ScardCall`, built from upstream's `DeviceControlRequest::encode` plus the call.

    **Live-Windows rules these halves must keep** (found against mstsc + a TPM virtual smart
    card, 2026-06-18; full APDU transceive verified end to end):
    - A NULL `[unique]` referent means no deferred conformant array. The decoders check the
      referent before reading `MaximumCount` for `mszReaderNames` (`StatusReturn`, Windows
      often sends an ATR-only status), `pbRecvBuffer` (`TransmitReturn`, no card data), and
      `msz` in `ListReadersReturn` (a length-only probe). Upstream's `GetStatusChangeReturn`
      encode now writes `cReaders=0` and a NULL pointer for a non-success return (#2175); our
      decoder reads that form.
    - **`TransmitCall` writes a NULL `pbExtraBytes` referent when there are no extra PCI
      bytes** (`encode_io_request_ptr`). This deliberately does NOT use upstream's
      `ndr::Encode for SCardIORequest`, which writes a non-NULL referent for the same
      input; real Windows rejects that `Transmit_Call` with `STATUS_UNSUCCESSFUL`. Don't
      "simplify" it back to `send_pci.encode_ptr`.
    - Pointer-sized (8-byte) contexts/handles and the connect handle's empty embedded
      context are upstream's own behaviour (`ScardContext`/`ScardHandle`, #1654).

### Retired at 79e763a0

- `DeviceAnnounceHeader::device_type()` widened to `pub` → upstream made it public in #2440
  (`2e65d081`).

### Retired at e258f6a0 (upstream now has it)

- All drive decode halves: `ClientNameRequest::decode`, `ClientDeviceListAnnounce::decode`,
  `DeviceAnnounceHeader`/`PreferredDosName` decode + `device_id()`/`preferred_dos_name()`,
  and the `Device{Create,Read,Close,Write}Response` decoders (upstream #1779; they now take
  the `DeviceIoResponse` first).
- All drive encode halves: `Device{Create,Read,Close,Write}Request`,
  `ServerDriveQueryDirectoryRequest`, `ServerDriveSetInformationRequest`, the
  set-information buffers, and `FileDirectoryInformation` decode. `FileInformationClass::level()`
  went too: upstream's `ServerDriveSetInformationRequest::encode` maps the class itself.
- Smart card: variable-length `ScardContext`/`ScardHandle` and their NULL handling
  (upstream #1654), and `From<ScardIoCtlCode> for u32`.

### Testing

The crate's lib is testable as upstream ships it: `cargo test -p ironrdp-rdpdr --lib` runs upstream's
tests plus our 14 (`pdu::server_direction::tests`, `pdu::esc::server_direction::tests`; the round
trips are table-driven, so one test covers many PDUs). Ours round-trip every request through
upstream's own decode chain and every reply from upstream's own encoder, and add hand-built byte
layouts real Windows sends (empty handle context, NULL reader names, NULL receive buffer).
**Mutation-checked 2026-10-01:** each of these, introduced alone, fails a test — non-NULL
`pbExtraBytes` referent, unconditional `msz`/reader-names/receive-buffer read, a typo in
the `ReturnCode` table, dropping the NDR string padding, skipping the connect handle's
value.

## ironrdp-dvc

`src/` is upstream 79e763a0 plus **one** pure addition, divergence (3), in `server.rs`: no upstream
line is deleted or changed. The crate keeps IronRDP's workspace lints and is lint-clean under them.
Every `ironrdp-*` crate comes from the same fork rev, so the git crates that depend on
`ironrdp-dvc` (egfx, displaycontrol, echo, rdpeusb) use this copy too; no second patch is needed.

### Divergence

(3) **`DrdynvcServer::request_soft_sync(tunnel_type, channel_ids)`** (NOT upstreamed;
    added 2026-10-01 at the e258f6a0 bump). Upstream's `DrdynvcServer` owns a
    Soft-Sync state machine (#1584 + #1954): one request per connection, and the
    client's response is validated against the request — an unsolicited, duplicate,
    or unrequested-tunnel response is an **error** out of `process()`, i.e. it ends
    the session. Its only public request API is `request_reliable_udp(non-empty
    ids)`, which also **registers** the channels for tunnelling — after which
    `process()` rejects their data over TCP ("received TCP data for a channel
    selected by Soft-Sync"). Unchanged at 79e763a0.

    macrdp needs three things that API can't do: Soft-Sync onto the **lossy** tunnel
    (`--enable-lossy-audio`, `AUDIO_PLAYBACK_LOSSY_DVC`); an **empty** request (the
    default when neither lossy audio nor `--udp-migrate-egfx` is on); and moving EGFX
    **back to TCP** when the tunnel wedges (the watchdog — the protocol has no reverse
    Soft-Sync, so the client is never told, and its data may arrive over TCP). So the
    added method: validates the channels (open, no duplicates), records the request in
    the state machine so the response validates normally, builds one channel list for
    `tunnel_type` (none if `channel_ids` is empty — only SOFT_SYNC_TCP_FLUSHED set),
    and **does not register per-channel routing** — `tunnel_for_outgoing_channel`
    stays `None` and `process()` keeps accepting the channels' data over TCP. macrdp
    routes the data itself.

    **`request_soft_sync_lists(lists)`** (2026-11-24, clintcan/IronRDP#4): the same with one
    channel list per tunnel, for server (12)'s two tunnels (lossy audio on `LOSSY_UDP`, EGFX on
    `RELIABLE_UDP`); `request_soft_sync` now calls it with one list. It also rejects a tunnel or a
    channel named twice. Tests: `a_soft_sync_can_name_two_tunnels`,
    `a_two_tunnel_soft_sync_rejects_a_repeated_tunnel_or_channel`.

    Tests: five `request_soft_sync_*` tests in `src/server.rs`'s test module (lossy request
    + response + TCP data still accepted; empty request carries no list; unrequested-tunnel
    response rejected; one request per connection; unopened/duplicate channels rejected).
    **Mutation-checked 2026-10-01:** making the method register routing like
    `request_reliable_udp` fails the first test. Wire bytes are additionally pinned by
    macrdp's `src/multitransport.rs` tests.

(1) **RETIRED 2026-10-01 at the e258f6a0 bump.** Server-direction Soft-Sync PDU codec →
    upstream #1584 (`SoftSyncTunnelType`, `SoftSyncRequestPdu::new`,
    `SoftSyncResponsePdu::tunnels_to_switch()`). `get_channel_id_by_name` has no upstream
    equivalent; callers use `get_channel_id_by_type::<T>()` (macrdp gives its two audio DVCs
    distinct types for that).

(2) **RETIRED 2026-08-05 at the a5d1c682 bump.** Invoking `DvcProcessor::close` on a
    client Close — upstream #1302 does it via `impl Drop for DynamicChannel`.

### Retiring this divergence

Two routes. (a) Upstream (3): the lossy + empty generalisation is a natural
extension of `request_reliable_udp`; the no-routing part is macrdp-specific (it
exists for the TCP fallback), so an upstream version would more likely register
routing and add an explicit "release this channel back to TCP" call. (b) Adopt
upstream's own UDP routing (#1954); upstream still has no way back to TCP (a lost
tunnel with EGFX on it ends the session), so the TCP fallback must be re-examined
first. Either way, verify the fallback live on mstsc.
