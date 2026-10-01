# vendor/ironrdp-server — divergence log

Local fork of ironrdp-server 0.13.0, **re-vendored 2026-10-01 from upstream
Devolutions/IronRDP@e258f6a0** (the pin-bump rev), replacing the a5d1c682 fork. The
reasoning behind each divergence (live-debugging notes, dead ends, what NOT to do)
is in **`DIVERGENCE-HISTORY.md`**, frozen at the old fork; numbers are shared, so (N)
here is (N) there. Read the history entry before changing a divergence.

Pulled in via `[patch.crates-io]` in the root `Cargo.toml`; has a standalone
`[patch.crates-io]` (vendored siblings by path, the rest git-pinned to e258f6a0) for
isolated builds, ignored in the macrdp workspace. Keep that rev in sync. The lib is
`test = false`, and macrdp is a single package, so the root `cargo fmt`/`clippy`/`test`
never reach this crate: check it with `--manifest-path vendor/ironrdp-server/Cargo.toml`
(`rustfmt.toml` matches upstream's 120 columns, and upstream's files are clean under it).

## Shape

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

## Kept divergences

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

Notes on the port:

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
- **(15)** A preemption winner's RTT is now sampled as a candidate (carried on
  `NegotiatedConnection.link_rtt_ms`); before, it inherited the previous connection's.
- **(16)** Upstream made a completion's raw `TS_URB_RESULT` bytes private, so
  `urb_completion_no_data_result` re-reads SelectConfiguration's result from the PDU,
  only after upstream's decode has validated the layout. Byte-tested against
  upstream's encoder, with two mutations caught, 2026-10-01 (scratch crate). Drop it
  once upstream exposes the bytes or a typed result.
- **(18)** Fires for preemption candidates too (`NegotiationContext` carries the
  handler); #2065's shared handler makes that borrow-safe.

## Retired at e258f6a0 (upstream has it; what macrdp calls instead)

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
  a macrdp *process* restart is now denied: live-test before release.
- **(21) mouse position before button** → `MouseEvent::Button { x, y, button, pressed }`
  (upstream #1769, fixing our report #1466). Breaking: the old positionless variants are gone.
- **(22)/(23) preemption** → builder `with_connection_policy(ConnectionPolicy::Preempt)`.
  The default is `Queue`, so omitting the call silently brings back the
  second-client hang, with no compile error. Upstream also invalidates the evicted
  client's cookie.
- **`ServerEvent::ClipboardFileCopy`** (was undocumented) →
  `ServerEvent::Clipboard(ClipboardMessage::SendInitiateFileCopy(files))`, whose arm
  is identical, including not ending the session on failure.
- Earlier: (1) PR #1276, (7) QOI at a5d1c682, (17) wheel decode at a5d1c682.

## Known leftovers

- `cargo clippy` flags two `collapsible_if`s in upstream's own code (`autodetect.rs`,
  `run()`'s `on_disconnected` call) under our newer toolchain. Left as upstream wrote them.
- Upstream's in-file `#[cfg(test)]` module never compiles here (`test = false`); its
  one `NegotiationContext` literal was kept in step with our added field anyway.
