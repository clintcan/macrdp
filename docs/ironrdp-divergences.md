# IronRDP divergences

macrdp takes every `ironrdp-*` crate from [clintcan/IronRDP](https://github.com/clintcan/IronRDP),
branch `macrdp`, at the rev in the root `Cargo.toml` `[patch.crates-io]`. That branch is upstream
Devolutions/IronRDP@e258f6a0 plus macrdp's divergences in `ironrdp-server`, `ironrdp-acceptor`,
`ironrdp-rdpdr` and `ironrdp-dvc`, one commit per divergence. The subject has the crate and the
number, for example `server: (34) send RemoteFX in a Stream Surface Bits command`.

To change a divergence: make the commit on that branch, then bump the rev in `Cargo.toml` (every
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

The logs below were written when the four crates were vendored in this repo, at `vendor/ironrdp-*`.
Read `vendor/ironrdp-<crate>/` as `crates/ironrdp-<crate>/` in clintcan/IronRDP. Notes about the
vendored copy's own `[patch.crates-io]`, `rustfmt.toml` and `[lib] test = false` describe the
vendored copy, not the branch. The reasoning behind the server divergences is in
[`ironrdp-divergence-history.md`](ironrdp-divergence-history.md).

## ironrdp-server

Local fork of ironrdp-server 0.13.0, **re-vendored 2026-10-01 from upstream
Devolutions/IronRDP@e258f6a0** (the pin-bump rev), replacing the a5d1c682 fork. The
reasoning behind each divergence (live-debugging notes, dead ends, what NOT to do)
is in **[`ironrdp-divergence-history.md`](ironrdp-divergence-history.md)**, frozen at the old fork; numbers are shared, so (N)
here is (N) there. Read the history entry before changing a divergence.

Pulled in via `[patch.crates-io]` in the root `Cargo.toml`; has a standalone
`[patch.crates-io]` (vendored siblings by path, the rest git-pinned to e258f6a0) for
isolated builds, ignored in the macrdp workspace. Keep that rev in sync. The lib is
`test = false`, and macrdp is a single package, so the root `cargo fmt`/`clippy`/`test`
never reach this crate: check it with `--manifest-path vendor/ironrdp-server/Cargo.toml`
(`rustfmt.toml` matches upstream's 120 columns, and upstream's files are clean under it).

### Shape

Upstream's files verbatim, plus macrdp's own files, wired in by hook edits marked
`(macrdp ...)`:

- **Our files:** `audin.rs` (25), `rdcamera.rs` (19), `rdpdr_drive.rs` (11),
  `rdpeusb.rs` (16), and `mt/` (12): `mod.rs`, `listener.rs`, `dtls.rs`,
  `audio_dvc.rs`, and `session.rs`, which `server.rs` declares as its child module
  (`#[path]`) so all of the multitransport logic lives outside upstream's file.
- **Hook edits in upstream files:** `server.rs` (fields grouped in one
  `macrdp: MacrdpServerState`; one-line calls at each hook point), `builder.rs`
  (one `macrdp_channels` field, four `with_*` methods, one post-build
  `install_macrdp_channels` call), `lib.rs` (module declarations and exports),
  `gfx.rs` (14), `sound.rs` (2/8).
- **Cargo deviations:** the `multitransport` feature + `macrdp-rdpeudp`/`getrandom`/
  `boring`; `anyhow` (our modules' error type); macOS `libc` (15); and `usb` drops
  `dep:ironrdp-rdpeusb`, which is a regular dependency here because `rdpeusb.rs` uses it.

**Coexistence rule.** Upstream now has its own RDPDR server (`with_rdpdr_factory`),
microphone (`with_rdpeai_factory`), USB (`usb` feature, `with_usb_factory`) and UDP
multitransport (`with_udp_transport`). They stay inert because macrdp never sets them.
Never set one together with macrdp's counterpart: both register the same channel
(`RDPDR`, `AUDIO_INPUT`, `URBDRC`) or offer, and the client would see it twice. Our
clashing names were renamed for this (11): `RdpdrDriveServerFactory`,
`RdpdrDriveMessage`, `RdpdrDriveServer`, `ServerEvent::RdpdrDrive`,
`with_rdpdr_drive_factory`; USB's builder method is `with_urbdrc_factory`.

### Kept divergences

| # | What | Where | macrdp's API |
|---|---|---|---|
| (2)(3)(8) | Audio on its own bounded channel + task; drop-stale lag model with resync after a writer stall; per-wave duration for AAC | `client_loop`'s `dispatch_audio`; `AudioLagModel`; `sound.rs` `AudioWave`, `set_audio_sender` | `SoundServerFactory::set_audio_sender` |
| (4) | Batch priority: CLIPRDR, then EGFX + rest, then RDPDR | `prioritize_batch` | — |
| (11) | Our RDPDR drive + smart card processor | `rdpdr_drive.rs` | `with_rdpdr_drive_factory`, `RdpdrHandle` |
| (12) | Our UDP multitransport (RDPEUDP + DTLS lossy audio + Soft-Sync + watchdog de-migration) | `mt/` (`session.rs` holds the server side) | `set_multitransport_*`, `set_egfx_on_*_handle`, `set_demigrate_request_handle`, `set_migrate_egfx` |
| (14) | EGFX decline flag (no CapabilitiesConfirm for a no-AVC client) | `gfx.rs` | `GfxDvcBridge::with_decline_flag` |
| (15) | Kernel TCP RTT at accept | `tcp_srtt_ms`, `store_link_rtt` | `set_link_rtt_handle` |
| (16) | Our URBDRC USB processor | `rdpeusb.rs` | `with_urbdrc_factory`, `UsbHandle` |
| (18) | `ConnectionHandler::on_authenticated` | `complete_security_upgrade`, via `PendingConnection::observe_authentication` | trait method |
| (19) | MS-RDPECAM camera | `rdcamera.rs` | `with_camera_factory` |
| (20) | Client-fingerprint log + `on_client_fingerprint` | top of `client_accepted` | trait method |
| (24) | Per-served-connection input-reset flag | `raise_input_reset` in `run_connection_inner` and `serve_negotiated` | `set_input_reset_handle` |
| (25) | Our MS-RDPEAI microphone | `audin.rs` | `with_audin_factory` |
| (26) | Client input read off the `Self` lock: fast-path input straight to the handler, X224 through a bounded queue; queued X224 replayed across a reactivation | `client_loop`'s `read_pdus` arm; `MacrdpServerState::read_ahead`, drained into the backlog in `client_accepted` | — |
| (28) | `zstd-safe` 8.2 instead of upstream's 7.2 (RustSec advisories in 7.x; QOIZ only compresses, so unreachable) | `Cargo.toml` only | — |
| (29) | AND mask for 32 bpp pointers, **harvested early** from upstream #2108 (merged 2026-10-19, after this pin) | `encoder/pointer_mask.rs` (upstream's file, verbatim) + the two `and_mask_from_alpha` call sites in `encoder/mod.rs` and a doc line in `display.rs` | — |
| (30) | An auto-reconnect cookie that fails verification falls back to a normal logon instead of denying the connection | `client_accepted`, the `is_auto_reconnect` check | — |
| (31) | A zero-payload fast-path update (`HiddenPointer`, `DefaultPointer`) goes out as one empty frame instead of nothing, **harvested early** from upstream #2274 (merged 2026-10-29, after this pin) | `encoder/fast_path.rs` `UpdateFragmenter::encode_next` | — |
| (32) | The serving path's pre-finalize phase (X.224, TLS, CredSSP) is bounded by `SERVE_NEGOTIATION_TIMEOUT` (60 s), so an unauthenticated peer that goes silent can't hold the serving connection. **Upstream as #2432 (`e482e340`); drops at the next pin bump** | `run_connection_inner`, around `negotiate_and_authenticate` | — |
| (33) | The client's desktop and device scale factors on `ConnectionInfo`, **harvested early** from upstream #2107 (merged 2026-10-30, after this pin) | `ConnectionInfo` fields + `new()`, the `on_connection_info` call in `client_accepted`; pairs with acceptor divergence (6) | `ConnectionInfo::{desktop,device}_scale_factor` |
| (34) | RemoteFX (codec GUID REMOTEFX) goes out in a Stream Surface Bits command when the client sets `STREAM_SURFACE_BITS`; ImageRemoteFX (GUID IMAGE_REMOTEFX) and every other codec keep Set Surface Bits. RemoteFX wins over ImageRemoteFX whatever the client's list order. Windows 10 mstsc shows a black session for RemoteFX in Set. **Port of upstream #2229 (`e20a2147`); drops at the next pin bump** | `encoder/mod.rs` `RemoteFxCodec`, `SurfaceBitsCommand`, `surface_bits`, `UpdateEncoderCodecs::set_remotefx_codec`, `RemoteFxHandler`; `server.rs` `pick_remotefx_codec` and the `BitmapCodecs` arm | — |

Notes on the port:
- **(26)** Ported from main (#222) on 2026-10-27. Upstream's per-PDU lock-wait
  logging stays in `dispatch_pdu`, which still takes the lock. The replay path has no
  test: removing the `read_ahead.push` leaves every test passing.
- **(28)** Drop it once upstream requires zstd-safe >= 8.2.
- **(29)** Numbered (29) because main used (27) and (28) while this branch was open.
  A byte-for-byte backport of upstream #2108 (by @antonmos): `RGBAPointer` and
  `LargePointer` used to go out with an empty AND mask, which MS-RDPBCGR 2.2.9.1.1.4.4
  requires and Windows App drops (macOS: stuck on the default arrow; suspected for the
  iOS no-cursor report, clintcan/macrdp#211). Upstream's unit tests live in
  `pointer_mask.rs` but this lib is `test = false`, so they were run once in a scratch
  crate (2026-10-26). **Drop it at the next pin bump** (any rev at or after `fb25b47c`).
- **(30)** Added 2026-10-28 after a live FreeRDP test: restarting macrdp with a client
  connected made it auto-reconnect with the old process's cookie, which never verifies.
  Upstream denied it, FreeRDP retried with the same cookie about three times a second,
  and every try ended in under a second, so `auth_guard` counted each as a failed login
  and locked out the client's address (30 s, then 59 s). Each try also evicted the live
  session first, because preemption runs before this check. MS-RDPBCGR 3.3.5.3.11: "If
  logon with the cookie fails, the credentials supplied in the Client Info PDU SHOULD
  be used". The fallback grants nothing: CredSSP or the acceptor's credential check has
  already authenticated the client, and the credential validator still runs. Test:
  `conn_test::a_stale_auto_reconnect_cookie_falls_back_to_a_normal_logon`. Upstream
  candidate.

- **(31)** Added 2026-11-13 while live-testing clintcan/macrdp#219 (hide the pointer
  while an app hides it). The fragmenter's `encode_next` returned `None` for an empty
  payload, so `DisplayUpdate::HidePointer` and `DefaultPointer` produced no bytes at
  all: macrdp logged `cursor hidden`, yet FreeRDP's `restoreCursor` never saw a null
  pointer and Windows App kept the arrow. Only the 3-line `fast_path.rs` part of #2274
  is taken; its pointer-promotion and RemoteFX-buffer changes are not. Test:
  `conn_test::hidden_and_default_pointer_updates_reach_the_client` (fails without the
  fix; catches a dropped `index == 0` guard and a `First` fragment). **Drop it at the
  next pin bump** (any rev at or after `904a7da6`).

- **(32)** Added 2026-11-13 for clintcan/macrdp#186. The candidate path was already
  bounded (`CANDIDATE_NEGOTIATION_TIMEOUT`, 10 s) and finalize by upstream's
  `FINALIZE_TIMEOUT`, but `run_connection_inner` awaited `negotiate_and_authenticate`
  with no bound, so a peer that sent a Connection Request and went silent held the
  serving connection forever, pre-authentication. 60 s rather than 10 s because Windows
  App for macOS prompts for the password inside this phase (7-11 s measured
  accept-to-auth) and a cellular CredSSP took 12.5 s. Test:
  `conn_test::a_silent_peer_cannot_hold_the_serving_connection` (paused clock; fails
  without the bound, and with a 30 s one). **Upstreamed** as Devolutions/IronRDP#2432,
  merged 2026-11-14 (`e482e340`, 60 s `NEGOTIATION_TIMEOUT` around
  `negotiate_and_authenticate`, covering every entry point). Drops at the next pin bump
  to any rev at or after `e482e340`; keep the conn_test as the regression check.

- **(33)** Added 2026-11-13 by clintcan/macrdp#201 (`--virtual-display-hidpi=auto`),
  replacing that PR's own shared-cell divergence: upstream #2107 (by @antonmos) carries
  the same values through `ConnectionInfo`, and macrdp's `conn_hooks::DesktopScaleHook`
  stores them in the scale cell, as `KeyboardLayoutHook` does for the layout. Upstream
  reports the pair only when both are valid (desktop 100..=500, device 100/140/180) and
  only on the initial accept, so a reactivation can't overwrite a scale the client
  announced later in a monitor layout. Copied from upstream's diff, not its file. Test:
  `conn_hooks::tests::the_desktop_scale_factor_reaches_the_shared_cell`. **Drop it at
  the next pin bump** (any rev at or after `335d6712`).

- **(34)** Added 2026-10-21 (clintcan/macrdp#226). Numbered (34) because main used
  (29) to (33) while this branch was open. MS-RDPRFX 3.1.8.3.1: "The sequence of
  encode messages is encapsulated inside an Extended Bitmap Data structure ..., which
  is encapsulated in a Stream Surface Bits Surface Command". Windows 10 mstsc
  (10.0.19041.4717), A/B on the wire 2026-10-21:
  - RemoteFX, id 3 (what mstsc gets when both are offered): Set Surface Bits =
    **black** session (no error, input still flows); Stream Surface Bits = renders
    and moves.
  - ImageRemoteFX, id 5 (server offering only ImageRemoteFX): Set Surface Bits =
    renders and moves (385 commands in 12 s); Stream Surface Bits = mstsc "An
    internal error has occurred" after 4 commands (3 of 3 runs).

  FreeRDP 3.30 `/rfx` offers only RemoteFX (id 3), so it cannot exercise id 5.
  Upstream merged the same fix after our pin as Devolutions/IronRDP#2229
  (`e20a2147`); this port uses its names and shape: `RemoteFxCodec::surface_bits_command`
  sends Stream Surface Bits only for RemoteFX and only when the client sets
  `STREAM_SURFACE_BITS` in its Surface Commands capability (MS-RDPBCGR 2.2.7.2.9), and
  `pick_remotefx_codec` picks RemoteFX over ImageRemoteFX whatever the order of the
  client's codec list (before, the last one in the list won). Tests:
  `encoder::surface_command_tests` and `server::remotefx_codec_choice_tests` (upstream's
  proptest replaced by fixed codec lists), run by CI (`cargo test --manifest-path
  vendor/ironrdp-server/Cargo.toml --lib --features egfx`). **Drop it at the next pin
  bump** (any rev at or after `e20a2147`).

- **(2)** `discard_stale_session_events` (upstream, before serving a preemption winner)
  now also drains the audio channel, so the evicted session's queued waves don't play
  into the winner's. `dispatch_audio` adopts upstream's `is_ready()` check (drop
  quietly until RDPSND negotiates) instead of warning per wave.
- **(4)** Now a stable `sort_by_key` by tier, equivalent to the old three-bucket split.
  Upstream's own `Rdpdr` events sort with ours, last.
- **(11)** Upstream's drive PDUs are now two-way, so the vendored ironrdp-rdpdr fork
  shrank to the smart card side (see its CLAUDE.md). `decode_write_response` keeps
  `DR_WRITE_RSP`'s padding byte optional, as MS-RDPEFS 2.2.1.5.4 says; upstream's
  decoder requires it. Upstream-issue candidate.
- **(12)** The offer now uses the rebased acceptor's API: `OfferRng`
  (`MultitransportSecurityRng`) registers each cookie with the listener as it is
  generated; the offer is `flag | SOFT_SYNC_TCP_TO_UDP` plus
  `set_multitransport_requested_protocol` for lossy; `MigrationState` comes from
  `acceptor.multitransport_request()` after each finalize. Soft-Sync goes through the
  dvc fork's `request_soft_sync` (same wire bytes, `SHOW_PROTOCOL` included). Upstream's
  message-channel handler consumes the Initiate Multitransport Response first, so ours
  is `mt_on_response` inside its arm, with the owed empty Soft-Sync sent by the caller.
  The two audio DVCs now have distinct types (`AudioReliableDvc`, `AudioLossyDvc`),
  because the DRDYNVC server's by-type lookup keys on the Rust type. Our field
  `egfx_on_udp` lives in `mt` state; upstream has its own field of that name.
  **Behaviour change:** the rebased acceptor only offers multitransport when the client
  negotiated the MCS message channel (the old fork fell back to the I/O channel). mstsc
  sends it; a client that doesn't now silently gets no UDP offer.
  **Regression found 2026-11-13 (silent lossy audio since v0.10.0):** on the message
  channel, mstsc DOES answer the offer with `S_OK` (the June notes saw only tunnel
  creation), and `mt_on_response` spent the connection's one Soft-Sync on an empty
  request for the reliable tunnel. `mt_soft_sync_on_egfx` then found it claimed and
  never migrated `AUDIO_PLAYBACK_LOSSY_DVC`, so mstsc dropped every wave streamed into
  the tunnel; `--udp-migrate-egfx` went through the same one-shot. Fix: when a channel
  is configured to migrate (`mt_has_channel_to_migrate`: lossy audio or EGFX), the
  response leaves the Soft-Sync to the migration gate; otherwise it still gets the
  empty one. Live-verified on mstsc (LAN): the Soft-Sync names the lossy audio DVC,
  mstsc answers `tunnels_to_switch: [3]`, and audio plays. No automated test (the path
  needs a real multitransport client). Follow-up the same day: waves stay on TCP
  (`mt_route_lossy_audio` returns them to the static rdpsnd path) until the Soft-Sync
  naming the lossy channel has been sent AND acknowledged
  (`lossy_audio_soft_synced` + `DrdynvcServer::soft_sync_response_received`). Before,
  they started ~0.5 s ahead of the Soft-Sync and that first half-second was dropped.
  Live: audio from the first second, lossy streaming 4 ms after the client's response.
- **(15)** A preemption winner's RTT is now sampled as a candidate (carried on
  `NegotiatedConnection.link_rtt_ms`); before, it inherited the previous connection's.
- **(16)** Upstream made a completion's raw `TS_URB_RESULT` bytes private, so
  `urb_completion_no_data_result` re-reads SelectConfiguration's result from the PDU,
  only after upstream's decode has validated the layout. Byte-tested against
  upstream's encoder, with two mutations caught, 2026-10-01 (scratch crate). Drop it
  once upstream exposes the bytes or a typed result.
  **Stricter decode:** upstream now decodes every `URB_COMPLETION` result as isoch
  (`into_isoch()`), so a *non-isoch* completion carrying a non-empty result payload fails
  the whole PDU, where the old fork accepted it. Normal for bulk/control; watch the USB
  live test for decode errors.
- **(18)** Fires for preemption candidates too (`NegotiationContext` carries the
  handler); #2065's shared handler makes that borrow-safe.

### Retired at e258f6a0 (upstream has it; what macrdp calls instead)

- **(5) SuppressOutput** → builder `with_display_suppressed_handle`. **Behaviour
  change:** upstream's capabilities ADVERTISE refresh-rect/suppress-output support,
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

### Known leftovers

- `cargo clippy` flags two `collapsible_if`s in upstream's own code (`autodetect.rs`,
  `run()`'s `on_disconnected` call) under our newer toolchain. Left as upstream wrote them.
  With `--all-features` it also flags `urbdrc::PendingIo` as more private than
  `UrbdrcDeviceServerMessage::IoReq::tx`, also upstream's.
- Upstream's in-file `#[cfg(test)]` module never compiles here (`test = false`); its
  one `NegotiationContext` literal was kept in step with our added field anyway.

## ironrdp-acceptor

Local fork of ironrdp-acceptor 0.10.0, **re-vendored 2026-10-01 from upstream
Devolutions/IronRDP@e258f6a0** (the pin-bump rev). `src/` is upstream verbatim except
`connection.rs`, which carries the three divergences below (every insertion of (4) and
(5) is marked `(macrdp divergence N)`; (6) is upstream code, unmarked). Earlier vendorings: @a5d1c682 (2026-08-05), @879ffed
(2026-06-12). Has a standalone `[patch.crates-io]` (core/pdu/svc/connector/async →
e258f6a0) for isolated builds, ignored in the macrdp workspace (the root `[patch]`
wins). Keep that rev in sync. `rustfmt.toml` mirrors the IronRDP workspace config
(added 2026-10-01 — without it rustfmt reformats upstream's 120-column code).

**Known, pre-existing, not ours:** `cargo clippy -- -D warnings` on this crate alone
flags a `collapsible_if` in upstream's credentials check (`connection.rs`, the
`if let Some(expected) = &self.creds` block). It's a newer-clippy lint (our 1.95 vs
upstream's 1.94); verified identical on pure upstream source. macrdp's own clippy run
doesn't lint path dependencies. Don't "fix" upstream code here.

### Testing

The lib is `test = false`, and the useful harness lives upstream in
`ironrdp-testsuite-core/tests/server/acceptor.rs`. To test a change, copy
`connection.rs` into the IronRDP clone (`~/Documents/Projects/Misc/IronRDP`) at the
pinned rev and run `cargo test -p ironrdp-testsuite-core server::acceptor`. The
2026-10-01 rebase did exactly that on the local branch
`scratch/macrdp-acceptor-divergences` (not pushed): four `macrdp_*` tests below, plus
upstream's 13 acceptor tests and the whole suite (1731) passing against this fork.
Both divergences were mutation-checked.

### Divergences

(4) **Client-fingerprint identity fields** (NOT upstreamed; added 2026-07-18).
    `Acceptor` captures `client_name: String`, `client_version: u32` (raw
    `RdpVersion.0`) and `client_build: u32` from the GCC Client Core Data in
    `BasicSettingsWaitInitial`, next to upstream's own `keyboard_layout` /
    `keyboard_type` / `ime_file_name` captures, carries them across
    deactivation–reactivation, and surfaces them as `pub` fields on `AcceptorResult`.
    Consumed by the vendored server's divergence (20) client-fingerprint log. Same
    additive shape as #1397 (keyboard layout) — cleanly upstreamable. Heuristics: mstsc
    sends the real Windows build + its hostname; FreeRDP hardcodes build 2600.
    Informational only — a client can claim anything.
    Test: `macrdp_result_carries_the_client_fingerprint` (drives to `Connected`, reads
    `get_result()`). Mutation: dropping the build capture fails it.

(5) **`Acceptor::set_multitransport_requested_protocol(RequestedProtocol)`** (NOT
    upstreamed; added 2026-10-01). Upstream's offer (#1951) only ever requests reliable
    UDP: `MultitransportBootstrapping` gates on `TRANSPORT_TYPE_UDP_FECR` and hard-codes
    `RequestedProtocol::UdpFecR` ("this acceptor never requests [lossy]"). macrdp's
    `--enable-lossy-audio` needs a lossy (`UdpFecL`) request. The setter (default
    `UdpFecR`, so upstream behaviour is unchanged when it isn't called) makes both gates
    — our advertised offer and the client's flags — and the request use the chosen
    protocol's transport flag. The gate's locals were renamed `offer_transport` /
    `client_supports_transport` (upstream's `offer_udp_fecr` / `client_supports_udp_fecr`
    would be wrong for lossy) and the "Not offering UDP multitransport" debug log gained
    `requested_protocol`. Tests: `macrdp_lossy_multitransport_request_when_requested_and_reciprocated`,
    `macrdp_lossy_request_needs_the_client_to_support_lossy`,
    `macrdp_lossy_request_needs_the_offer_to_include_lossy`. Mutation: hard-coding the
    request back to `UdpFecR` fails the first. Upstream candidate: a natural extension
    of #1951.

(6) **Client desktop and device scale factors** — **harvested early** from upstream
    #2107 (by @antonmos, merged 2026-10-30 as `335d6712`, after this pin). `Acceptor`
    captures `desktopScaleFactor` / `deviceScaleFactor` from the GCC Client Core Data in
    `BasicSettingsWaitInitial` through `validate_scale_factors` (both `Some` only as a
    valid pair, per MS-RDPBCGR 2.2.1.3.2), carries them across deactivation–reactivation,
    and surfaces them as `Option<u32>` fields on `AcceptorResult`. Consumed by server
    divergence (33). Copied from upstream's diff, with no `(macrdp divergence N)` markers
    so it matches upstream exactly. Upstream's acceptor tests cover it
    (`scale_factors_are_reported_only_as_a_valid_pair`, `scale_factors_survive_reactivation`).
    **Drop it at the next pin bump** (any rev at or after `335d6712`).

### Retired

(1) Honor the client's requested desktop size — upstream #1373 + #1404
    (`set_honor_client_desktop_size(Option<DesktopSize>)`), retired at a5d1c682.
    *Why it has to live in the acceptor at all* (the client's size is only in the GCC
    Client Core Data, before Demand Active) is recorded in `docs/known-quirks.md`.
(2) Expose the client's keyboard-layout id — upstream #1397, retired at a5d1c682.
(3) UDP multitransport — read side (client flags) upstream #1453, retired at a5d1c682;
    **offer side retired 2026-10-01** in favour of upstream's own offer (#1951). What
    went: our `MultitransportOffer` type and `set_multitransport_offer(Option<MultitransportOffer>)`,
    `AcceptorResult::multitransport_offered`, our `SC_MULTITRANSPORT` advertise, the
    request emitted in `LicensingExchange`, and the `finalization.rs` skip of
    non-io-channel PDUs. Upstream equivalents: `set_multitransport_offer(Option<MultiTransportFlags>)`
    advertises; `MultitransportBootstrapping` (right after licensing, the same window)
    sends the request; `multitransport_request()` exposes what was sent;
    `set_multitransport_security_rng()` supplies the cookie and request ID; and
    `is_late_multitransport_response` tolerates the client's reply during
    capabilities confirmation and finalization (stricter than our skip: only a payload
    that decodes as the reply to the sent request is consumed — which covers the
    E_ABORT case our skip existed for). One behaviour difference: with no MCS message
    channel, upstream doesn't offer; ours fell back to the I/O channel. Upstream's is
    the spec-correct one (2.2.15.1: the request travels on the message channel).

### Porting notes for the server rebase (vendored ironrdp-server)

- Offer: `acceptor.set_multitransport_offer(Some(flag | MultiTransportFlags::SOFT_SYNC_TCP_TO_UDP))`
  where `flag` is `TRANSPORT_TYPE_UDP_FECR` or `_FECL`, plus
  `set_multitransport_requested_protocol(..)` for the lossy case.
- Cookie: implement `MultitransportSecurityRng` so `fill_security_cookie` registers the
  cookie in the multitransport `CookieRegistry` as it is generated — the UDP listener
  then knows it before the client's first datagram (no race), replacing `new_offer()`.
- Replace `result.multitransport_offered` with `acceptor.multitransport_request()` (or
  the `accept_finalize_with_multitransport` handler).

## ironrdp-rdpdr

Local fork of ironrdp-rdpdr 0.7.0, **re-vendored 2026-10-01 from upstream
Devolutions/IronRDP@e258f6a0** (the pin-bump rev). Earlier vendorings: @a5d1c682
(2026-08-05), @879ffed (2026-06-16). Pulled in via `[patch.crates-io]` in the root
`Cargo.toml`; has a standalone `[patch.crates-io]` (core/error/pdu/svc → e258f6a0) for
isolated builds, ignored in the macrdp workspace (the root `[patch]` wins). Keep that rev in
sync.

### Shape: upstream verbatim + two new files

`src/` is upstream e258f6a0 **verbatim** except four code lines across three files, all
marked `(macrdp divergence N)`:

- `pdu/efs.rs`: `DeviceAnnounceHeader::device_type()` widened `pub(crate)` → `pub`.
- `pdu/mod.rs`: `mod server_direction;` + `pub use self::server_direction::ScardControlRequest;`
- `pdu/esc/mod.rs`: `mod server_direction;`

Everything else lives in two new files: `pdu/server_direction.rs` and
`pdu/esc/server_direction.rs`. As child modules they can implement traits on, and add
methods to, their parent's types and read private fields, so no upstream line needs to
change. **Keep it that way:** the next rebase is "copy upstream `src/`, re-apply four lines,
fix what no longer compiles in our two files".

Cost of this shape: under IronRDP's own workspace lints, `clippy::multiple_inherent_impl`
fires for every type that gains a second `impl` block here. It doesn't apply to our
standalone build. If any of this is upstreamed, it folds into upstream's existing `impl`
blocks.

### Why a fork at all

macrdp's RDPDR server processor is its own (`vendor/ironrdp-server/src/rdpdr.rs`,
divergence (11) there), and it needs to build requests and parse replies in the server's
direction. Upstream has since made the EFS (drive) PDU layer two-way (#1779) and added its
own `RdpdrServer` (#1783/#1784); its ESC (smart card) layer is still client-only. Moving
macrdp onto upstream's `RdpdrServer` is a later, separate change (group 2 of the pin-bump
triage). Until then this crate supplies what our processor uses.

### Divergences

(1) **Drive (EFS) server direction** — nearly all retired at e258f6a0 (see below). What's left:
    - `device_type()` made `pub` (the server reads the announced device type).
    - `pdu/server_direction.rs`: `impl Encode + SvcEncode for ServerDriveIoRequest` —
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
      `ScardCall`. Now built from upstream's `DeviceControlRequest::encode` (new at
      e258f6a0) plus the call; same bytes as before.

    **Live-Windows rules these halves must keep** (found against mstsc + a TPM virtual smart
    card, 2026-06-18; full APDU transceive verified end to end):
    - A NULL `[unique]` referent means no deferred conformant array. The decoders check the
      referent before reading `MaximumCount` for `mszReaderNames` (`StatusReturn`, Windows
      often sends an ATR-only status), `pbRecvBuffer` (`TransmitReturn`, no card data), and
      now `msz` in `ListReadersReturn` (a length-only probe).
    - **`TransmitCall` writes a NULL `pbExtraBytes` referent when there are no extra PCI
      bytes** (`encode_io_request_ptr`). This deliberately does NOT use upstream's
      `ndr::Encode for SCardIORequest`, which writes a non-NULL referent for the same
      input; real Windows rejects that `Transmit_Call` with `STATUS_UNSUCCESSFUL`. Don't
      "simplify" it back to `send_pci.encode_ptr`.
    - Pointer-sized (8-byte) contexts/handles and the connect handle's empty embedded
      context are now upstream's own behaviour (`ScardContext`/`ScardHandle`, #1654); our
      old `u64 value + length` fix was retired in favour of it.

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

The lib is `test = false`; run the tests on a scratch copy with `test = true`
(`cargo test --lib`). 52 pass: upstream's 38 plus 14 of ours (`pdu::server_direction::tests`,
`pdu::esc::server_direction::tests`; the round trips are table-driven, so one test covers
many PDUs). Ours round-trip every request through upstream's own
decode chain and every reply from upstream's own encoder, and add hand-built byte layouts
real Windows sends (empty handle context, NULL reader names, NULL receive buffer).
**Mutation-checked 2026-10-01:** each of these, introduced alone, fails a test — non-NULL
`pbExtraBytes` referent, unconditional `msz`/reader-names/receive-buffer read, a typo in
the `ReturnCode` table, dropping the NDR string padding, skipping the connect handle's
value.

### Porting notes for the server rebase (vendored ironrdp-server)

Measured against `vendor/ironrdp-server/src/rdpdr.rs`: 10 of its compile errors come from
this crate, all mechanical.
- `Device{Create,Read,Write}Response::decode(src)` → decode the `DeviceIoResponse` first,
  then `::decode(io_response, src)`.
- The inline query-directory parse used `FileDirectoryInformation::decode` (now private) →
  use upstream's `ClientDriveQueryDirectoryResponse::decode_for_class(FILE_DIRECTORY_INFORMATION, …)`.
- `ret.return_code` / `ret.context` → `ret.return_code()` / `ret.context()` for `LongReturn`
  and `EstablishContextReturn`.
- `ListReadersReturn::readers` and `TransmitReturn::recv_buffer` are now `Option` (NULL
  referent ⇒ `None`).
- `src/rdpdr/smartcard.rs` logs `ctx.value` → `ctx.value()` (or `as_bytes()`).
- Later (group 2): adopt upstream's `RdpdrServer` and drop this fork's drive half.

## ironrdp-dvc

Local fork of ironrdp-dvc 0.8.0, **re-vendored 2026-10-01 from upstream
Devolutions/IronRDP@e258f6a0** (the pin-bump rev). `src/` is upstream verbatim plus
**one** pure addition, divergence (3). Earlier vendorings: @a5d1c682 (2026-08-05),
@879ffed (2026-06-26). Keep this vendor dir until (3) is upstreamed AND released, or
until macrdp adopts upstream's own UDP routing (see "Retiring this fork" below).
Has a standalone `[patch.crates-io]` (core/svc/pdu → e258f6a0) for isolated builds,
ignored in the macrdp workspace (the root `[patch]` wins). Keep that rev in sync.

**Patch wiring is two-sided.** Unlike the other vendored crates, ironrdp-dvc is a
**path dependency of the git-pinned ironrdp crates** (egfx / displaycontrol / echo /
rdpeusb depend on it *within the IronRDP git workspace*), so `[patch.crates-io]`
alone leaves two versions — the git copy those crates pull and the vendored copy
ironrdp-server pulls — which fail to unify (trait mismatch). The root Cargo.toml
therefore patches **both** sources to the single vendored copy:

```toml
[patch.crates-io]
ironrdp-dvc = { path = "vendor/ironrdp-dvc" }
[patch."https://github.com/Devolutions/IronRDP.git"]
ironrdp-dvc = { path = "vendor/ironrdp-dvc" }
```

That is also why the fork must track upstream closely: the two-sided patch forces
this copy on the git crates too, so a stale fork breaks them (the e258f6a0 dry run
showed exactly that — upstream's `ironrdp-rdpeusb` needs `DvcClientProcessor`, which
the a5d1c682-based fork predated).

### Style / cleanliness

Copied verbatim from upstream and kept that way: the divergence is a **pure
addition** — `diff` against upstream `src/` shows **zero deletions or modifications**
of upstream lines. `rustfmt.toml` mirrors the IronRDP workspace config. Under
`cargo +nightly fmt`, upstream's own `client.rs` differs (upstream formats with stable
rustfmt, which skips the two nightly-only import options) — leave it, reformatting
upstream code would break the pure-addition property. The crate is a path dep, so the
root `cargo fmt`/`cargo test` don't reach it, and its lib is `test = false`; run its
tests on a scratch copy with `test = true` (`cargo test --lib`).

### Divergence

(3) **`DrdynvcServer::request_soft_sync(tunnel_type, channel_ids)`** (NOT upstreamed;
    added 2026-10-01 at the e258f6a0 bump). Upstream's `DrdynvcServer` now owns a
    Soft-Sync state machine (#1584 + #1954): one request per connection, and the
    client's response is validated against the request — an unsolicited, duplicate,
    or unrequested-tunnel response is an **error** out of `process()`, i.e. it ends
    the session. Its only public request API is `request_reliable_udp(non-empty
    ids)`, which also **registers** the channels for tunnelling — after which
    `process()` rejects their data over TCP ("received TCP data for a channel
    selected by Soft-Sync").

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
    routes the data itself, as it always has. Net behaviour = the pre-bump fork's.

    Tests: five in `src/server.rs`'s test module (lossy request + response + TCP data
    still accepted; empty request carries no list; unrequested-tunnel response
    rejected; one request per connection; unopened/duplicate channels rejected).
    **Mutation-checked 2026-10-01:** making the method register routing like
    `request_reliable_udp` fails the first test. Wire bytes are additionally pinned by
    macrdp's `src/multitransport.rs` tests.

(1) **RETIRED 2026-10-01 at the e258f6a0 bump.** Server-direction Soft-Sync PDU
    codec (our `SoftSyncRequestPdu` / `SoftSyncResponsePdu` / `SoftSyncChannelList`,
    the `TUNNELTYPE_UDPFEC{R,L}` / flag constants, the `switch_to_*` constructors, the
    `process()` response arm, and `DrdynvcServer::get_channel_id_by_name`). Upstream
    #1584 (Marc-André Moreau, merged 2026-08-11) added its own codec: the
    `SoftSyncTunnelType` newtype (`RELIABLE_UDP` / `LOSSY_UDP`),
    `SoftSyncRequestPdu::new(Vec<SoftSyncChannelList>)` (flags derived: TCP_FLUSHED
    always, CHANNEL_LIST_PRESENT iff a list is present — same rule as ours), and
    `SoftSyncResponsePdu::tunnels_to_switch()`. `get_channel_id_by_name` has no
    upstream equivalent; callers use `get_channel_id_by_type::<T>()` (macrdp gives
    its two audio DVCs distinct types for that).

(2) **RETIRED 2026-08-05 at the a5d1c682 bump.** Invoking `DvcProcessor::close` on a
    client Close — upstream #1302 does it via `impl Drop for DynamicChannel`.

### Retiring this fork

Two routes. (a) Upstream (3): the lossy + empty generalisation is a natural
extension of `request_reliable_udp`; the no-routing part is macrdp-specific (it
exists for the TCP fallback), so an upstream version would more likely register
routing and add an explicit "release this channel back to TCP" call. (b) Adopt
upstream's own UDP routing (#1954) at the group-2 UDP swap, at which point the TCP
fallback is re-examined anyway. Either way, verify the fallback live on mstsc.
