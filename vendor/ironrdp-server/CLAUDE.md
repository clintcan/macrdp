# vendor/ironrdp-server — divergence log

Local fork of ironrdp-server 0.10.0, pulled in via `[patch.crates-io]` in
`Cargo.toml`. The audio-lag control in the dedicated `dispatch_audio` task
(carved out of `dispatch_server_events`) is the live divergence. Keep this
vendor dir until (2)/(3)/(4)/(5)/(6)/(8)/(9)/(10)/(11)/(12)/(13)/(14)/(15)/(16)/(17)/(18)/(19)/(20)/(21)/(22)/(23) below are upstreamed
AND released — #1276 landing is NOT sufficient. ((7) was HARVESTED at the a5d1c682 pin bump — see (7).)

(1) The original "keep newest queued waves on per-batch overflow"
    direction-flip LANDED upstream (PR #1276, merged 2026-05-21) — do NOT
    treat that as the reason this fork exists; it's superseded locally by (2).

(2) Cross-batch audio-lag tracker (NOT upstreamed): replaces the per-batch cap
    with a cumulative buffer-depth model (`audio_shipped_ms` vs wall-clock
    `audio_clock_start`) so slow drift from many small client pauses is caught,
    not just one big stall. Drops oldest waves when the projected client buffer
    would exceed `MAX_LAG_MS` (200). The model + the Wave dispatch itself now
    live task-local in the dedicated `dispatch_audio` task in `client_loop`
    (audio was carved out of `dispatch_server_events` onto its own bounded
    `mpsc` channel via `SoundServerFactory::set_audio_sender`); the former
    `RdpServer::{audio_shipped_ms, audio_clock_start}` fields are dead state.

(3) Resize-stall resync (NOT upstreamed): when the writer stalls (mstsc
    freezing the socket during a window resize/move/fullscreen-toggle blocks an
    EGFX video `write_all` while it holds the shared socket-writer mutex; audio
    rides its own channel + `dispatch_audio` task but every `audio_writer.
    write_all` serializes on that SAME socket lock — H.264-only, legacy bitmaps
    don't contend the same way: dirty-rect + intermittent, coalesced through
    `dispatch_display`),
    wall-clock outruns `audio_shipped_ms` and the (2) model would read it as
    "client starving" and ship the whole stale backlog late, bloating the
    buffer and compounding each stall. Fix: if deficit
    (`real_elapsed - audio_shipped`) > 300 ms, resync `audio_shipped_ms` to
    live so the backlog is dropped to one `MAX_LAG_MS` of the freshest waves.

(4) Per-batch dispatch priority (NOT upstreamed): in `dispatch_server_events`,
    stably partition the drained batch into THREE priority tiers —
    **CLIPRDR → {EGFX video + everything else} → RDPDR** — so each tier is
    written ahead of the lower ones over the shared socket writer. Two starvation
    bugs, opposite directions, same fix:
      • CLIPRDR first: without it, with `--enable-h264` a CLIPRDR
        FileContentsResponse queues behind dozens of large video frames every
        batch, throttling Mac→Windows file copies to a crawl and freezing
        Explorer's synchronous paste read.
      • RDPDR last (added 2026-06-18): a large drive transfer (a big DeviceWrite
        PDU when copying TO the redirected drive) would otherwise hold the writer
        ahead of the EGFX frames in the same batch and stutter the video — here
        video is the victim, RDPDR the bulk hog. Reorder triggers when EGFX
        shares a batch with CLIPRDR and/or RDPDR.
    Audio is deliberately NOT part of this batch at all — it ships per-wave in
    arrival order from the dedicated `dispatch_audio` task (its own channel),
    preserving the natural ~21 ms cadence; an earlier version of this patch
    lumped audio in with clipboard as "non-EGFX" and burst-shipped each batch's
    waves in a clump, which made the client's adaptive jitter buffer extend and
    added a few hundred ms of steady-state playback latency. The partition is
    stable within each tier (H.264 inter-frame sequence preserved); the audio
    wave-drop ordering is preserved independently in `dispatch_audio`. CLIPRDR
    and RDPDR ride their own SVC channels, so reordering them relative to EGFX
    within a batch breaks no on-wire ordering. Gated on the egfx feature.

(5) SuppressOutput / RefreshRectangle handling (upstream PR #1319 ✅ MERGED
    2026-05-27, commit `aa7ff679` — not yet released; vendor stays until a
    published release carries it): in `handle_io_channel_data`, pattern-match the two PDUs instead of
    warn-and-drop, and flip a shared `Arc<AtomicBool> display_suppressed`
    (exposed via `display_suppressed_handle()` and overridable via
    `set_display_suppressed_handle()` so macrdp can share one flag with
    capture.rs's gate). Without honoring SuppressOutput, a minimized mstsc
    accumulates EGFX frames; the refocus chew-through locks up its input
    dispatch for seconds. See the "Honoring SuppressOutput..." quirk in
    docs/known-quirks.md for the client-side trap (first-frame arming gate +
    per-connection reset).

(6) NSCodec encoder + selection (upstream PR #1332 ✅ MERGED 2026-06-01, commit
    `54af8f67` — but NOT yet released; this vendor copy stays until a published
    `ironrdp-server` + `ironrdp-nscodec` release carries it): adds
    `mod nscodec;` in `encoder/mod.rs` (the file was previously dead code,
    never wired up), an `NsCodecHandler` that calls
    `nscodec::encode(bitmap, color_loss_level)`, a `nscodec: Option<(u8, u8)>`
    slot on `UpdateEncoderCodecs` with a matching `set_nscodec`, a
    `BitmapUpdater::NsCodec` dispatch variant, a selection arm below RemoteFX in
    `UpdateEncoder::new`, a `has_nscodec()` on `RdpServerOptions`, and an active
    `CodecProperty::NsCodec` server-side match arm that re-uses the client's
    confirmed CLL. Verified against the macOS Microsoft Remote Desktop / Windows
    App client — that client's legacy codec list contains only NSCodec, so
    before this wiring it silently fell through to raw BitmapUpdate at much
    higher bandwidth. The new `NsCodecHandler::new` emits a `debug!` line
    ("NSCodec encoder selected for this session") so codec selection is visible
    at `RUST_LOG=...ironrdp_server::encoder=debug`. Modern FreeRDP loads the
    NSCodec decoder module at connect but doesn't advertise the codec back in
    `ClientConfirmActive`, so xfreerdp/sfreerdp don't exercise this path — only
    the macOS Microsoft Remote Desktop / Windows App does today. Upstream shape
    (as merged): same wiring but the encoder lives in a dedicated `ironrdp-nscodec`
    peer crate (CBenoit's architecture preference, confirmed in discussion #1322),
    gated by a new `nscodec` feature on `ironrdp-server`; here the vendor uses the
    in-tree `vendor/ironrdp-server/src/encoder/nscodec.rs` directly with no feature
    gate. **Post-release migration:** drop this in-tree wiring and depend on the
    published `ironrdp-nscodec` crate + enable the `nscodec` feature on
    `ironrdp-server`.

(7) Opt-in QOI Rgb-only workaround — **HARVESTED / DELETED at the a5d1c682 pin
    bump (2026-08-06).** PR #1335 (commit `8a9ee626`, server-side always-Rgb) and
    #1341 (commit `ef20ea4e`, client-side Rgba decode) are BOTH in a5d1c682, so the
    `--qoi-force-rgb` workaround was redundant. `qoi_encode` here now converges to
    upstream's exact form (every 4-byte input → its `*x` variant, so the QOI header
    always advertises `Channels::Rgb`, the only variant `ironrdp-session`'s
    `fast_path.rs::qoi_apply` decodes without blanking). Removed: the
    `QOI_FORCE_RGB` static + `set_qoi_force_rgb` setter + the `lib.rs` re-export
    (here), and macrdp's `--qoi-force-rgb` CLI flag + wiring + `docs/configuration.md`
    entry. The old default (emit natural `*a`/Rgba, which blanked pre-#1341 clients)
    is gone — always-Rgb, matching upstream. mstsc / MS Remote Desktop / Windows
    App / FreeRDP don't advertise QOI and are unaffected either way.

(8) AudioWave carries an explicit per-wave duration (NOT upstreamed): the
    `AudioWave` tuple in `src/sound.rs` gained a third field
    `Option<f64> duration_ms`, and the `dispatch_audio` task now uses
    `duration_ms.unwrap_or_else(|| data.len() as f64 / BYTES_PER_MS)` for
    `wave_ms` instead of always deriving it from byte length. Required for the
    `--enable-aac` path in macrdp: a compressed AAC access unit is ~120 bytes
    for ~23 ms of audio, so the hardcoded PCM `BYTES_PER_MS = 176.4` would read
    the projected client buffer as near-empty and silently disable the
    drop-oldest / resync lag control (divergences (2)/(3)). The PCM path passes
    `None` and is byte-for-byte unchanged. Small and upstreamable (generalizes a
    PCM-only constant to any advertised codec); offer it upstream alongside the
    SuppressOutput work. Until then it rides with this fork.

(9) Honor-client-desktop-size plumbing (UPSTREAMED as #1373, MERGED 2026-07-02
    — DROP ON PIN BUMP; pairs with the `vendor/ironrdp-acceptor` divergence (1),
    also #1373). **HARVEST ASSESSED 2026-08-06 (a5d1c682 has #1373 + #1404
    in-base) but DEFERRED:** unlike the acceptor half (already converged at the
    bump to the upstream `Option<DesktopSize>` form), the SERVER half still carries
    macrdp's older TWO-field shape (`honor: bool` + `max: Option<DesktopSize>`) +
    two runtime setters, and upstream a5d1c682 exposes only a single
    `Option<DesktopSize>` via the `with_honor_client_desktop_size` BUILDER (no
    runtime setter). Converging means reshaping the fields/setters, updating
    divergence-(23)'s `NegotiationContext` (which threads both honor-size fields —
    the preemption path), and restructuring `main.rs` to compute the `Option` before
    the builder — mechanical + low behavioral risk (same value forwarded to the
    already-converged acceptor) but multi-site + touching preemption, so it wants a
    real-client resolution-adopt pass. Do it as a focused follow-up, not on the
    soaking pin-bump branch. `RdpServer` gains a
    `honor_client_desktop_size: bool` (default false) + setter
    `set_honor_client_desktop_size`, forwarded in `run_connection` to each
    connection's `Acceptor` via the vendored
    `Acceptor::set_honor_client_desktop_size`. With it set, the acceptor
    adopts the desktop size the client requests in its GCC Client Core Data
    BEFORE Demand Active is sent, so the session is negotiated at the
    client's resolution from the start (no deactivation-reactivation
    resize). The display handler observes the adopted size through the
    existing `request_initial_size` call — conformant clients echo the
    Demand Active size in their Confirm Active bitmap capset, which is also
    why the confirm-active capset alone could never reveal the client's own
    request (verified empirically with sdl-freerdp `/size:1024x768`:
    confirm-active echoed the server's 1512×982). macrdp wires this from
    its default-on client-resolution auto-adopt (`--no-client-resolution`
    opts out). Upstream form is the builder method
    `RdpServerBuilder::with_honor_client_desktop_size` (#1373); on the next pin
    bump, drop this divergence and switch main.rs's setter call to the builder.
    **Extension (2026-07-09):** `honor_client_desktop_size_max:
    Option<DesktopSize>` + setter `set_honor_client_desktop_size_max`,
    forwarded to the acceptor alongside the bool — the operator ceiling for
    the honored size (macrdp's `--max-client-size`, defense-in-depth; the
    clamp itself lives in the vendored acceptor, see its divergence (1)
    extension). Mirrors upstream PR #1404 (OPEN); on the pin bump past it,
    drop this and adopt the upstream `Option<DesktopSize>` honor-size API.

(11) Server-side RDPDR (drive redirection) static channel (NOT upstreamed;
    added 2026-06-16; depends on vendored `ironrdp-rdpdr` divergence (1)):
    a new `src/rdpdr.rs` houses `RdpdrServer` (a `SvcServerProcessor` peer to
    the client `Rdpdr`) that drives the MS-RDPEFS init handshake (Server
    Announce → capability exchange → Client-ID Confirm → User-Logged-On) and
    surfaces the client's announced devices to a `RdpdrServerHandler` backend,
    plus the `RdpdrServerFactory`/`RdpdrBackendFactory` traits + `AnnouncedDevice`
    (exported from `lib.rs`). Wiring mirrors cliprdr/rdpsnd exactly: a
    `rdpdr_factory: Option<Box<dyn RdpdrServerFactory>>` field on `RdpServer`, a
    `RdpServer::new` param with `set_sender` wiring, attachment in
    `attach_channels` **right after rdpsnd** (MS-RDPEFS requires rdpdr be
    co-advertised with rdpsnd), and `RdpServerBuilder::with_rdpdr_factory`.
    `ironrdp-rdpdr` added to Cargo.toml deps for the wire types. The server's
    static-channel `start()` dispatch (`client_accepted`) ships the Server
    Announce — no extra send path needed for the handshake. Phase 1b added
    device I/O: an `IoRouter` (completion-id → oneshot, like clipboard's
    `DownloadRouter`), an async `RdpdrHandle` (`read_file` = create→read→close,
    wired with the connection's event sender by `build_rdpdr` and handed to the
    backend via `RdpdrServerHandler::set_handle`), a `ServerEvent::Rdpdr`
    variant + dispatch arm (encodes the handle's `SvcMessage`s on the rdpdr
    channel), and `RdpdrServer::process` routing `CoreDeviceIoCompletion`
    responses back to the waiting caller by completion id. `RdpdrHandle` exposes
    `read_file` (create→read→close) and `list_dir` (create→query-directory loop
    until NO_MORE_FILES→close, returning `DirEntry`s).
    Phase 2 added the **write** half of `RdpdrHandle`: `write_file`
    (open→`DeviceWrite`→close), `create_file`/`create_dir` (`DeviceCreate` with
    FILE_OPEN_IF/FILE_CREATE), `remove`/`rename`/`set_len` (open→`SetInformation`
    FileDisposition/FileRename/FileEndOfFile→close), plus a generalized
    `open_with` (explicit `CreateDisposition`; `create_with` now delegates with
    FILE_OPEN) and a `file_write_access()` rights set. These depend on the
    `ironrdp-rdpdr` Phase-2 encode halves (divergence (1) there).
    `read_file`/`write_file` reuse a kept-open handle via an LRU `HandleCache`
    (`acquire`/`invalidate`/`evict_path`, cap `MAX_OPEN_HANDLES`, keyed by
    `(device_id, path, Read|Write)`) so sequential I/O is ~1 open + N ops instead
    of N×(open+act+close); `evict_path` closes cached handles before
    `remove`/`rename`. `RdpdrHandle` failures carry the `NtStatus` (`RdpdrStatus`)
    so the surface can map ACCESS_DENIED → permission-denied etc.
    Read-write; macrdp gates it behind `--enable-drive-redirection`.
    Upstreamable as the server counterpart to the client `Rdpdr`.
    Smart-card phase (2026-06-18, for `--enable-smartcard-redirection`): the same
    `RdpdrServer`/`IoRouter` plumbing now also drives **MS-RDPESC**. `RdpdrHandle`
    gained `scard_*` methods (`scard_establish_context` / `release_context` /
    `list_readers` / `get_status_change` / `connect` / `status` / `transmit` /
    `disconnect`) that ship a `ScardControlRequest` (the vendored `ironrdp-rdpdr`
    divergence (2) DR_CONTROL_REQ), await the completion via the existing router,
    and decode the `*Return` — surfacing the PC/SC `ReturnCode` (distinct from the
    transport `NtStatus`) as an error unless `Success`. Methods live on
    `RdpdrHandle` (not a separate handle) because the completion-id space + event
    sender are shared with drive I/O, and esc already owns the name `ScardHandle`
    (the PC/SC card handle). `SCARD_SHARE_*` / `SCARD_*_CARD` constants are
    re-exported from `lib.rs`. The `CoreDeviceIoCompletion` router already routes
    these (a smart-card IOCTL completion is just another `DeviceIoResponse`).
    Made `LongReturn`/`EstablishContextReturn` fields `pub` in `ironrdp-rdpdr` so
    the handle can read them. `scard_transmit` takes a `recv_len` (the caller's
    `*RxLength`, forwarded from the IFD handler so extended-length responses
    aren't capped) and clamps it to the MS-RDPESC `cbRecvLength` range
    [256, 0x10000]. The macrdp-side backend + IFD socket bridge live in
    `src/rdpdr/{mod,smartcard}.rs`.
    **VERIFIED end-to-end 2026-06-18 on mstsc** with a TPM virtual smart card:
    establish-context / list-readers / get-status-change (ATR) / connect / a full
    APDU transceive (GIDS SELECT → FCI + `90 00`) all round-trip through the
    redirected reader to macOS PC/SC. The NDR conformance fixes that made it work
    are in `ironrdp-rdpdr` divergence (2).

(10) Publish the client's keyboard-layout id to a shared cell (NOT
    upstreamed; added 2026-06-16; pairs with `vendor/ironrdp-acceptor`
    divergence (2)): `RdpServer` gains `keyboard_layout: Option<Arc<AtomicU32>>`
    (default None) + setter `set_keyboard_layout_handle`, mirroring the
    `display_suppressed` shared-flag pattern (divergence (5)). In
    `client_accepted`, the server stores `result.keyboard_layout` (the KLID the
    acceptor captured from Client Core Data) into the cell. macrdp hands the
    same `Arc<AtomicU32>` to its `MacInputHandler`, which auto-selects a
    matching non-US keyboard layout when `--keyboard-layout` isn't given
    (`src/keyboard_layout.rs`; US 0x0409 / unknown 0 keep the positional
    keycode path). Additive + matches the existing handle-setter pattern, so
    upstreamable alongside the acceptor change. Verified live: sdl-freerdp
    `/kbd:layout:0x040C` → server logs `client keyboard layout announced
    klid=1036`, input handler logs `auto-selected … layout=com.apple.keylayout.French`.

(12) RDP UDP multitransport (MS-RDPEMT) server support — **M1: negotiation only**
    (NOT upstreamed; added 2026-06-25; behind the new `multitransport` cargo
    feature, default OFF so the standard build is byte-identical; pairs with
    `vendor/ironrdp-acceptor` divergence (3)). New `src/multitransport/mod.rs`
    defines the `MultitransportProvider` trait (M1: one method,
    `requested_protocol()`) + `MigrationState`. `RdpServer` gains
    `multitransport: Option<Box<dyn MultitransportProvider>>` (setter
    `set_multitransport_provider`, mirroring the handle-setter pattern) and a
    per-connection `multitransport_migration: Option<MigrationState>`. In
    `client_accepted` (initial accept only, not reactivation),
    `maybe_offer_multitransport` sends a `MultitransportRequestPdu`
    (`UdpFecR`/reliable) on the IO channel via a `SendDataIndication`
    (BasicSecurityHeader-wrapped; NOT a ShareControl PDU — framed like
    `encode_share_data_pdu` minus the ShareControl/ShareData wrapping) when the
    client advertised `TRANSPORT_TYPE_UDP_FECR` + `SOFT_SYNC_TCP_TO_UDP`
    (from acceptor divergence (3)). `handle_io_channel_data` tries `ShareControl`
    decode first and, on failure, decodes a `MultitransportResponsePdu`
    (re-validates the `TRANSPORT_RSP` flag) → `handle_multitransport_response`.
    **(SUPERSEDED in M3c: this post-finalization, IO-channel send was rejected by
    real clients — emission moved into the acceptor's `LicensingExchange` on the
    message channel; see the M3c note below. `maybe_offer_multitransport` and the
    `handle_io_channel_data` response-decode branch were removed.)**
    **M1 has NO UDP listener**: the client's out-of-band UDP attempt times out
    and it reports `E_ABORT`, and the session continues on TCP unchanged — this
    proves the negotiation/framing contract + graceful fallback before any
    socket code. The negotiation PDUs (`MultitransportRequest/ResponsePdu`,
    `RequestedProtocol`) and GCC `MultiTransportFlags` already exist in
    `ironrdp-pdu` (unused upstream); only the server-side wiring is new. Later
    milestones add `listener`/`session`/`router`/`migration` submodules (the UDP
    transport + EGFX channel migration) and grow the trait. Feature-off path is
    cfg-split to keep the original `?`-based decode byte-identical. See
    `docs/rdp-udp-multitransport-feasibility.md` (the M1→M5 plan).
    **M3b (added 2026-06-25): the UDP listener.** New
    `src/multitransport/listener.rs` (`UdpMultitransportListener` +
    `ListenerConfig`, re-exported from `lib.rs`) owns a `tokio::net::UdpSocket`,
    demuxes inbound datagrams by peer address, and drives a per-peer
    `RdpeudpState` (from the new `ironrdp-rdpeudp` dep, pulled in by the
    `multitransport` feature: `multitransport = ["dep:ironrdp-rdpeudp"]`) through
    the RDPEUDP SYN→SYN+ACK handshake — answering a real client's SYN with the
    wire-correct SYN+ACK (M3a) and negotiating V3/EUDP2. SYN-family packets are
    zero-padded to the MTU (`Datagram::peek_fec_flags` detects them). Background
    task; `Drop` aborts it. **Cookie validation is soft** (the SYNEX `cookieHash`
    is logged, not verified) — the listener produces the first macrdp↔client
    capture needed to derive the hash formula; strict binding + wiring the
    listener to bind on connect (with the `MigrationState` cookie, on the same
    port as TCP) is M3c. Not yet connected to the server's accept path or to a
    data consumer — it's a standalone, separately-testable unit. Tested via a
    loopback integration test in the **macrdp** crate (this vendored crate is
    `test = false`): bind to `127.0.0.1:0`, send a real captured client SYN, assert
    the MTU-padded SYN+ACK fields. Cross-platform (pure tokio/std), so Linux CI
    runs it.
    **M3c (added 2026-06-25): wired into the accept path + offer moved to the
    acceptor; VERIFIED on real mstsc.** Two parts:
    1. macrdp's `main.rs` binds the listener on the same address/port as TCP at
       startup when `--enable-udp-multitransport` is set (single-process path;
       `--fork-workers` warns + falls back — the persistent UDP socket would
       belong to the supervisor, deferred).
    2. The negotiation offer + emission **moved out of the server crate into the
       acceptor** (acceptor divergence (3) M3c) because the M1 post-finalization
       IO-channel send was rejected by real clients. `run_connection` now, when a
       `MultitransportProvider` is installed, calls
       `acceptor.set_advertise_extended_client_data(true)` +
       `acceptor.set_multitransport_offer(Some(new_offer(...)))`; the acceptor
       advertises EXTENDED_CLIENT_DATA, echoes SC_MULTITRANSPORT, grants
       SC_MCS_MSGCHANNEL, and emits the Initiate Request on the message channel
       after licensing (before Demand Active). `client_accepted` reads
       `result.multitransport_offered` to build the per-connection
       `MigrationState`. `new_offer(protocol) -> MultitransportOffer` (in
       `multitransport/mod.rs`) issues the process-monotonic `request_id` + a
       16-byte cookie. The old `maybe_offer_multitransport` method and the
       `handle_io_channel_data` response-decode branch were deleted.
    **VERIFIED end-to-end on real mstsc (Win11, over WiFi LAN) 2026-06-25:** mstsc
    connects cleanly (the earlier protocol-error/white-screen is gone — fixed by
    the acceptor finalize channel-skip), sends a real RDPEUDP **SYN** to the
    listener, we answer SYN+ACK, the handshake **establishes**, and mstsc starts
    pushing `ACK | DATA` datagrams (its MS-RDPEMT TLS handshake; we don't carry it
    up yet, so it retransmits w/ CWR — expected). Session renders over TCP
    throughout. FreeRDP also reaches ACTIVE + renders (it consumes the offer but
    sends no UDP — graceful fallback). **Cookie finding:** mstsc negotiates RDPEUDP
    **V2** — the 16-byte SYN `cookieHash` is V3/RDPEUDP2-only, so at V2 there is no
    SYN cookie to capture; the security cookie rides the MS-RDPEMT
    `RDP_TUNNEL_CREATEREQUEST` (behind TLS), making strict cookie binding an M4
    concern. Also de-risks M4: mstsc's reliable transport here is plain RDPEUDP
    **v2** carrying TLS (FecFlags ACK|DATA), NOT EUDP2 — so the v1 state machine is
    the correct codepath and the EUDP2 wire-format spike may be off mstsc's
    critical path for the reliable channel. The session still runs over TCP (no
    TLS/EMT tunnel or migration yet — M4).
    **M4a (added 2026-06-25): reliable data path — verified on real mstsc.** The
    listener now consumes the SM's `delivered` output: each `Peer` accumulates the
    reassembled reliable byte-stream (`Peer::inbound`) and logs it (+ a TLS
    ClientHello sniff). Getting mstsc's reliable stream to actually flow needed two
    fixes in `ironrdp-rdpeudp` (see its CLAUDE.md M4a): the SYN consumes a sequence
    number (first source packet = `initial_seq + 1`), and outbound ACKs must carry
    a populated `RDPUDP_ACK_VECTOR_HEADER` (an empty vector is ignored by mstsc →
    infinite retransmit). Result: mstsc's TLS ClientHello is delivered + acked,
    mstsc stops retransmitting and idles on `ACK|ACK_DELAYED` keepalives, waiting
    for our TLS ServerHello. No TLS/EMT yet (M4b/M4c); listener still not bound to
    the per-connection cookie/MigrationState.
    **M4b (added 2026-06-25): rustls server TLS over the reliable stream — verified
    on real mstsc.** The listener now drives a per-`Peer` `rustls::ServerConnection`
    (`Peer::tls`, created lazily on first reliable data) over the SM's reliable
    byte-stream: each delivered chunk is fed to `read_tls` + `process_new_packets`
    in a loop (until the cursor drains), `tls.reader()` is drained so the decrypted
    plaintext can't stall record processing, and any `write_tls` output is
    `enqueue`d back through the SM (reliable, MTU-fragmented via a new
    `send_datagrams` helper used for both SM and TLS output). The rustls
    `ServerConfig` is the **same cert/config as the main TCP connection** — passed
    into `UdpMultitransportListener::bind(addr, cfg, tls_config: Option<Arc<ServerConfig>>)`
    from `main.rs` (`make_tls_acceptor` now also returns the `Arc<ServerConfig>`);
    the client trusts it via the main connection's TOFU. Result on real mstsc: the
    TLS handshake **completes** (`MS-RDPEMT TLS handshake complete`) and mstsc then
    streams encrypted tunnel PDUs which decrypt cleanly (logged as `MS-RDPEMT
    decrypted tunnel bytes received plaintext_len=28`, repeating ~every 200 ms — its
    `RDP_TUNNEL_CREATEREQUEST` retransmitted while it waits for our `CREATERESPONSE`
    = M4c). The decisive interop fix was in `ironrdp-rdpeudp` (skip the inbound
    ACK_OF_ACKS section — see its CLAUDE.md M4b): mstsc sets that flag periodically
    once up, and folding its 4 bytes into the stream corrupted the TLS records
    (`InvalidContentType`). The `tls_config: None` path (the loopback handshake
    test) is unchanged. Still no EMT tunnel parsing / cookie binding / migration
    (M4c/M5); the 28-byte plaintext is logged, not yet parsed.
    **M4c (added 2026-06-25): MS-RDPEMT tunnel established — verified on real
    mstsc.** The listener now parses the decrypted tunnel PDUs and answers the
    handshake. Each `Peer` accumulates TLS-decrypted plaintext (`emt_inbound`);
    `handle_emt_tunnel` frames complete tunnel PDUs (via `ironrdp_rdpeudp::emt`'s
    `peek_pdu_len`) and, on the client's `RDP_TUNNEL_CREATEREQUEST`, writes a
    `RDP_TUNNEL_CREATERESPONSE(S_OK)` into the TLS connection — whose encrypted
    bytes the existing `write_tls` drain ships back through the SM. Gated by
    `Peer::tunnel_created` so the client's retransmits (it resends the request
    until it sees the response) are answered exactly once. The TLS block now
    destructures `Peer` into disjoint field borrows so the rustls feed/write runs
    alongside the EMT buffer/state. Result on real mstsc: CREATEREQUEST
    (request_id=2, the issued cookie echoed back verbatim) → our CREATERESPONSE →
    mstsc ACKs, **stops retransmitting**, tunnel idles on `ACK|ACK_DELAYED`
    keepalives = established. **Cookie binding is still SOFT** (logged + verified
    by eye to match the issued offer; we reply S_OK regardless) — strict
    enforcement needs a shared (request_id, cookie) registry between the
    offer-issuing acceptor path and the process-global listener, which is an **M5
    prerequisite** (M5 is when channel data actually rides the tunnel, so
    hijack-resistance starts to matter; nothing rides it yet). RDP_TUNNEL_DATA
    (action 0x2) is logged "not handled yet (channel migration is M5)" and skipped.
    **M5a (added 2026-06-25): strict cookie binding — verified on real mstsc.**
    The tunnel is now bound to a real, current TCP session so a forged/replayed
    cookie can't open one (the security prerequisite before any data rides the
    tunnel in M5b/c). New `CookieRegistry` (multitransport/mod.rs, re-exported):
    a shared `Arc<Mutex<HashSet<[u8;16]>>>` with insert / remove / `take`
    (atomic check-and-consume = one-time use). `new_offer` now generates the
    16-byte cookie via **getrandom (CSPRNG)** instead of the predictable
    request_id-derived pattern (getrandom added under the `multitransport`
    feature; it's already in the tree via rustls so no real build-graph cost).
    `RdpServer` gained `multitransport_cookies: Option<CookieRegistry>` +
    `set_multitransport_cookie_registry`; the offer site registers the issued
    cookie (and evicts the previous connection's, so TCP-fallback connections
    leave at most ~one stale entry per live RdpServer). The listener's `bind`
    gained a `cookie_registry: Option<CookieRegistry>` param threaded to
    `handle_emt_tunnel`, which `take`s the CREATEREQUEST's echoed cookie before
    replying — on a miss it logs "cookie not recognized — rejecting tunnel" and
    sends nothing (client times out the UDP attempt, stays on TCP). `None`
    registry = soft binding (accept any cookie — the handshake-only test path).
    main.rs creates one registry and hands the same clone to both `bind` and the
    server. Verified on real mstsc: random cookie `f76fdf2b…`, "bound to session",
    tunnel establishes. One-time-use logic unit-tested in the macrdp crate
    (`cookie_registry_take_is_one_time_and_rejects_unknown`).
    **M5b-2 (merged 2026-06-26, PR #34): server-side MS-RDPEDYC Soft-Sync codec.**
    Vendored `ironrdp-dvc` (4th fork) gained `SoftSyncRequestPdu`/
    `SoftSyncResponsePdu` + the `DrdynvcClientPdu::SoftSyncResponse` decode arm so
    the client's Soft-Sync reply no longer tears the session down. No behavior
    change here yet (codec only). See `vendor/ironrdp-dvc/CLAUDE.md`.
    **M5c step 1+2 (added 2026-06-26): Soft-Sync gate + send (SAFE SPIKE — empty
    channel list) — VERIFIED on real mstsc.** KEY FINDING (from the first live
    run): **mstsc signals multitransport success by *creating the UDP tunnel*, NOT
    by an Initiate Multitransport Response over TCP.** A TCP Initiate Response only
    comes on *failure* (E_ABORT). So the success gate has to come from the
    listener, not a message-channel PDU. Two pieces:
    1. **Gate (listener-driven).** `MigrationState` gained `soft_sync_sent: bool`.
       `CookieRegistry` now maps each cookie → a shared **tunnel-bound flag**
       (`Arc<AtomicBool>`); the offer path keeps the flag (`RdpServer::
       udp_tunnel_bound`), and the listener flips it `true` inside `take()` when it
       binds the matching tunnel (cookie match). In the **EGFX dispatch arm**
       (`ServerEvent::Egfx`, after shipping a frame), `maybe_soft_sync_on_egfx`
       checks the flag: once the tunnel is bound AND EGFX is shipping (its DVC
       channel is open, client fully in DVC mode), it sends the Soft-Sync request
       exactly once (`soft_sync_sent` guard). This couples the trigger to the right
       moment (tunnel up + EGFX live), which is also where the real migration will
       hook. A SECONDARY TCP gate (`maybe_handle_multitransport_response` in
       `handle_x224`'s `else` branch — message channel) still handles a client that
       *does* send an Initiate Response (E_ABORT → stay on TCP; S_OK → also send),
       but mstsc never exercises it. (The M1 `handle_io_channel_data` IO-channel
       response-decode branch is legacy/dead for real clients, left harmless.)
    2. **Send.** `send_soft_sync_request` ships a `DYNVC_SOFT_SYNC_REQUEST` as a
       top-level `SvcMessage` on the drdynvc static channel (NOT sub-channel DATA —
       so `SvcMessage::from(DrdynvcServerPdu::SoftSyncRequest(..))`, not
       `encode_dvc_messages`). **Safe spike:** the request carries an EMPTY channel
       list (`switch_to_udpfecr(vec![])` → TCP_FLUSHED only, NumberOfTunnels=0), so
       it migrates nothing and video keeps flowing over TCP — it only proves the
       send path + the client's Soft-Sync Response decode (M5b-2) without risk.
    **Verified on real mstsc 2026-06-26:** "UDP tunnel bound + EGFX active —
    Soft-Sync gate open" → "Sent DYNVC_SOFT_SYNC_REQUEST" → mstsc replied with a
    `DYNVC_SOFT_SYNC_RESPONSE` (`tunnels: []`, decoded cleanly by the vendored
    ironrdp-dvc — the M5b-2 fix proven on the wire) → **EGFX + audio kept flowing,
    session ended only on the user's graceful disconnect.** The real migration
    (list the EGFX channel id + the server→listener data handoff + route EGFX over
    the tunnel as RDP_TUNNEL_DATA) is the next step. Watch
    `RUST_LOG=...ironrdp_server::server=debug,ironrdp_dvc=debug` for the gate /
    send / "Got DVC Soft-Sync Response" lines.
    **M5c step 3a (added 2026-06-26): real EGFX migration request + outbound
    server→tunnel route — migration ACCEPTED by real mstsc.** Behind a second env
    gate `MACRDP_UDP_MIGRATE_EGFX` (default OFF; when off, the M5c step-1+2 empty
    safe spike is byte-for-byte unchanged — verified no-regression on mstsc). When
    set:
    1. **Name the EGFX DVC in the Soft-Sync.** `DrdynvcServer` gained
       `get_channel_id_by_name` (vendored `ironrdp-dvc`); `maybe_soft_sync_on_egfx`
       looks up `"Microsoft::Windows::RDS::Graphics"`, sets `RdpServer::egfx_on_udp`,
       and sends `switch_to_udpfecr(vec![gfx_id])` (TCP_FLUSHED|CHANNEL_LIST_PRESENT).
    2. **Outbound handoff (server→listener).** New `tunnel_channel()` →
       (`TunnelSender`, `UnboundedReceiver<TunnelOutbound>`); `RdpServer` gained
       `multitransport_tunnel_sender` (setter `set_multitransport_tunnel_sender`).
       The `ServerEvent::Egfx` dispatch arm, when `egfx_on_udp`, calls
       `route_egfx_over_udp` — `StaticVirtualChannel::chunkify(messages)` → each
       chunk handed to the sender keyed by the connection's migration cookie
       (instead of `server_encode_svc_messages` over TCP). The listener's recv loop
       is now bidirectional (`tokio::select!` recv + an outbound arm); `ship_outbound`
       wraps each chunk in `RDP_TUNNEL_DATA` (`emt::encode_tunnel_data`), encrypts
       through the peer's MS-RDPEMT TLS, and ships it reliably over RDPEUDP (peer
       looked up by cookie via `bound_addrs`, populated when `handle_emt_tunnel`
       binds the tunnel). `main.rs` wires `tunnel_channel()` → sender to the server,
       rx to `bind`. `TunnelOutbound` is `pub` (it leaks through the public `bind`
       signature); when the feature/flag is off the outbound arm is just a
       never-ready future, so the recv-only path is unchanged.
    **VERIFIED on real mstsc 2026-06-26 — migration accepted, NOT yet rendering:**
    `Sent DYNVC_SOFT_SYNC_REQUEST (migrating EGFX onto the UDP tunnel) gfx_channel_id=3`
    → mstsc replied `DYNVC_SOFT_SYNC_RESPONSE { tunnels: [1] }` (**it agreed to
    switch the EGFX channel onto the reliable UDP tunnel** — the migration request
    is wire-correct). Then the session froze (~300 ms) and the user disconnected.
    **Root cause (diagnosed, this is step 3b):** after migration EGFX is
    bidirectional over the tunnel — mstsc's **frame acknowledgements** come back as
    inbound `RDP_TUNNEL_DATA` (`action=2 len=10`, logged "not handled yet"), and we
    drop them; macrdp's H.264 ship loop gates on frame acks (`max_in_flight=2`), so
    after ~2 unacked frames it stalls → frozen video. The OUTBOUND route + the
    migration handshake are proven; the missing piece is the **inbound tunnel →
    drdynvc path**: un-tunnel the HigherLayerData (an SVC channel-data blob =
    `CHANNEL_PDU_HEADER` + DRDYNVC PDU, identical to what `handle_x224` feeds
    `svc.process(&data.user_data)` over TCP), route it back to the owning
    connection's `client_loop` (a per-cookie reverse channel), and run it through
    the drdynvc `StaticVirtualChannel::process` so the EGFX handler sees the acks
    (shipping any reply PDUs back over the tunnel). That reverse handoff is M5c
    step 3b. Watch the gate/send lines plus
    `MACRDP_UDP_MIGRATE_EGFX: Soft-Sync will migrate the EGFX DVC` and (listener)
    `MS-RDPEMT tunnel PDU not handled yet … action=2`.
    **M5c step 3b (added 2026-06-26): EGFX RENDERS over the UDP tunnel — VERIFIED
    end-to-end on real mstsc.** Two fixes, together making H.264 video flow over
    UDP (still behind `MACRDP_UDP_MIGRATE_EGFX`; default OFF unchanged + verified
    no-regression):
    1. **Outbound framing fix (the unlock).** The MS-RDPEMT tunnel carries the
       **bare DRDYNVC PDU**, NOT a static-channel-framed one — HigherLayerData has
       no `CHANNEL_PDU_HEADER` (confirmed empirically: inbound `action=2 len=10`
       ⇒ 6-byte HigherLayerData, smaller than the 8-byte header; and ironrdp-svc
       exposes `SvcMessage::encode_unframed_pdu` documented "for RDPEMT tunnel
       data"). Step 3a wrongly used `StaticVirtualChannel::chunkify`, which prepends
       `CHANNEL_PDU_HEADER`, so mstsc misparsed the EGFX stream → froze.
       `route_egfx_over_udp` now encodes each message via `encode_unframed_pdu` (one
       tunnel PDU per message; `encode_dvc_messages` already DVC-chunked them to
       fit). This alone makes video render — macrdp's H.264 throttle is
       `submitted − shipped` (ack-INDEPENDENT, `max_frames_in_flight = u32::MAX`),
       so dropping inbound frame acks never stalled it; the freeze was purely the
       garbled outbound framing.
    2. **Inbound tunnel→drdynvc path (protocol correctness).** `CookieRegistry`
       now stores a per-cookie inbound sink (`register(cookie, sender)`; `take`
       returns the sink on bind). The listener's `Peer` gained `inbound_sink`, set
       on a cookie-bound CREATEREQUEST; on inbound `RDP_TUNNEL_DATA` (action 0x2) it
       extracts the HigherLayerData (`emt::tunnel_data_payload`) and forwards the
       bare DRDYNVC PDU to the owning connection (instead of the old log-and-drop).
       `RdpServer` gained `multitransport_tunnel_inbound_rx` (created at the offer
       site alongside the cookie registration); `client_loop` adds a
       `dispatch_tunnel_inbound` select arm that drains it into
       `process_tunnel_inbound` → `DrdynvcServer::process` (no CHANNEL_PDU_HEADER to
       strip — the tunnel replaced that layer), shipping any reply PDUs back over
       the tunnel. Idle-forever (`std::future::pending`) when there's no inbound
       tunnel, so feature-off / no-migration / soft-bound paths are unchanged.
    **VERIFIED on real mstsc 2026-06-26:** `tunnels: [1]` accepted → EGFX H.264
    **renders and stays live** (mouse/keyboard/window changes all update over UDP),
    inbound `RDP_TUNNEL_DATA` now delivered+processed (the "not handled yet" lines
    are gone), audio still on TCP (RDPSND channel 1005) throughout. As far as is
    known this is the first open-source RDP **server** with a working UDP
    multitransport *data path* (serving real EGFX video over the tunnel) — FreeRDP,
    the most complete OSS stack, has **no working UDP data path on either side**:
    its server is a multitransport *bootstrap stub* (`multitransport_server_request`
    / `_handle_response`, no UDP socket or data path) and its client declines UDP
    with `E_ABORT` (`multitransport_no_udp`). The RDPEUDP/RDPEUDP2 work (David Fort,
    2021/2023 blog posts) is out-of-tree prototype only — never a merged PR or
    released feature (re-verified against full FreeRDP git history 2026-06-26). xrdp
    / ogon / gnome-remote-desktop / Weston are TCP-only or ride FreeRDP's server
    lib. ("first" can't be proven exhaustively — phrase it "first known".)
    Flag-OFF (empty safe spike) re-verified no-regression.

    **Soak observability (added 2026-06-26):** the listener now logs the reliable
    transport's RTO retransmits — `RDPEUDP RTO retransmit` at the inbound-driven
    `step()` site and `RDPEUDP RTO retransmit (outbound)` at the server-data
    `enqueue()` site, gated `if retransmits > 0 || syn_retransmit` so a clean link
    stays silent. The counts come from the new `StepOutput.{retransmits,
    syn_retransmit}` diagnostic fields (`ironrdp-rdpeudp`; the SM stays sans-I/O —
    it counts, the listener logs). For a lossy-link soak run with
    `RUST_LOG=ironrdp_server::multitransport::listener=debug`; protocol + the
    `scripts/netshape.sh` shaper are in `docs/rdp-udp-multitransport-feasibility.md`
    ("Soak testing the UDP path under loss").

    **Soak fix — periodic retransmit timer (added 2026-06-26):** `run_recv_loop`'s
    `tokio::select!` now has a third arm, a `retransmit_tick` interval at ¼ RTO that
    calls `pump_peers_on_timer` (`step(now, None)` on every established peer, sending
    due retransmits / queued data / owed ACKs; logs `RDPEUDP RTO retransmit (timer)`).
    WHY: the SM only retransmits when pumped, and pumps were driven *solely* by
    inbound datagrams / new outbound data. The **first 5%-loss soak deadlocked** —
    the initial EGFX burst lost segments, the screen went static (no new frames), the
    client went quiet waiting, so nothing pumped → lost segments never resent → window
    filled → frozen (blank). The timer pumps during silence; full screen then
    rendered. Idle ticks emit nothing (pump only sends what's due), so a clean link is
    unaffected. **NOTE the soak also exposed a *structural* limit this does NOT fix:**
    the EGFX channel rides one reliable *ordered* RDPEUDP stream, which HOL-blocks on
    its own loss like TCP — so reliable-only multitransport does not beat TCP for
    video under loss; that needs Phase 2 (lossy `UdpFecL` + FEC). See the feasibility
    doc "First soak findings".

    P2.0 go/no-go spike (2026-06-26, `multitransport/listener.rs`): observe-only
    support for a LOSSY flow, to answer whether modern mstsc opens one at all
    before committing to a DTLS+FEC build. `sniff_dtls_client_hello` scans each raw
    datagram for a DTLS handshake record (`0x16 0xFE 0xFF`/`0xFD`); on a match the
    peer is marked `dtls_observed`, logged at WARN ("P2.0 SPIKE GREEN …"), and the
    rustls (TLS) feed is skipped for it (DTLS records aren't TLS — feeding them is
    error spam; no DTLS is implemented). Paired with the env-gated `UdpFecL` offer
    (`MACRDP_UDP_OFFER_FECL=1` in `src/multitransport.rs`) + the acceptor's
    matching `SC_MULTITRANSPORT` advertise. **Result: GREEN on real mstsc** — it
    opens a `SYN_LOSSY` V2 flow and sends a **DTLS 1.2** ClientHello; the TCP
    session is unaffected (the lossy handshake just retries unanswered). Default
    build/offer is unchanged reliable; this is harness for the Phase 2 lossy work.
    See the feasibility doc "P2.0 — Go/No-Go spike".

    P2.1a — DTLS 1.2 server handshake on the lossy flow (2026-06-26,
    `multitransport/dtls.rs`, gated `dep:boring` under the `multitransport`
    feature): a sans-I/O DTLS 1.2 server over boring's custom-BIO `SslStream`
    (`DtlsServerContext::from_der` built from the same cert as the TCP/reliable
    path; per-peer `DtlsConn::read_datagram` fed one delivered chunk = one
    datagram at a time — the load-bearing boundary rule). The listener creates a
    `DtlsConn` for a `dtls_observed` peer when a `DtlsServerContext` is passed to
    `bind` (6th arg; `None` = observe-only), feeds delivered datagrams, and ships
    the handshake flights back via the reliable SM's `enqueue`. **Verified GREEN
    on real mstsc**: full DTLS 1.2 handshake completes in ~13 ms / ~2 RTT over the
    lossy flow, no errors. boring config: `SslMethod::dtls()` + `set_mtu(1100)` +
    `SslOptions::NO_DTLSV1` (boring has no DTLS `SslVersion` consts) + `set_verify(NONE)`;
    no cookie exchange (BoringSSL dropped `DTLSv1_listen`). BoringSSL is vendored +
    built from source (cmake+Go+libclang; coexists with rustls aws-lc-rs via symbol
    prefixing). Scope is handshake-only — post-handshake the client retransmits an
    encrypted EMT `CREATEREQUEST` we don't answer yet (P2.4). Default build pulls in
    boring (multitransport feature always-on for macrdp); runtime DTLS path only
    runs for a lossy peer, i.e. only when `MACRDP_UDP_OFFER_FECL=1`. See the
    feasibility doc "P2.1 … Result (P2.1a)".

    P2.4a — MS-RDPEMT tunnel over DTLS (2026-06-26): post-handshake, decrypt the
    client's DTLS app records (`DtlsConn::recv` = `ssl_read`), parse the EMT PDUs,
    answer `RDP_TUNNEL_CREATEREQUEST` with `CREATERESPONSE(S_OK)` re-encrypted
    through DTLS (`DtlsConn::send` = `ssl_write`), binding via the same cookie
    registry as the reliable flow. `handle_emt_tunnel` was made transport-agnostic
    — it no longer writes into the rustls conn; it returns `EmtTunnelOutcome
    { bound_cookie, response }` and the caller encrypts the response via rustls
    (reliable) OR DTLS (lossy). Reliable path behavior unchanged (writes the same
    bytes at the same point, before its `wants_write` drain). **Verified GREEN on
    real mstsc**: CREATEREQUEST cookie matched the issued cookie → CREATERESPONSE
    sent → tunnel established AND the client STOPPED retransmitting (the definitive
    accept signal). P2.4b (migrate the AUDIO_PLAYBACK_LOSSY_DVC audio channel onto
    the tunnel) is now de-risked — see P2.4b-2 below. See feasibility doc "P2.4a".

    P2.4b-1 — audio output DVC handshake (2026-06-27, `multitransport/audio_dvc.rs`;
    PAUSED after the spike): a `DvcProcessor`/`DvcServerProcessor` (`AudioLossyDvc`)
    that, on channel open, sends Server Audio Formats (v8) and runs the MS-RDPEA
    format/quality-mode/training handshake, reusing `ironrdp-rdpsnd`'s
    `ServerAudioOutputPdu`/`ClientAudioOutputPdu` codecs verbatim — the SNDPROLOG
    header is KEPT (byte-identical to the static rdpsnd path), wrapped in an
    `OwnedAudioPdu` that delegates `Encode` (so the DRDYNVC framing length matches).
    Registered in `attach_channels` (after the egfx block) ONLY when the application
    calls `RdpServer::set_multitransport_lossy_audio_formats(Some(formats))` — macrdp
    gates that behind the experimental `MACRDP_UDP_LOSSY_AUDIO` env (+ `--enable-aac`),
    so the default build is byte-unchanged. **KEY FINDING (verified on real mstsc): the
    EGFX "negotiate-on-TCP-then-Soft-Sync" pattern does NOT carry to a *lossy*-named
    channel.** With the literal name `AUDIO_PLAYBACK_LOSSY_DVC`, mstsc accepts the DVC
    Create but, on receiving Server Audio Formats over TCP/DRDYNVC, goes silent and
    **stops reading the whole TCP socket** (broken pipe ~3–4 s later — it tears down
    EGFX/everything, not just audio). The reliable name `AUDIO_PLAYBACK_DVC`
    (diagnostic env `MACRDP_AUDIO_DVC_RELIABLE=1`) handshakes perfectly over TCP
    (formats → client formats(AAC) → quality mode → training → confirm), audio plays,
    EGFX stays healthy — the discriminator proving the channel *name* is the blocker,
    not the PDU/framing and not coexistence with static rdpsnd (dual negotiation is
    fine). So the lossy DVC must be **Soft-Synced onto the lossy tunnel BEFORE any
    data**, with the handshake over the tunnel (opposite of EGFX, which migrates to the
    RELIABLE tunnel *after* a TCP handshake). Spec note (confirmed live): for v6+ the
    client sends Quality Mode immediately after Client Audio Formats and the server
    sends Training only after that (MS-RDPEA Initialization Sequence) — the handler
    waits for Quality Mode. The reliable-DVC path that works over TCP is NOT worth
    landing on its own (a reliable tunnel HOL-blocks under loss like TCP — no win).

    P2.4b-2 — lossy Soft-Sync of the audio DVC ACCEPTED by mstsc (2026-06-27, the
    linchpin de-risk; `audio_dvc.rs` + `server.rs`; verified on real mstsc): the open
    question P2.4b-1 left was whether a *lossy* Soft-Sync itself trips mstsc or only
    format-data-over-TCP. Answer: only the latter. `AudioLossyDvc::start()` now returns
    NO formats for the lossy name (defers the handshake — P2.4b-1 finding), and the
    server Soft-Syncs the channel onto the lossy tunnel:
    `send_soft_sync_request(.., dvc::pdu::TUNNELTYPE_UDPFECL, vec![audio_id])`. The
    trigger is a new branch in `maybe_soft_sync_on_egfx` (gated on
    `multitransport_lossy_audio_formats.is_some()`, placed after the `udp_tunnel_bound`
    check, before the EGFX one-time guard): it resolves the lossy DVC's channel id via
    `DrdynvcServer::get_channel_id_by_name(AUDIO_PLAYBACK_LOSSY_DVC)` (retries next frame
    if not open yet, guard intact), claims the one-time `MigrationState::soft_sync_sent`
    guard, and Soft-Syncs FECL. `send_soft_sync_request` gained a `tunnel_type: u32`
    param (uses `SoftSyncRequestPdu::switch_to_tunnel`); the two pre-existing callers
    pass `TUNNELTYPE_UDPFECR`. Live mstsc result: `lossy audio DVC opened — deferring
    Server Audio Formats` → `Sent DYNVC_SOFT_SYNC_REQUEST` → `SoftSyncResponsePdu {
    tunnels: [3] }` (UDPFECL accepted) → session stayed alive to a graceful disconnect,
    EGFX (on TCP) acking the whole time. So the #54 blocker is format-data-over-TCP, NOT
    the lossy Soft-Sync. (This run needs `--enable-h264` — the trigger rides the EGFX
    dispatch arm; EGFX itself stays on TCP, `MACRDP_UDP_MIGRATE_EGFX` off.) **Next
    (2b-iii):** run the MS-RDPEA handshake (formats → quality → training) over the
    lossy/DTLS tunnel for the migrated peer, then stream AAC waves (2b-iv) + reconcile
    the drop-stale lag model. The verified groundwork is kept in-tree, default-off
    (`MACRDP_UDP_OFFER_FECL` + `MACRDP_UDP_LOSSY_AUDIO`). See feasibility doc "P2.4b".

    P2.2 step 2 — lossy flow uses lossy delivery (2026-06-27, `listener.rs`; verified
    on real mstsc). Behind the experimental env `MACRDP_UDP_LOSSY_DELIVERY` (read once
    at the top of `run_recv_loop`; default off), the listener classifies each flow at
    its opening SYN — `Datagram::peek_fec_flags(data)` containing `SYN_LOSSY` ⇒ the
    `UdpFecL` flow (the first datagram from any peer is always its SYN, so peeking at
    `peers.entry(..).or_insert_with` time is reliable) — and builds that peer's
    `RdpeudpState` with `DeliveryMode::Lossy` (P2.2 step 1) instead of `Reliable`. The
    reliable (`UdpFecR`) flow is unaffected; with the env unset every peer stays
    `Reliable` (the proven P2.1a/P2.4a path), so it's a clean A/B + instant fallback.
    **Verified live (mstsc, env set):** lossy peer logs `RDPEUDP peer using LOSSY
    delivery`, then the DTLS 1.2 handshake (`P2.1 GREEN`) AND the MS-RDPEMT tunnel
    (`P2.4 GREEN`) both reach established over send-once/no-retransmit delivery —
    DTLS's own record reordering + a clean link (one-shot flights all arrive) carry it
    without transport retransmission. **Under-loss caveats (soak-phase, NOT yet
    addressed):** the SM no longer resends the one-shot `CREATERESPONSE(S_OK)` / DTLS
    server flight, and `handle_emt_tunnel`'s `tunnel_created` guard answers a repeated
    `CREATEREQUEST` only once — so a dropped handshake datagram under real loss may
    stall the tunnel; making the response idempotent (re-answer on retransmit in lossy
    mode) or adding a handshake-phase retransmit is the next soak fix. See feasibility
    doc "P2.2".

    P2.4b 2b-iv-A — dual audio-DVC topology (2026-06-27, `audio_dvc.rs` + `server.rs`;
    verified on real mstsc). The lossy-audio design uses BOTH audio DVCs at once: the
    RELIABLE `AUDIO_PLAYBACK_DVC` runs the full MS-RDPEA format/quality/training
    handshake over TCP/DRDYNVC (this is what mstsc tolerates — the P2.4b-1 finding:
    Server Audio Formats on a `_LOSSY_`-named channel makes mstsc tear down the whole
    TCP socket), and the LOSSY `AUDIO_PLAYBACK_LOSSY_DVC` is data-only (no formats) and
    Soft-Synced onto the UDPFECL/DTLS tunnel (P2.4b-2). The lossy channel **inherits the
    reliable channel's negotiated format index**: `AudioLossyDvc` carries a shared
    `NegotiatedAudioFormat(Arc<AtomicU32>)` — the reliable instance publishes the chosen
    `wFormatNo` via `shared.set(idx)` on TrainingConfirm ("P2.4b GREEN: reliable audio
    DVC negotiated + training confirmed over TCP"); the server reads it back through
    `lossy_audio_format.get()`. Two constructors: `AudioLossyDvc::reliable(formats,
    negotiated)` (defer_formats=false, sends formats, runs the handshake) and
    `::lossy()` (defer_formats=true, `start()` returns empty). Both registered in
    `attach_channels` when `set_multitransport_lossy_audio_formats(Some(..))` is set
    (macrdp gates that behind `MACRDP_UDP_LOSSY_AUDIO` + `--enable-aac`, so the default
    build is byte-unchanged). The "video freezes on connect" seen once during this work
    was transient mstsc state (Run A, byte-identical, rendered fine next attempt — the
    documented "mstsc caches bad RDP state until reboot" behavior), NOT a topology bug.

    P2.4b 2b-iv-B — AAC Wave2 streamed over the lossy tunnel; audio RENDERS over UDP
    (2026-06-27, `audio_dvc.rs` + `server.rs`; **VERIFIED end-to-end on real mstsc**).
    Once 2b-iv-A's preconditions all hold, the `dispatch_audio` task ships each wave as
    a `Wave2Pdu` on `AUDIO_PLAYBACK_LOSSY_DVC` over the bound UDP/DTLS tunnel instead of
    the static rdpsnd TCP write — exactly one playback path at a time (no double-play).
    Pieces:
    - `audio_dvc::lossy_wave_dvc_message(block_no, audio_timestamp, format_no, data)`
      builds `ServerAudioOutputPdu::Wave2(Wave2Pdu { block_no, timestamp: 0,
      audio_timestamp, format_no, data })` wrapped in the `OwnedAudioPdu`
      (`Encode`+`DvcEncode`) the DVC path uses.
    - `RdpServer::lossy_audio_target() -> Option<(format_no, lossy_channel_id)>` returns
      `Some` only when ALL hold: lossy formats registered, `lossy_audio_format.get()`
      Some (reliable handshake done), tunnel sender + migration cookie present,
      `udp_tunnel_bound` true, and `DrdynvcServer::get_channel_id_by_name(
      AUDIO_PLAYBACK_LOSSY_DVC)` Some. Until then audio stays on static rdpsnd → clean
      handover, no double-play.
    - `RdpServer::route_lossy_audio_wave(..)` (one-shot WARN marker "P2.4b 2b-iv-B:
      streaming Wave2 audio over the LOSSY UDP/DTLS tunnel … static rdpsnd now silent")
      → `encode_dvc_messages(lossy_id, [wave], empty)` → `route_dvc_over_udp` (bare
      DRDYNVC PDU as `RDP_TUNNEL_DATA`, DTLS-encrypted, shipped reliably-or-lossy over
      the SM). New per-connection fields `lossy_audio_block_no: u8` (wrapping) +
      `lossy_audio_streaming: bool` (one-shot marker), init 0/false in `new()`.
    The `dispatch_audio` branch sits AFTER the cross-batch lag model (resync +
    drop-stale): the drop-stale guard still rightly protects the tunnel from flooding
    with stale audio (correct for any live stream), the resync-on-stall is moot but
    harmless (the tunnel send is non-blocking), and `audio_shipped_ms += wave_ms` runs
    on both paths so the model stays coherent — so the lag model needs no tunnel-specific
    change (verified: clean audio in the live run). **VERIFIED on real mstsc 2026-06-27:**
    reliable `AUDIO_PLAYBACK_DVC` handshake GREEN → lossy `AUDIO_PLAYBACK_LOSSY_DVC`
    Soft-Synced onto UDPFECL (`SoftSyncResponsePdu { tunnels: [3] }`) → the one-shot
    marker fired (format_no=0, lossy_channel_id=5) → **audio plays**, the lossy UDP flow
    shows continuous client ACKs with a growing ACK-vector, EGFX (TCP) keeps acking +
    rendering, session alive to a graceful disconnect, no teardown. As far as is known
    this is the first open-source RDP server streaming **audio** over a UDP
    multitransport tunnel. All gated default-off (`MACRDP_UDP_OFFER_FECL` +
    `MACRDP_UDP_LOSSY_AUDIO`, + `--enable-aac` + `--enable-h264` since the Soft-Sync
    trigger rides the EGFX dispatch arm). See feasibility doc "P2.4b".

    P2.3 — 1+1 lossy redundancy (the FEC pivot), 2026-06-27, `listener.rs`. Real
    Reed-Solomon FEC is structurally unavailable (a real-Windows capture proved
    modern mstsc negotiates RDPUDP2, which has no FEC — see feasibility doc "P2.3 FEC
    capture RESULT"). The protocol-safe stand-in: behind the experimental env
    `MACRDP_UDP_LOSSY_AUDIO_DUP` (read once at `run_recv_loop` top, value-aware via
    `env_truthy`; default OFF), a **lossy** peer is built with the new `RdpeudpState`
    `Config { duplicate_lossy_sends: true }` (ironrdp-rdpeudp P2.3), so each source
    datagram it sends ships twice (same seq, byte-identical) → an independent-loss
    link of rate `p` drops a payload only at `p²`. Scoped to the lossy flow
    (`use_lossy && lossy_dup`); the reliable flow and the no-env default are
    byte-unchanged. mstsc's DTLS anti-replay drops the duplicate, so audio never
    double-plays (dedup lives above the transport — the lossy SM intentionally does
    not dedup). This is the second soak A/B axis (dup vs no-dup at a fixed loss);
    `scripts/soak-lossy-audio.sh` exposes it. Verification on a real lossy link is
    pending (the spike is built + unit-tested; the soak run is the user's call).

    Ack-driven IDR recovery support (2026-06-27): `RdpServer` gained
    `egfx_on_lossy_handle: Option<Arc<AtomicBool>>` + setter
    `set_egfx_on_lossy_handle` (mirrors the `udp_tunnel_bound` / handle-setter
    pattern). At the EGFX Soft-Sync site, when EGFX is migrated onto the **lossy**
    tunnel (`TUNNELTYPE_UDPFECL`, non-empty channel list) the flag is flipped true
    so macrdp's H.264 pipeline can arm ack-driven IDR recovery (the codec-side
    detection lives in `src/h264.rs`; the server only publishes the on-lossy state).
    On the reliable tunnel / TCP the flag stays false. All `multitransport`-gated;
    feature-off and the no-migration path are byte-unchanged. See the feasibility
    doc "Ack-driven IDR recovery (EGFX-on-lossy video)".

    EGFX-migration promoted from env to a flag (2026-06-28): `RdpServer` gained a
    `migrate_egfx: bool` field (default false) + setter `set_migrate_egfx`, mirroring
    the handle-setter pattern. At the EGFX Soft-Sync site the gate is now
    `self.migrate_egfx || migrate_egfx_enabled()` — i.e. the macrdp `--udp-migrate-egfx`
    flag OR the legacy `MACRDP_UDP_MIGRATE_EGFX` env var (the env fallback is kept so
    the `MACRDP_UDP_MIGRATE_EGFX_LOSSY` isolation test, which still reads the env,
    keeps working). The reliable-tunnel EGFX path is now reachable with just the two
    CLI switches (`--enable-udp-multitransport --udp-migrate-egfx`); no env needed. It
    stays a clean-link feature (reliable ordered stream HOL-blocks under loss — the
    under-loss freeze is the documented structural limit, soak finding #4). All
    `multitransport`-gated; feature-off byte-unchanged.

    M3c idle-timeout peer GC (2026-06-28, `listener.rs`). The listener's
    `peers: HashMap<SocketAddr, Peer>` was inserted-but-never-removed ("GC + idle
    timeout come with M3c" — never built), so a client whose RDP/TCP session went away
    kept its peer entry forever and `pump_peers_on_timer` kept RTO-retransmitting
    unacked EGFX to it. Reproduced from a real mstsc pcap (EGFX-over-UDP reconnect):
    after the client's TCP RST the server shipped UDP retransmits to the gone client
    for the rest of the capture (~10s/32 pkts and still going), and recovery needed a
    server restart — the user's "the UDP connection doesn't close on disconnect"
    report. Fix: `Peer` gained `last_seen_ms`, bumped on **every inbound datagram**
    (only inbound — a dead peer still *sends* outbound retransmits but receives
    nothing, so its clock stops); a new `gc_idle_peers(peers, bound_addrs, now_ms)`
    runs on the existing `retransmit_tick` (right after `pump_peers_on_timer`) and
    evicts any peer idle > `PEER_IDLE_TIMEOUT_MS`, also dropping its
    `bound_addrs` cookie→addr mapping (`retain(|_, a| *a != gone)`). Activity-based, so
    it covers graceful / abrupt / crashed disconnects uniformly.
    **TIMEOUT CORRECTED 2026-06-29: 10s → 60s.** The original 10s rested on a wrong
    assumption — that a live client *always* sends frequent RDPEUDP keepalive/delayed
    ACKs (~200/s). That holds only while the picture is **active**. When the screen goes
    idle, **mstsc drops to a ~15s UDP keepalive cadence** (verified live: inbound
    datagrams exactly 15s apart once frame-acks stop). The 10s GC then reaped the peer
    **between** two keepalives — killing the UDP tunnel of a fully live TCP session
    (audio still flowing), so EGFX froze **permanently** (the client's next keepalive
    isn't a SYN, so the peer is never recreated → needs reconnect). This was a distinct
    bug from the load-freeze (#89): triggered by going idle ~15–60s, on mirror-primary
    too. Fix: 60s = 4× the observed 15s keepalive, so an idle-but-live peer is never
    reaped; a genuinely dead peer still ages out (the "UDP retransmits after disconnect"
    leak becomes ≤60s instead of indefinite). **Verified on real mstsc**: a live peer
    idle ~45s recovered on activity with no eviction, while the *abandoned* peer from a
    prior reconnect was correctly reaped at idle_ms≈60042. The fully robust fix is an
    explicit TCP-session-close → evict signal (deferred half of M3c); until then the
    activity-based backstop must stay generous. This is the listener-only backstop half
    of M3c; the prompt server→listener "instant retire-on-disconnect" signal is still
    deferred (the GC is needed regardless). Logs `evicted idle UDP peer` at `debug`
    (`ironrdp_server::multitransport=debug`). Does NOT by itself fix the mstsc EGFX
    reconnect-blank (a client-side surface-retention quirk + the clean-link limit may
    compound it); the robust WiFi config remains `--udp-migrate-egfx` off (EGFX on TCP).
    All `multitransport`-gated; feature-off byte-unchanged. See feasibility doc
    "M3c peer GC".

    M3c reconnect state-reset — EGFX-over-UDP went blank/black on the 2nd
    connection (2026-06-28, `server.rs` + `listener.rs`; verified on real mstsc).
    With `--udp-migrate-egfx`, the FIRST connection rendered but a RECONNECT showed a
    blank desktop that went black (EGFX wedged after a frame or two); plain-TCP EGFX
    reconnect was always fine, so it was UDP-specific. TWO per-connection-state bugs on
    the persistent server+listener, both "set once on connection 1, never reset for
    connection 2":
    (a) **Server (`server.rs`, the universal cause):** `egfx_on_udp` (set true at
    Soft-Sync, checked to route EGFX over UDP) — plus `lossy_audio_block_no`,
    `lossy_audio_streaming`, and the `egfx_on_lossy_handle` flag — were never cleared
    between connections. So connection 2 started with `egfx_on_udp == true` and routed
    EGFX over a UDP tunnel **its own** Soft-Sync hadn't bound yet (cookie unbound) →
    frames dropped, and nothing went out on TCP either → blank/black. Fix: reset all of
    these right after `self.static_channels = StaticChannelSet::new()` in the `run()`
    accept loop (the post-connection cleanup). Now connection 2 keeps EGFX on TCP until
    its tunnel binds and re-fires Soft-Sync (clean migration; and a correct TCP fallback
    if the new tunnel never binds). (`multitransport_migration` / `udp_tunnel_bound` /
    the inbound rx are already refreshed per connection at the offer site, so only these
    only-set-never-reset flags needed clearing.)
    (b) **Listener (`listener.rs`, the same-port case):** on a fast reconnect that
    reused the client's UDP source addr/port (within the 10s idle-GC window), the
    `peers.entry(addr).or_insert_with` reused the **stale** established `Peer` —
    `tunnel_created` still true and `inbound_sink` still pointing at the gone
    connection's receiver — so `handle_emt_tunnel` skipped the new CREATEREQUEST
    (gated on `!tunnel_created`) and silently dropped connection 2's inbound EGFX acks.
    Fix: before the entry, if a **SYN** arrives on an address whose existing peer is
    already `is_established()`, it's a new flow on a reused port → remove the stale peer
    (+ its `bound_addrs` cookie bindings) so a fresh one is built and the new tunnel
    binds cleanly. A SYN on a still-handshaking peer is a normal SYN retransmit
    (`is_established()` gates it out). **Verified on real mstsc** — multi-cycle reconnect
    now renders and stays responsive. **Residual (the EGFX-over-UDP freeze under load /
    on reconnect) — FIXED 2026-06-28 in `macrdp` (`src/h264.rs`), not this crate:** the
    EGFX path never throttled on the client's `queueDepth`, so under a high-volume stream
    (or a backed-up reconnect) the client's frame queue ran away (peak ~352k) → frozen
    display + RDPEUDP ACK storm (input still reached the Mac over TCP, so it only *looked*
    dead). The fix is a frame-ack-lag backpressure gate in `submit_bgra` **with a trickle
    floor** — dropping most captures when the client is behind but never to zero, because
    mstsc only presents/acks an H.264 frame once trailing frames arrive (dropping to zero
    latches the freeze permanently). Verified on real mstsc under the headless
    `--capture-primary` + held-Cmd+Tab repro. The earlier "mstsc surface-retention quirk"
    guess was disproven (it reproduced on a fresh mstsc process); and the `egfx-ship`
    thread parked in `_dispatch_semaphore_wait_slow` during the freeze is a red herring —
    that's just Rust's `std::sync::mpsc::recv` idling on macOS. A fuller continuous
    rate controller remains finding-#5 future work. All `multitransport`-gated; feature-off
    byte-unchanged. See feasibility doc "M3c reconnect state-reset" + the "Residual …
    rate-control gap" / trickle-floor note.
    **EGFX-over-UDP → TCP watchdog (added 2026-06-29):** the reliable (UdpFecR)
    tunnel is ordered, so under loss it head-of-line-blocks like TCP (finding #4) —
    once the client stops acking while the server is still shipping, the tunnel is
    wedged and queued frames never arrive → permanent freeze until reconnect. The
    H.264 pipeline (`src/h264.rs::should_demigrate_to_tcp` / `submit_bgra`) detects
    that wedge (reliable UDP only, `egfx_on_udp && !egfx_on_lossy`, acks silent
    `MACRDP_UDP_EGFX_WATCHDOG_MS`≈3s while actively shipping, not suspended) and sets
    a shared `demigrate_request: Arc<AtomicBool>` (setter
    `set_demigrate_request_handle`, wired in `main.rs` alongside the egfx_on_udp
    handle). The `ServerEvent::Egfx` route arm reads it: on true it flips
    `egfx_on_udp` false (+ mirrors the egfx_on_udp handle so the H.264 #89 gate
    releases), logs, and falls through to the TCP DRDYNVC path — the existing
    `soft_sync_sent` guard stops any re-migration. mstsc renders EGFX on TCP after a
    Soft-Sync (proven by the throwaway timed "Spike A", verified live 2026-06-29).
    One-way per connection (h264 `demigrated` latch + the server resets
    `demigrate_request` false on reconnect alongside `egfx_on_udp`). Default-on but a
    strict no-op unless EGFX is on the reliable UDP tunnel; even a false positive only
    routes EGFX to TCP (the proven everyday path). **Verified on real mstsc under
    clumsy UDP-only loss** (UDP-only so TCP stays a healthy fallback): the wedge fired
    at `since_ack_ms≈7.7s` (real wedges dribble acks before going fully silent, vs.
    the deterministic injection's clean ~3s) and EGFX recovered on TCP — no permanent
    freeze. A future ack-lag-pegged secondary trigger could shorten recovery.
    **Adaptive-bitrate loss signal (added 2026-06-29):** the UDP listener
    (`multitransport/listener.rs`) accumulates reliable-tunnel **retransmits** into a
    shared `Arc<AtomicU64>` so macrdp's H.264 controller can do congestion-responsive
    bitrate (AIMD). Threaded as a new last param through
    `UdpMultitransportListener::bind` → `run_recv_loop` alongside the existing tls/dtls
    Arcs (NOT in `ListenerConfig`, which stays `Copy`), bumped at all three `sm.step()`
    retransmit sites via a `bump_loss` closure + `pump_peers_on_timer`'s return value.
    `None` = not wired. See `src/h264.rs::adaptive_bitrate_step` + feasibility doc
    "rate control P1". **Watchdog follow-up (TODO):** under sustained loss mstsc resets
    ~60s after a de-migration (its multitransport dead-tunnel timeout on the now-silent
    UDP tunnel) — keepalive or cleanly close the abandoned tunnel on de-migrate.

(13) Server Auto-Reconnect Cookie (MS-RDPBCGR ARC_SC_PRIVATE_PACKET) — NOT
    upstreamed; added 2026-07-02. `RdpServer` gains `auto_reconnect_cookie:
    Option<rdp::session_info::ServerAutoReconnect>` (default None) + a
    per-TCP-connection `auto_reconnect_sent: bool` guard, and a setter
    `set_auto_reconnect_cookie(logon_id: u32, random_bits: [u8;16])`. When set,
    `client_accepted` sends a Save Session Info PDU
    (`ShareDataPdu::SaveSessionInfo` → `InfoData::LogonExtended` with
    `LogonExFlags::AUTO_RECONNECT_COOKIE` + the `ServerAutoReconnect`) via the
    existing `encode_share_data_pdu` on the IO channel, once, right after
    activation completes (Confirm Active processed + encoder built, before
    `client_loop`); the guard is reset at the top of `run_connection` so a
    deactivation-reactivation resize doesn't re-send it. All the PDU types
    already exist in `ironrdp-pdu` (`rdp::session_info`) — only the server-side
    send is new. **Why:** without this cookie a client (mstsc) does NOT
    auto-reconnect on an ungraceful drop — it just reports disconnected. macrdp
    provisions it (`main.rs`, default on, `MACRDP_AUTO_RECONNECT=0` disables) so
    the EGFX blank-recovery connection drop (`src/h264.rs`
    `perform_blank_drop` → `ServerEvent::Quit`) heals with a seamless
    client-driven auto-reconnect instead of a manual one. The returning ARC_CS
    cookie is intentionally NOT validated (macrdp is single-console-session and
    re-auths via NLA every connection), so this only *enables the client's*
    auto-reconnect loop; a fixed per-process `logon_id`/`random_bits` is fine.
    Additive + standard RDP server behavior → cleanly upstreamable (a real RDP
    server always sends this). **MERGED 2026-07-31 as PR #1405**
    (`feat(server): send the Server Auto-Reconnect Cookie during logon`, by
    mamoreau-devolutions). mamoreau's review asked for returning-cookie
    validation + rotation; those were accepted as a **deferred follow-up** (the
    phased split — send-side merged in #1405) and are **now DONE upstream: PR
    #1509 (`validate auto-reconnect cookies`) MERGED 2026-08-02, auto-closing
    issue #1508 as completed** — it carried macrdp's independent HMAC-MD5
    known-answer test + the `ServerAutoReconnect` re-export + our live-mstsc
    validation, and glamberson's two-cookie rotation grace window also landed.
    macrdp's vendored send-only form is unaffected (single-console-session +
    NLA re-auth doesn't need the returning-cookie validation), so the pin bump
    can adopt upstream's full send+validate+rotate API and drop this divergence. The upstream port keeps the same send point + per-connection
    guard but shapes the API like `credential_validator` — a builder method
    `with_auto_reconnect_cookie(Option<ServerAutoReconnect>)` + a runtime setter
    `set_auto_reconnect_cookie(Option<..>)` (vs this vendored `(logon_id,
    random_bits)` setter); when the pin bumps past its merge+release, macrdp can
    adopt the upstream API and drop this divergence (the acceptor/rdpdr/dvc/
    rdpeudp forks stay for their own divergences). Verified: build/clippy/test
    clean. **Cookie behavior IS live-verified** — the blank-recovery drop test
    (2026-07-02) auto-reconnected mstsc on its own, which only happens if the
    cookie was provisioned during logon (see the h264 reconnect-blank quirk note).

(14) GfxDvcBridge channel-level decline flag (NOT upstreamed; added
    2026-07-04). `GfxDvcBridge` gains an optional shared
    `decline_output: Option<Arc<AtomicBool>>` (constructor
    `with_decline_flag`; `new` keeps `None` = upstream behavior). When the
    flag is true after the inner `GraphicsPipelineServer::process` returns
    (the application's `GraphicsPipelineHandler` runs synchronously inside
    it, so the flag is current for the PDU just handled), `process` discards
    the output instead of shipping it — crucially the `CapabilitiesConfirm`.
    WHY: upstream's `handle_capabilities_advertise` unconditionally queues a
    CapabilitiesConfirm — there is no decline path — so a server that can't
    actually drive the pipeline for this client (macrdp: client advertised
    EGFX with AVC_DISABLED on every capset; macrdp only implements AVC420
    over EGFX) used to confirm the pipeline and then send legacy bitmap
    updates anyway. Windows App for ANDROID does exactly this advertise and
    treats confirmed-pipeline+legacy-updates as a protocol error —
    hard-disconnect ~2 s after activation (observed live 2026-07-04). With
    the decline, the client is never told the pipeline came up (an
    unconfirmed pipeline is the normal "not yet active" state every client
    renders legacy in) and stays on legacy BitmapUpdate. macrdp wires the
    flag per connection in h264.rs `build_server_with_handle` and sets it in
    `on_ready` when `client_supports_avc` is false. VERIFIED locally both
    ways with sdl-freerdp: `/gfx:progressive` (advertises the same
    AVC_DISABLED signature as Android) → declined, stays connected on legacy,
    zero protocol errors; plain (AVC420) → H.264 fully active, unaffected.
    Upstreamable as-is (additive), or better as a real decline hook on
    `GraphicsPipelineHandler` (e.g. `fn accept_pipeline(&caps) -> bool`
    consulted before queueing the confirm) — offer alongside the egfx work.

    Tunnel-death detection + multitransport-offer cooldown (2026-07-04, extends
    divergence (12)): the fix for mstsc's ~60 s dead-tunnel session reset
    (two live repros: the 2026-06-29 watchdog de-migrate follow-up, and the
    2026-07-04 ZeroTier lossy-audio cycle — session up ~60 s → reset →
    reconnect → blank → recovery → repeat). Keepalives were evaluated and
    REJECTED: in the overlay-network case the UDP path itself is dead, so
    server keepalives can't reach the client either. What the server CAN fix:
    (a) `CookieRegistry::take` now also returns the tunnel-bound flag, the
    listener keeps it on the `Peer` (`bound_flag`), and `check_dead_tunnels`
    (on the retransmit tick) declares a BOUND peer dead after
    `MACRDP_UDP_TUNNEL_DEAD_SECS` (default 30 s ≈ two missed mstsc idle
    keepalives; must stay < the 60 s idle GC; 0 disables) of inbound silence —
    flipping the flag false so the server's per-wave `lossy_audio_target`
    check fails and audio falls back to the static TCP channel immediately
    (EGFX has its own ack-silence watchdog); (b) the same event starts a
    multitransport-offer COOLDOWN (`CookieRegistry::{suppress_multitransport,
    multitransport_suppressed}`, shared state inside the registry both ends
    already hold — zero new wiring; `MACRDP_UDP_MT_COOLDOWN_SECS`, default
    600, 0 disables), and `run_connection` skips the offer while suppressed —
    so the client's dead-tunnel reset reconnects as a stable plain-TCP session
    instead of re-establishing a doomed tunnel and cycling. Registry semantics
    unit-tested in macrdp's `src/multitransport.rs` (the vendored crate is
    test = false). Live verification of the listener path needs a real mstsc
    with a bound tunnel + UDP-only blockage — pending.

(15) Kernel TCP RTT sampled at accept → shared cell (NOT upstreamed; added
    2026-07-05). `RdpServer` gains `link_rtt_ms: Option<Arc<AtomicU32>>`
    (default None) + setter `set_link_rtt_handle`, mirroring the
    keyboard-layout cell (divergence 10). In the `run()` accept loop — the
    ONLY point the concrete `TcpStream` (hence the raw fd) is reachable;
    `run_connection` takes a generic stream and wraps it immediately — the
    server reads the kernel's smoothed TCP RTT via
    `getsockopt(TCP_CONNECTION_INFO)` (`tcp_srtt_ms`, macOS-only with a
    None-returning stub elsewhere so Linux CI compiles; new
    `[target.'cfg(target_os = "macos")'.dependencies] libc` in Cargo.toml)
    and stores it (ms; 0 = unknown, sub-ms LAN maps to 1) into the cell
    before `run_connection`. The kernel seeds srtt from the SYN/SYN-ACK
    exchange so a meaningful value exists immediately at accept. macrdp's
    h264.rs samples the cell per connection to drive link-aware behavior:
    the blank-recovery RTT gate (evidence window scales with RTT; the drop
    lever is withheld past MACRDP_BLANK_RECOVERY_MAX_RTT_MS — the ZeroTier
    false-positive fix, see docs/known-quirks.md) and the adaptive-bitrate
    seed (ceiling/3 start past MACRDP_ADAPTIVE_SEED_RTT_MS). Additive +
    handle-setter pattern → upstreamable, though the libc dep and the
    macOS-only sample make it less obviously general than (10)/(13).

    RTT-gated multitransport offer (2026-07-05, extends divergences 12+15): the
    offer site in `run_connection` now also withholds the offer when the
    connection's accept-time kernel TCP RTT (the divergence-15 cell) is at or
    above `MACRDP_UDP_OFFER_MAX_RTT_MS` (default 80; 0 disables) — an
    overlay-class link (VPN/ZeroTier/mobile) runs plain TCP from the first byte
    with no tunnel to wedge, making the UDP switches safe to leave enabled on a
    roaming client. Composes with the tunnel-death cooldown (predictable case
    avoided up front; post-connect degradation still caught reactively). Log
    marker: "multitransport offer WITHHELD (link RTT above the offer gate)".
    Verified live: threshold-1 loopback trips the gate (link_rtt_ms=1), default
    80 leaves loopback offering normally.
    **No-offer state reset (2026-07-06, found by the #136 double-check):** when
    the offer is skipped (cooldown OR RTT gate), `run_connection` now explicitly
    clears `multitransport_migration` / `udp_tunnel_bound` / the inbound rx and
    evicts the previous cookie from the registry. Those fields were otherwise
    only refreshed AT the offer site, so a skipped offer inherited the PREVIOUS
    connection's state — including a bound-tunnel flag that stays true for up to
    ~30 s after that session ends (until tunnel-death lowers it), enough for the
    new connection to route lossy audio into a dead tunnel or fire a bogus
    Soft-Sync. The cooldown path was safe only by accident (suppression follows
    a tunnel-death event that already lowered the flag); the RTT gate made the
    window genuinely reachable (e.g. a WiFi session with a bound tunnel followed
    within seconds by an auto-reconnect over a high-latency link).

    Final-check hardening (2026-07-06, from the post-#136 regression sweep —
    one live finding + two review findings): (a) **abandoned-tunnel false
    cooldown (LIVE, the big one):** every ended session's tunnel went
    inbound-silent and 30 s later `check_dead_tunnels` declared it DEAD +
    started the 10-min offer cooldown — downgrading subsequent healthy-LAN
    connections to plain TCP (observed twice in the deploy-night log after a
    blank-recovery drop). Fix: the post-connection reset now RETIRES the
    tunnel (lowers the shared `udp_tunnel_bound` flag), and the death check
    treats an already-lowered flag as benign teardown (debug "retiring
    quietly", no cooldown; peer ages out via the 60 s GC). A tunnel that
    wedges while its session is ALIVE still fires death + cooldown unchanged.
    (b) **GC flag strand (review):** `gc_idle_peers` now lowers a surviving
    `bound_flag` on eviction — otherwise `MACRDP_UDP_TUNNEL_DEAD_SECS >= 60`
    (the GC timeout) let eviction win the race and strand the server-side
    flag true forever (no cooldown ever + lossy audio routed into a
    nonexistent tunnel with no TCP fallback). (c) **fork-workers RTT gap
    (review):** `tcp_srtt_ms` is now `pub` (re-exported) and macrdp's worker
    branch samples the inherited socket into the shared cell before
    `run_connection` — the accept-loop sample site never runs in a worker, so
    the link-aware features (blank gate / seed / offer gate) silently treated
    every worker connection as LAN, re-arming the #135 false positive for
    FORK_WORKERS-over-VPN deployments. Also fixed the interleaved
    check_dead_tunnels/gc_idle_peers doc blocks (cosmetic).

    Tunnel-state lifecycle fixes (2026-07-06, from the second sweep's
    concurrency audit — extends the #138 hardening): (F1) the offer-site
    cookie registration leaked one registry entry per connection that died
    before activation (mstsc cert-prompt broken pipe, CredSSP failures,
    probes) because eviction was keyed off MigrationState, set only in
    client_accepted. New `current_offer_cookie` field, set at the offer site
    and evicted in the post-connection reset (runs on every run_connection
    return path) — which also closes (F3): a client's LATE tunnel bind (UDP
    handshake outliving a fast session end) could consume the stale cookie,
    re-raise the retired bound flag, and produce a zombie peer whose eventual
    "death" started a spurious 10-min cooldown. (F2) the SYN stale-peer
    replacement is the THIRD Peer-removal site and now also lowers a
    surviving bound flag (mid-session re-SYN from a reused port stranded the
    server flag true = permanent audio silence — same class as the #138 GC
    fix). (F4) check_dead_tunnels now adjudicates retired flags AT the first
    tick the lowering is observed: already-silent-past-threshold at
    retirement = the wedge predated the session end → death + cooldown still
    fire (an early session end no longer launders a real wedge into "benign
    teardown", which could have resurrected the reset cycle cooldown-free on
    sub-80ms flapping links); recently-active at retirement = quiet retire.
    A wedge younger than the threshold at session end intentionally reads as
    teardown (inherently ambiguous). (F5) MACRDP_UDP_TUNNEL_DEAD_SECS is
    clamped below the 60 s idle GC with a warn — at/above it the GC won the
    race and the cooldown protection silently never engaged. Audit also
    confirmed clean: per-offer flag Arc identity (no cross-connection
    retirement), CookieRegistry lock ordering/poisoning, inbound-sink
    lifetime, Relaxed ordering adequacy.

(16) Server-direction MS-RDPEUSB (USB redirection, the `URBDRC` DVC) — NOT
    upstreamed; added 2026-07-06; behind macrdp's `--enable-usb-redirection`
    (opt-in). New `src/rdpeusb.rs` drives the server side of MS-RDPEUSB against
    the pinned PDU-only `ironrdp-rdpeusb` crate (added as a **git dep**, not a
    `[patch.crates-io]` since it's `publish = false`) — we write the processor
    ourselves rather than adopt the upstream `Urbdrc*Server` (which is ~3 PRs
    ahead and needs a breaking IronRDP pin bump), the same pattern as the
    server-direction RDPDR (divergence 11).
    - `UrbdrcServer` (main channel `DvcProcessor`+`DvcServerProcessor`): drives the
      MS-RDPEUSB init handshake — RIM capability exchange → `CHANNEL_CREATED`
      (Direction::ToClient) → `RIMCALL_RELEASE`. Each of those server→client
      steps is required; the caps exchange alone leaves the client's device
      registered but un-announced (found live with FreeRDP-with-urbdrc).
      `RIMCALL_RELEASE` has no dedicated server PDU in the pinned crate, so it's a
      bare `SharedMsgHeader` (NOTIFY_CLIENT / StreamIdProxy / FunctionId
      RIMCALL_RELEASE) via a local `UsbHeaderMsg` `DvcEncode` wrapper (the
      server→client `UrbdrcServerPdu` rides a `UsbDvcPdu` wrapper, à la
      `OwnedAudioPdu`). On the client's `ADD_VIRTUAL_CHANNEL` (device announce),
      the processor can't open a DVC itself, so it signals the event loop via the
      new **`ServerEvent::Urbdrc(UrbdrcServerMessage::OpenDeviceChannel)`**.
    - Dispatch arm in `client_loop` (mirrors the `ServerEvent::Rdpdr`/`Egfx` arms):
      `get_svc_processor::<DrdynvcServer>()` → `create_channel(UrbdrcDeviceProcessor)`
      → `server_encode_svc_messages` the resulting CreateRequest. This is the only
      place a per-device DVC can be opened (only the loop holds `&mut DrdynvcServer`).
    - `UrbdrcDeviceProcessor` (per-device channel): on open (`start`) runs the SAME
      per-channel handshake as the main channel — **capability exchange →
      `CHANNEL_CREATED` → `RIMCALL_RELEASE`** (the `INIT_CHANNEL_OUT` barrier) — so
      the client sends `ADD_DEVICE` with the real descriptors, which `process`
      decodes/logs. **The full handshake is REQUIRED by mstsc** (2026-07-07): FreeRDP
      is fine with the per-device channel jumping straight to `RIMCALL_RELEASE` (its
      readiness state is global to the main channel), but mstsc CLOSES a per-device
      channel that receives `RIMCALL_RELEASE`/`CHANNEL_CREATED` with no preceding
      capability exchange — it keeps a silent channel open and waits for the caps
      request. Diagnosed with a silence-vs-message A/B; with the full handshake mstsc
      completes it and sends `ADD_DEVICE`. (This was the blocker that made mstsc USB
      redirection never announce a device; FreeRDP masked it — same lesson as the
      RDPDR handshake-ordering divergence.) `start`/`process` carry a `next_msg_id`
      for the caps/CHANNEL_CREATED/RIMCALL_RELEASE MessageIds. Pairs with the
      `ironrdp-rdpeusb` divergences (1) `UsbDevice=0` + (2) interface-0 completion
      routing — all three needed for an mstsc device to enumerate.
    - **Phase 3.1b(2) transfer path (async `UsbHandle`/`UsbRouter`):** transfers ride
      an async seam modeled on `rdpdr::RdpdrHandle`/`IoRouter`. `UsbRouter` (shared
      `Arc` inner: `AtomicU32` req id masked to 31 bits + `Mutex<HashMap<id,
      oneshot>>`) correlates a request with its `URB_COMPLETION` by the TS_URB
      `RequestId` the completion echoes. `UsbHandle { sender, router, channel_id,
      device_iface }` (clone it to drive transfers from anywhere) exposes
      `get_descriptor()`/`device_descriptor()`: register a waiter → ship the request
      via `ServerEvent::Urbdrc(SendMessages { channel_id, messages })` → the loop
      DVC-frames it onto the device channel (`encode_dvc_messages` → mirrors the Echo
      arm) → `await` the completion. `UrbdrcDeviceProcessor::process()` is thin —
      decode + route: log `ADD_DEVICE`, hand `URB_COMPLETION`s to `router.deliver`,
      tolerate the rest. `DeviceDescriptor::parse` keeps the USB byte-layout in one
      typed place (no inline offsets). The DRIVER (what to do with a device) is NOT
      in the vendored crate — `UrbdrcServerFactory::device_callback() ->
      Option<UsbDeviceCallback>` (an `Arc<dyn Fn(UsbHandle) + Send + Sync>`) is the
      seam: the device processor calls it once per `ADD_DEVICE` with the device's
      handle, and macrdp's `MacUsb` (the presenting side) does the work
      (`src/usb_redirect/mod.rs::drive_device` — fetch the descriptor now, drive the
      UserHCI controller next). **VERIFIED live** with a USB-3.2 flash drive over
      FreeRDP-with-urbdrc: macrdp's driver fetched `vid=0x2174 pid=0x2100
      usb_version=0x0320` (the drive's real data, read from the device) through the
      handle. macOS libusb kernel-detach for a mass-storage device was not a blocker
      after `diskutil unmountDisk`.
    - **Device lifetime → presenting-side teardown.** `UrbdrcDeviceProcessor` holds a
      `watch::Sender<bool>` and hands each `UsbHandle` a subscriber; `UsbHandle::closed()`
      awaits it. The server resets `static_channels` right after the connection loop
      returns (`server.rs:1244`), so the per-device processor drops on disconnect → the
      sender drops → every handle's `closed()` resolves. macrdp's `present_device` selects
      `closed()` against its request channel and destroys the UserHCI controller when it
      fires — so a controller no longer outlives its connection (VERIFIED: the presented
      device disappears from `ioreg` on disconnect while the server stays up). `close()` on
      the DVC processor is never called by the server (same gap as EGFX `on_close`), so this
      leans on `Drop` via that `static_channels` reset, which is prompt.
    - **Phase 3.2 SelectConfiguration + typed URB results (2026-07-06).** The router
      now delivers a `UrbReply { output_buffer, urb_result, hresult }` instead of a
      bare `Vec<u8>` — a `URB_COMPLETION` carries both the transferred bytes AND the
      TS_URB result payload (decoded as `TsUrbResultPayload::Raw` by default; a typed
      request re-decodes it). `UsbHandle::select_configuration(config_bytes)` parses
      the full config descriptor (`parse_configuration` → `UsbConfigDesc` +
      per-interface `TsUsbdInterfaceInfo`/`TsUsbdPipeInfo`), sends `TsUrb::SelectConfig`
      via `TransferInRequest`, and decodes `TsUrbSelectConfigResult` into `UsbPipe
      { endpoint_address, pipe_handle, is_bulk }` — the client `pipe_handle`s are the
      prerequisite for any bulk transfer. **VERIFIED the URB is correct** (FreeRDP
      receives + parses it as `TS_URB_SELECT_CONFIGURATION` and attempts it) but it
      can't COMPLETE on a macOS-client loopback for a mass-storage device: macOS
      libusb can't detach the mass-storage kernel driver to claim the interface
      (`LIBUSB_ERROR_ACCESS`), which every bulk transfer needs — so the loopback
      proves enumeration + encoding, and the mount needs a claimable-interface client
      (real Windows / Linux FreeRDP). macrdp bounds it with a 5 s timeout (degrade to
      enumerate-only, no hang).
    - **Phase 3.2 bulk transfer forwarding — the redirected drive MOUNTS (2026-07-06).**
      `UsbHandle::bulk_transfer_in(pipe_handle, length)` / `bulk_transfer_out(pipe_handle,
      data)` build a `TS_URB_BULK_OR_INTERRUPT_TRANSFER` (via the shared
      `bulk_transfer_request` helper: `RegisterRequestCallback` + `TransferInRequest`
      with `output_buffer_size` for IN / `TransferOutRequest` with `output_buffer` for
      OUT; the `USBD_TRANSFER_DIRECTION_IN` flag MUST match the request PDU or the codec
      rejects it) on a client `pipe_handle` from SelectConfiguration. macrdp's driver loop
      forwards each kernel-raised bulk transfer on the mass-storage endpoints (0x01 OUT /
      0x82 IN) so the macOS driver's SCSI (CBW → data → CSW) rides the client's real drive.
      **VERIFIED end-to-end on a real Linux FreeRDP client** (UTM-QEMU Ubuntu + USB-2.0 hub
      for a claimable interface): the ESD310C flash drive mounts on the Mac and stays
      mounted, 1300+ steady bulk transfers, no resets/timeouts.
    - **Phase 3.2 control-OUT forwarding (2026-07-06).** `UsbHandle::control_transfer_out(setup,
      data)` forwards an EP0 host→device request to the real device via a generic
      `URB_FUNCTION_CONTROL_TRANSFER_EX` (`TsUrb::CtlTransferEx`) on pipe handle 0 (the default
      control endpoint — MS-RDPEUSB maps `EndpointAddress = PipeHandle & 0xff`), so a
      mass-storage Bulk-Only Reset / Clear-Feature(HALT) reaches the device for SCSI error
      recovery instead of being ACKed only on the macOS side. macrdp's Obj-C side forwards on the
      control STATUS stage and excludes the standard requests the host controller /
      SelectConfiguration own (SET_ADDRESS/CONFIGURATION/INTERFACE). Regression-verified live (clean
      path unaffected — SET_CONFIGURATION correctly stays a local ACK); the forward only fires under
      a device error/stall.
    - **Hardening pass (2026-07-07, all live-verified with the connect-while-mounted repro).**
      Five fixes closed real gaps found reviewing the bulk/control path:
      1. **Disconnect-race deadlock (serious).** Every `UsbHandle` transfer awaited a
         oneshot whose sender lives in the router map, kept alive by the handle's own
         `Arc` — so on disconnect the completion never arrives AND the sender never
         drops, and a bare `rx.await` pends forever, wedging the presenting driver
         mid-transfer (controller + dedup slot leaked until process exit). New
         `UsbHandle::await_reply` races every completion against `closed()` (`biased`,
         reply-first). ALL transfer methods route through it.
      2. **Generic control-IN forwarding.** New `UsbHandle::control_transfer_in(setup,
         max_len)` (generic `CONTROL_TRANSFER_EX`, IN direction, raw SETUP preserved);
         the Obj-C side now forwards ANY device→host EP0 data stage, and the Rust
         driver routes standard device-recipient `GET_DESCRIPTOR` (`0x80/0x06`) through
         the dedicated descriptor URB and everything else generically. Fixes mass-storage
         **Get Max LUN** (`0xa1/0xfe`, multi-LUN devices) — VERIFIED forwarded+answered
         live — and unblocks HID report-descriptor reads. `control_out_request` was
         generalized to `control_transfer_request(dir, …)` serving both directions;
         `get_descriptor` now honors `hresult` (stalls instead of returning 0 bytes,
         which made the kernel retry).
      3. **Per-endpoint transfer supersession (fixes a SIGBUS).** When a slow device
         (Get Max LUN on a flaky drive) missed the kernel's ~5 s EP0 timeout, the kernel
         re-issued the transfer slot; the late completion of the ORIGINAL then wrote
         through the retired ring `msg` → SIGBUS in the completion memcpy (the two prior
         guards — endpoint liveness + object identity — couldn't catch it: same endpoint,
         still Active, only the ring slot stale). `usb_spike.m` now tracks one outstanding
         transfer per endpoint (`pendingByEndpoint`) and invalidates the prior on a new
         raise / EndpointDestroy. VERIFIED: connect-while-mounted (the crash repro) now
         mounts and stays.
      4. **Client channel-close → presenting teardown (hot-unplug).** `UrbdrcDeviceProcessor`
         now implements `DvcProcessor::close` (newly *invoked* by the vendored
         `ironrdp-dvc` server — divergence 2 there): it flips the liveness `watch` so a
         client-initiated per-device-channel close (device unplugged/reset on the client)
         tears the controller down and releases the dedup slot, so a reset re-presents
         fresh instead of being skipped as a duplicate of a corpse. `closed()` switched
         from `changed()` to `wait_for(|v| *v)` so a transfer raised after the flip still
         sees it. **VERIFIED live 2026-07-07**: detaching the drive in UTM logged
         `per-device channel closed by the client (device unplugged/reset) → destroying
         UserHCI controller → dedup slot released`, and re-attaching re-presented + mounted.
    - Remaining: retract/hot-unplug via an explicit RETRACT_DEVICE PDU (the client
      channel-close path above covers detach/reset — the common case — live-verified),
      true multi-device (iSerialNumber), remaining non-mass-storage device classes
      (HID/gamepad verified 2026-07-08 on FreeRDP + mstsc; audio/others untested).
    - **mstsc client — ENUMERATES + CONFIGURES + negotiates format (2026-07-07); the
      only gap left is the client not delivering bulk video frames.** With the handshake
      fixes (per-device full handshake here + `ironrdp-rdpeusb` (1)/(2)) a real mstsc
      RemoteFX-USB device announces `ADD_DEVICE` and enumerates; four further fixes then
      carried it all the way to a streaming attempt:
      - **`SelectConfiguration` now succeeds** (was `0x80070057`). Two bugs, both
        mstsc-strict / FreeRDP-lenient: (a) `parse_configuration` emitted one
        interface-info entry per interface *descriptor* — including every alternate
        setting — producing DUPLICATE interface numbers, which real Windows rejects; it
        now emits exactly one entry per interface **number** at alt setting 0 (the default
        a freshly-configured device is in). (b) the URB carried only the 9-byte config
        descriptor header while `ConfigurationDescriptorIsValid` was set — fixed by
        `ironrdp-rdpeusb` (3) (full descriptor). Mass storage (one interface, no alts)
        never tripped either, which is why FreeRDP worked.
      - **Control transfers now succeed** (was `0x80070057` on every one). mstsc's URBDRC
        rejects the generic `URB_FUNCTION_CONTROL_TRANSFER_EX`; `setup_to_typed_urb` now
        maps each SETUP packet to the specific typed URB real Windows emits
        (`CLASS_INTERFACE`, `GET_DESCRIPTOR_FROM_INTERFACE`, `SET_FEATURE_TO_*`, …), with
        `CONTROL_TRANSFER_EX` kept as the fallback. 135+ control transfers succeed and the
        **UVC VS_PROBE/COMMIT format negotiation completes** end-to-end.
      - **`USBD_SHORT_TRANSFER_OK`** is now set on bulk/interrupt IN (a short read — a
        video payload, a HID report — is normal, not `USBD_STATUS_ERROR_SHORT_TRANSFER` →
        `0x8007001f`). Mass storage's exact-length SCSI reads never needed it.
      - **`RIMCALL_RELEASE` is recognized + ignored**: mstsc sends one per completed
        request (releasing the callback we registered) — decoded quietly instead of
        flooding the tolerated-decode-error log.
      After all four, a redirected **camera** enumerates, configures, and negotiates a
      video format on mstsc; macOS then issues continuous bulk reads on the video
      endpoint, but **mstsc never returns frame data** (of ~10 concurrent bulk reads it
      completes one with `0x8007001f` and leaves the rest pending forever). That's a
      **client/mstsc-side limitation** — for a webcam, Windows routes real video over the
      **dedicated camera-redirection channel** ("Video capture devices" in mstsc), a
      different protocol macrdp doesn't implement — not a server bug. So on mstsc: mass
      storage rides Drives/RDPDR (excluded from the RemoteFX USB list, can't be exercised
      here); a camera/HID/audio device fully enumerates + configures but doesn't stream.
      The four fixes above are all in the shared path and were regression-checked against
      FreeRDP mass storage (still mounts + read/write). True webcam support = implement the
      camera-redirection channel (a separate feature). isoch (camera/audio) endpoints
      remain unimplemented; **interrupt (HID) endpoints work** — a redirected Xbox
      gamepad is live + button-responsive on the Mac over BOTH FreeRDP and mstsc
      (2026-07-08; interrupt rides the same bulk-or-interrupt URB path, no new endpoint
      code — the only mstsc-specific fix was the SET_FEATURE/Guide-button routing above).
    - **One controller per physical device (dedup, presenting-side).** A client can
      announce ONE physical device on more than one `URBDRC` channel — FreeRDP announces
      the same drive twice with instance ids differing by a byte (`…d31`/`…d32`), plus a
      reset re-announces it — and presenting each spins up its own controller: two virtual
      drives then **duel over the single client device** (conflicting SCSI, 10 s timeouts,
      failed mount — observed live). So macrdp's `drive_device` dedups on the device's
      **stable hardware identity** (`VID:PID:bcdDevice` from the descriptor, fetched before
      claiming), NOT the client's per-announce `device_instance_id` (which varies). This is
      a **presenting-side policy**, not in the vendored crate: the server correctly opens
      the channel the client asked for; the presenting side decides not to present one
      device twice. (`UsbHandle` still exposes `device_instance_id` for logging.) Limitation:
      two different drives of the identical model+revision share a key — true multi-device
      needs the iSerialNumber string, deferred.
    - A per-connection `MAX_DEVICE_CHANNELS` (32) cap on `OpenDeviceChannel`
      requests bounds a client that spams `ADD_VIRTUAL_CHANNEL` (each opens a DVC
      that's never pruned within a connection) from growing the DRDYNVC slab.
    - **Robustness (load-bearing):** BOTH `process()` impls TOLERATE decode errors
      (log + `Ok(Vec::new())`, never propagate) — a decode error would otherwise
      propagate out of `svc.process()?` and tear down the whole RDP session for an
      opt-in feature (same lesson as the ironrdp-dvc Soft-Sync divergence). **The
      SEND path is encode-tolerant too (2026-07-08):** the
      `UrbdrcServerMessage::SendMessages` dispatch arm (server.rs) logs + drops a
      transfer whose URB fails to encode instead of `?`-propagating (which used to
      tear the session down) — the never-kill-the-session principle applied to the
      encode side. That backstops the real fix: `control_transfer_request` now
      routes `TS_URB_CONTROL_FEATURE_REQUEST` (SET/CLEAR_FEATURE) via `TRANSFER_IN`
      (the URB *function* carries the host→device direction; the MS-RDPEUSB codec
      only accepts feature requests in TRANSFER_IN, never TRANSFER_OUT). **Found
      live on mstsc:** pressing the Xbox controller's **Guide button** issues
      `SET_FEATURE(DEVICE_REMOTE_WAKEUP)`, which the old TRANSFER_OUT routing made
      `TsUrb::encode` reject → whole-session disconnect. Now it forwards and the
      session survives (verified live: an 81 s gamepad session through the Guide
      press vs the old ~6 s teardown). The
      device processor also recognizes `ADD_DEVICE` from its header
      (`peek_function_id`) so a body it can't parse still logs a GO. **Phase 3.1b
      (2026-07-06): `ironrdp-rdpeusb` is now VENDORED** (`vendor/ironrdp-rdpeusb`,
      leaf crate → one-sided path-dep + `ironrdp-str` pinned) with a lenient
      `UsbDeviceCaps` decode (USB 3.x `SupportedUsbVer` + `Other(u32)` fallbacks on
      the device-reported version/speed fields — see its CLAUDE.md divergence 1), so
      the tolerant `process()` is now the belt to that suspenders: a real USB-3.2
      flash drive's `ADD_DEVICE` **fully parses** (`usb_version=Usb32`), not just
      header-recognized. Still remaining for 3.1b: the `UsbHandle`/router async
      transfer path + client-sourced descriptors in `usb_spike.m`.
    - Wiring: `usb_factory: Option<Box<dyn UrbdrcServerFactory>>` field + `new`
      param + `set_sender` + `builder.with_usb_factory` + `.with_dynamic_channel`
      in `attach_channels` (advertised only when `Some` — byte-identical when off).
      macrdp's `MacUsb` factory captures the connection event sender (`set_sender`)
      and hands it to each `build_processor()` → `UrbdrcServer::with_sender`.
    **VERIFIED end-to-end** with a purpose-built FreeRDP-with-urbdrc client
    (`WITH_URBDRC=ON` + libusb) redirecting a real USB-3 flash drive: full handshake
    → per-device channel opened → `ADD_DEVICE` received (GO), session stays up
    (decode error tolerated). Off-path (`--enable-usb-redirection` absent) unchanged.

(17) Wheel-rotation two's-complement decode fix (macrdp issue #113; NOT
    upstreamed, real fix belongs in `ironrdp-pdu`). `From<MousePdu> for
    MouseEvent` in `handler.rs` re-decodes `number_of_wheel_rotation_units`
    as 9-bit two's complement (`WHEEL_NEGATIVE` is the sign bit); upstream's
    decode does sign-magnitude instead (`-(byte)`), so a byte meaning -1
    comes out as -255 — its own encode/decode don't round-trip. Recover:
    `if v < 0 { -v - 256 } else { v }`. Found via live trace on the macOS
    Windows App client, whose fine-grained ±1..±3 deltas exposed it; mstsc's
    whole ±120 notches masked it. Delete once ironrdp-pdu's decode is fixed
    upstream and the pin moves past it.

(18) ConnectionHandler auth-outcome hook (NOT upstreamed; added 2026-07-10).
    Adds `fn on_authenticated(&mut self, success: bool, reason: Option<&str>)`
    (default no-op) to the `ConnectionHandler` trait, and calls it in
    `run_connection` around the Hybrid/CredSSP `accept_credssp` — `Ok` →
    `on_authenticated(true, None)`, `Err(e)` → `on_authenticated(false,
    Some(&e.to_string()))` then propagate the error unchanged (`auth_result?`).
    macrdp is **always** Hybrid + always sets the static credential, so this
    runs on every connection and `Ok` = the client's NTLM response validated
    (unambiguous); `Err` = auth did not complete (dominated by bad
    credentials, but also a client abort or a rare post-TLS mid-CredSSP
    transport error), so the hook carries the error string as a `reason` for a
    SOC to distinguish logon-denied from a reset. The call fires exactly once
    per TCP connection (reactivation reuses the connection, no re-auth) and is
    ordered AFTER `mark_security_upgrade_as_done()`, so a pre-TLS nego blip
    (mstsc's cert-prompt broken pipe) can't false-fire a failure. The `pub_key`
    is cloned out of `self.opts.security` before the call so the
    `self.connection_handler.as_mut()` borrow is disjoint. macrdp's
    `AuthGuardHandler` implements it → an explicit `event="auth"` audit record
    on the SIEM JSON stream (`src/auth_guard.rs::audit_auth`), replacing the
    connection-duration inference for the login verdict. Additive +
    upstreamable (an auth-lifecycle hook is generally useful — metrics, audit,
    fail2ban); offer it upstream as an `on_authenticated` (or a
    `credential_validator`-shaped) hook. **Scope:** single-process only — under
    `--fork-workers` the verdict happens in a worker running with
    `connection_handler = None`, so the hook doesn't fire there (fork-workers
    keeps its exit-code-derived accept/disconnect audit); documented as a v1
    boundary.

(19) Server-direction MS-RDPECAM camera redirection — **COMPLETE; shipped in
    macrdp v0.9.0** (NOT upstreamed; added 2026-07-16 as a Phase-0 gate, finished
    2026-07-20; behind macrdp's `--enable-camera-redirection`, opt-in, default OFF).
    The client's webcam is received here and presented by macrdp as a real macOS
    camera. `src/rdcamera.rs` houses
    `RdCameraServer` (a `DvcProcessor`+`DvcServerProcessor` on the
    `RDCamera_Device_Enumerator` DVC) that advertises the MS-RDPECAM enumeration
    channel, answers the client's version negotiation, and LOGS the client's
    `DEVICE_ADDED_NOTIFICATION`. That log line is the go/no-go signal that a modern
    mstsc/Win11 will hand macrdp a client-redirected webcam over MS-RDPECAM — the
    channel the decrypted pcap proved the camera actually rides (NOT URBDRC/USB; see
    `docs/rdp-camera-redirection-feasibility.md` + the
    `project_camera_redirection_feasibility` memory).
    - **Beyond the gate (the shipped path):** on `DEVICE_ADDED_NOTIFICATION` the
      enumerator asks the event loop to open the client-named **per-device DVC** (via
      `ServerEvent::Camera` — the URBDRC per-device model), where
      `RdCameraDeviceProcessor` runs the state machine: `ActivateDevice` →
      `StreamList` → **media-type negotiation** (picks H.264 from the client's offered
      formats) → `StartStreams` → the `SampleRequest`↔`SampleResponse` **pull loop**
      (requests must be pipelined or the stream stalls). Decoded samples are handed
      out through the `CameraSampleSink` trait (`on_media_type` / `on_sample`), which
      macrdp implements in `src/camera/` (VideoToolbox decode → a CoreMediaIO Camera
      system extension). **LIVE-VERIFIED on real mstsc at 1080p/~30 fps.**
    - **Wire gotcha:** `StartStreamsRequest` carries the 27-byte `START_STREAM_INFO`
      (StreamIndex + the 26-byte packed-LE `MEDIA_TYPE_DESCRIPTION`) with **NO leading
      count byte** — the count is implicit in the PDU length. Emitting one gets
      `InvalidMessage` (0x02) from real Windows.
    - Handshake (MS-RDPECAM 3.1/3.2): every message starts with a 2-byte
      `SHARED_MSG_HEADER` = `Version(u8)` + `MessageId(u8)`. The CLIENT speaks first
      (`SelectVersionRequest` 0x03, its max version) once the server opens the DVC, so
      `start()` returns empty; `process()` decodes the header and, on 0x03, replies
      `SelectVersionResponse` (0x04) with `min(client, OUR_MAX=2).max(1)`; on
      `DEVICE_ADDED_NOTIFICATION` (0x05) parses `DeviceName` (null-term UTF-16LE) +
      `VirtualChannelName` (null-term ASCII) with reads bounded to the payload
      (CVE-2026-57157 was an OOB scan for those terminators in FreeRDP <3.28.0) and
      logs the GREEN line; on 0x06 logs; else debug. `SelectVersionResponse` is a
      2-byte `Encode`+`DvcEncode` newtype (mirrors `UsbHeaderMsg` in rdpeusb.rs).
    - Robustness: `process()` TOLERATES every decode (log + `Ok(Vec::new())`, never
      propagates) — a decode error would otherwise tear down the whole session for an
      opt-in gate (same lesson as divergences 16/the ironrdp-dvc Soft-Sync one).
    - Wiring: `camera_factory: Option<Box<dyn RdCameraServerFactory>>` field + `new`
      param + struct init + `builder.with_camera_factory` + `.with_dynamic_channel`
      in `attach_channels` (advertised only when `Some` — byte-identical when off).
      LIKE the URBDRC factory, `RdCameraServerFactory` has a `ServerEventSender`
      supertrait + `set_sender`, so the enumerator can ask the event loop to open the
      per-device channel; it also gains `build_sample_sink()` returning the optional
      `CameraSampleSink`. macrdp's cross-platform `src/camera/mod.rs` (`MacCamera`) is
      the factory; the macOS decode/present code sits behind it in `src/camera/
      {decode,feed}.rs` + `gui/Sources/macrdpcamera`.
    Cleanly upstreamable as the server counterpart to a (nonexistent-upstream)
    client MS-RDPECAM; the API has now been shaped by a working implementation.
    Reference: FreeRDP `channels/rdpecam/server/` implements this server side.

(20) Client-fingerprint connect log (NOT upstreamed; added 2026-07-18; pairs with
    acceptor divergence (4)): `client_accepted` logs one `info!` line per initial
    connection (skipped on reactivation) — `client fingerprint` with
    `client_name` / `rdp_version` (hex) / `client_build` (from the acceptor's new
    `AcceptorResult` fields) + `platform` (`major/minor_platform_type` scanned by
    reference from the General capset in `result.capabilities`, before the
    consuming loop). Answers "which RDP client connected": mstsc = real Windows
    build + WINDOWS platform; FreeRDP family = build 2600; Windows Apps = their
    host platform. Lands in macrdp.log next to the `macrdp::audit` lines.
    Informational fingerprinting only. Additive; upstreamable with (4).

(21) Mouse button PDU applies its position before the button (macrdp #166,
    branch `fix/ios-touch-input`, @antonmos; NOT upstreamed, STRONG upstream
    candidate — general input correctness, not macrdp-specific). A `MousePdu`
    carrying a button flag ALSO carries x/y, but `From<MousePdu> for MouseEvent`
    maps it to a positionless `Left/RightPressed|Released` variant and DROPS the
    position — so a lone button PDU clicks wherever the server cursor last was.
    New `pub(crate) fn mouse_events_from_pdu` in `handler.rs` prepends a
    `MouseEvent::Move { x, y }` when the PDU has `LEFT_BUTTON | RIGHT_BUTTON` set;
    both mouse dispatch arms in `server.rs` (fast-path `FastPathInputEvent::
    MouseEvent` + slow-path `InputEvent::Mouse`) call it instead of
    `handler.mouse(mouse.into())`. No-op for clients that move-then-click (mstsc,
    macOS Windows App — the redundant Move has identical coords); fixes the **iOS
    Windows App touch mode**, which sends a tap as a single button PDU with no
    preceding move (taps otherwise land at the stale position). Sits next to the
    wheel-decode fix (17) in the same `From<MousePdu>` area. Scoped to the regular
    Mouse PDU; `MouseX` (the back/forward X buttons) and middle-click have the same
    latent drop but aren't the reported bug (touch taps are left-clicks), so left
    unaddressed. **Filed upstream as Devolutions/IronRDP#1466** (bug report, not a
    patch — the report lays out both the non-breaking synthesize-a-Move shape and
    the breaking position-on-button-event shape and leaves the choice to the
    maintainer, since the same gap is reachable through `MousePdu`/`MouseXPdu`/
    `MouseRelPdu` and a uniform fix touches the public `MouseEvent` API). Delete
    this divergence once the upstream fix lands and the pin moves past it.

(22) Second-client PREEMPTS the live session in the accept loop — SUPERSEDED
    by (23) below; kept for history (added 2026-07-23, superseded 2026-07-27).
    Upstream `RdpServer::run` `await`s the whole `run_connection(stream)`
    inline, so while one client is connected a SECOND client's TCP connect
    sits unserved in the listen backlog — the server never calls `accept()`
    again until the first session ends: a silent HANG for the second client.
    macrdp is single-console-session by design, so the right behavior is for a
    new client to TAKE OVER. The original mechanism reshaped `run()` so the
    in-flight connection raced `listener.accept()`, and a candidate won by
    passing `probe_preempting_client` — a cheap `recv(MSG_PEEK)` check for a
    TPKT header (version 3 + reserved 0), just enough to prove "this looks
    like the start of an RDP handshake." Two narrower bugs in that mechanism
    were fixed in place (on_accept had to run on the candidate BEFORE it could
    start probing, not after it already won; and exactly once per physical
    connection, not twice) before the mechanism itself was found to have a
    structural problem — see (23).

(23) Second-client preemption is gated on FULL AUTHENTICATION, not a TPKT peek
    (NOT upstreamed; added 2026-07-27, replacing divergence (22)'s mechanism).
    **The bug that forced this redesign, found via a real CI failure:**
    `scripts/test-audit-log.sh` (two sequential connections — correct password,
    then wrong password) started failing after (22) shipped: the FIRST
    connection's `event="auth" outcome="success"` never appeared in the audit
    log at all. Root cause: (22)'s `probing = false` from the moment a
    connection started being served meant the accept loop raced
    `listener.accept()` through the live connection's OWN negotiation, not just
    once it was fully active. The test's second `sdl-freerdp` process could
    connect and win the TPKT-peek race (2 bytes, near-instant) BEFORE the
    first connection's TLS+CredSSP handshake (real crypto, genuinely slower)
    completed — cancelling a connection that was about to authenticate
    successfully, based on nothing more than "some other socket also sent 2
    plausible bytes." This is not just a benign-overlap edge case: it means
    ANY connection attempt — including a failed/malicious one that never
    authenticates — could evict the live session purely by winning a race
    against a TPKT peek, which is exactly backwards from what preemption
    should guarantee ("unauthenticated connection shouldn't disconnect the
    session," confirmed as the hard requirement rather than a nice-to-have).

    **The fix: a candidate must complete REAL negotiation — TLS, then CredSSP
    where applicable — before it's allowed to preempt anything.** This is
    architecturally harder than the TPKT peek because `RdpServer` serves one
    connection at a time by design (single `static_channels`, single
    `gfx_handle`, factories that build real per-connection backends against
    shared hardware — SCK capture, camera, USB, RDPDR/NFS); the live
    connection's `run_connection` holds `&mut self` for its whole lifetime, so
    a candidate's negotiation has to run WITHOUT touching `self` mutably at
    all, or it can't run concurrently with the live connection's own
    `&mut self` borrow. The fix is a Phase 1 (negotiate + authenticate,
    concurrency-safe) / Phase 2 (exclusive, resource-driving) split:

    - **`NegotiationContext`** (new struct): a cheap `Rc`/`Arc`-cloned snapshot
      of everything Phase 1 needs — `self.opts`/`self.creds` (both `Clone`),
      `self.display`/`self.handler` (already `Arc`), `self.echo_handle`/
      `self.ev_sender` (already `Clone`), the channel factories, and
      `self.connection_handler` (already `Rc<RefCell<..>>` from the (22)
      on_accept-ordering fix). Built once per race via
      `RdpServer::negotiation_context(&self)`.
    - **Factory storage changed `Box<dyn X>` → `Rc<dyn X>`** (cliprdr, sound,
      rdpdr, usb, camera, gfx) so `NegotiationContext` can hold cheap clones
      instead of needing exclusive access through `self`. Public builder API
      unchanged (still takes `Box<dyn X>`; wrapped via `Rc::from` at
      construction, after the one-time `set_sender` setup that needs `&mut`
      on the still-owned `Box`).
    - **`attach_channels_impl`** (new free function): the channel-attaching
      body of the old `attach_channels`, factored out to take explicit
      factory/handle references instead of reading `self`. `RdpServer::
      attach_channels` (the normal path) now just calls it with `self`'s own
      fields; `negotiate_candidate` calls it with a `NegotiationContext`'s.
      Returns the GFX handle instead of writing `self.gfx_handle` directly —
      only the actual WINNER should claim it, so installation is deferred to
      `serve_negotiated` (Phase 2 entry).
    - **`negotiate_candidate`** (new free function, `TcpStream`-specific):
      duplicates the pre-`accept_finalize` portion of `run_connection` —
      build `Acceptor` → `attach_channels_impl` → `accept_begin` → TLS upgrade
      → CredSSP (when the security mode is Hybrid; fires
      `on_authenticated` on the candidate's outcome too, so a rejected
      preemption attempt still shows up in the audit log) — against a
      `NegotiationContext` instead of `&mut self`. Returns `Some(
      NegotiatedCandidate)` (the negotiated `Acceptor` + framed TLS stream +
      GFX handle) ONLY on full success; any failure at any step — malformed
      negotiation, TLS rejected, CredSSP rejected — returns `None` and the
      live session is left completely untouched. Duplicates rather than
      shares code with `run_connection` because that function is generic over
      any stream type and needs `&mut self`; this needs neither. Keep the two
      in sync by hand if the negotiation sequence changes upstream.
      **DECISION 2026-07-29 — keep the duplication as-is; do NOT refactor it
      into a shared negotiate core.** Unifying was assessed and is technically
      feasible (extract a generic `negotiate<S>(ctx, framed, offer_mt: bool) ->
      NegotiatedCandidate` that both call, with `run_connection` routing through
      `serve_negotiated` for the finalize), but deliberately declined: this is
      security-critical, just-landed vendored code on the auth/preemption path,
      and the "don't refactor working hot-paths" rule applies (a big stateful
      async fn, no unit coverage of the extraction). The RIGHT fix is
      **upstreaming this divergence** — where the shared structure gets designed
      properly and macrdp drops the divergence entirely; a bigger local refactor
      would only INCREASE divergence and make that merge harder. The duplication
      is bounded and only bites on an upstream pin bump (when the vendored server
      is already being re-verified). If the hand-sync ever actually bites, the
      low-risk mitigation is a drift-catching TEST (assert a candidate and a
      normal connection negotiate equivalently), not a structural refactor.
    - **Multitransport is a normal-path-only feature for a candidate.** Both
      the UDP transport OFFER (in `run_connection`, before `attach_channels`)
      and the lossy-audio DVC (inside `attach_channels_impl`, only useful
      paired with that offer) are skipped entirely for a candidate — those
      fields (`multitransport_cookies`, `current_offer_cookie`,
      `udp_tunnel_bound`, …) are process-wide, per-connection-mutated
      bookkeeping shared with the live connection's own multitransport state,
      not safe to touch concurrently. A candidate that wins via preemption
      always negotiates plain TCP; a normal (non-racing) connection is
      unaffected. Same reasoning extends to RTT sampling (`link_rtt_ms`) —
      not sampled for a preemption winner, since by the time it's confirmed
      the winner its raw `TcpStream` is already wrapped in TLS.
    - **`serve_negotiated`** (new method): Phase 2 for a winning candidate —
      installs the deferred GFX handle, resets `auto_reconnect_sent`
      (mirroring the top of `run_connection`), then hands off to the SAME
      `accept_finalize` the normal path uses. From here a preemption-won
      connection is indistinguishable from a normally-accepted one.
    - **`run()`'s race loop**: `pending` now carries a `NegotiatedCandidate`
      (not a raw stream) — by construction, nothing reaches `pending` without
      having already passed `on_accept` AND fully authenticated. The `probe`
      future's output changed from `Option<(TcpStream, SocketAddr)>` to
      `Option<NegotiatedCandidate>`; since that no longer carries the peer
      address, a `probing_peer: Option<SocketAddr>` local tracks it alongside
      `probing`. `conn` (the live connection's future) changed from
      `core::pin::pin!` (stack-pinned, single concrete type) to `Box::pin`
      (heap-allocated, `dyn Future`), because it's now EITHER
      `self.run_connection(stream)` (a fresh `Entry::Fresh`) OR
      `self.serve_negotiated(candidate)` (an `Entry::Negotiated` — a
      previously-preempting candidate that itself is now the live connection,
      and must be just as preemptible by a THIRD candidate) — two different
      concrete future types unified behind one `dyn Future` so the same race
      loop serves both without duplicating it.

    Covered by the existing `src/conn_test.rs::
    second_client_preempts_the_live_session` (a well-behaved candidate still
    wins end-to-end — proves the positive path through the new
    `negotiate_candidate` machinery) and `::
    preemption_does_not_evict_a_session_the_handler_would_reject` (the
    `on_accept` gate still blocks a bad actor before negotiation even starts).
    **Not covered locally: a candidate that reaches CredSSP but fails it**
    (wrong password) — `ironrdp-server` isn't a workspace member here (a
    `[patch.crates-io]` path override), so `#[cfg(test)]` code inside the
    vendor crate itself can never run via any `cargo test` invocation from
    macrdp (confirmed empirically: `cargo test -p ironrdp-server` refuses,
    "requires dev-dependencies and is not a member of the workspace" — the
    same reason other vendored crates' tests live in macrdp's own `src/*.rs`
    testing public APIs instead). Building a full NTLM-capable test client in
    `conn_test.rs` to drive real CredSSP failure was judged more risk/effort
    than the remaining gap warranted, given `scripts/test-audit-log.sh` (real
    sdl-freerdp, correct AND wrong password) is exactly this scenario and runs
    in CI — that's the authoritative check for this path. The property does
    follow directly from the code structure, though: a candidate cannot reach
    `pending` without `negotiate_candidate` returning `Some`, and that can
    only happen after `accept_credssp` returns `Ok`.

    Upstreamable, same shape as (22): an opt-in policy (a `ConnectionHandler`/
    builder switch: reject-new vs preempt-existing-once-authenticated), since
    a general-purpose multi-session server would want to keep the existing
    queue-behind behavior. Filed as Devolutions/IronRDP#1476 — **@antonmos
    rebuilt it (2026-08-03) to exactly this full-auth gate** (a candidate must
    complete real negotiation + CredSSP under Hybrid before it can evict; an
    honest per-mode table; a shared `negotiate_and_authenticate` free fn +
    `attach_channels_impl` + `Box`→`Rc` factories — the same structure as this
    divergence), after CBenoit independently flagged the bare TPKT-peek as
    unsafe (matching macrdp's 07-27 finding) and that peek shape was rejected.
    #1476 also adds the `ERRINFO_DISCONNECTED_BY_OTHERCONNECTION` + anti-storm
    eviction notice macrdp ships. **OPEN, awaiting CBenoit's merge decision**
    (issue #1483 is the backing RFC). When it merges + the pin bumps, macrdp
    de-vendors this divergence.

    **Eviction must tell the loser WHY, or the two clients ping-pong forever
    (2026-07-27, found in live testing of the above — the failure that made
    this a two-part fix).** With auth-gating in place, preemption worked
    exactly as designed and was still unusable: two real clients on a LAN
    (.44 and .46) traded the session back and forth every ~1-2 s,
    indefinitely. The loop is entirely self-inflicted and obvious in
    hindsight: macrdp provisions the **Server Auto-Reconnect Cookie** by
    default (divergence 13, so a blank-recovery drop heals seamlessly), so a
    client dropped for ANY reason silently auto-reconnects about a second
    later. A preemption drop was indistinguishable from a blank-recovery drop,
    so the evicted client came straight back, authenticated (legitimately —
    it's a real client with real credentials, so the auth gate is no defense
    here), and preempted the client that had just replaced it. Repeat forever.
    Note this is a case where each individual component behaved *correctly*
    and the composition was broken; nothing was going to catch it short of
    running two real clients.

    Two layers now:

    1. **`ServerEvent::EvictedByOtherConnection` (the real fix).** A new event
       distinct from `Quit`: `dispatch_server_events` (which has the writer +
       channel ids) sends a Server Set Error Info PDU carrying
       `ERRINFO_DISCONNECTED_BY_OTHERCONNECTION` (0x05, MS-RDPBCGR 2.2.5.1.1 —
       the exact code real Windows RDS uses for a session takeover) before
       returning `Disconnect`. A client that receives an administrative
       disconnect reason shows it ("another user connected…") and does NOT
       auto-reconnect. `run()`'s preemption path sends this and then keeps
       polling the incumbent for `EVICTION_GRACE` (750 ms) so the PDU actually
       reaches the wire, instead of cancelling the future outright; on timeout
       it degrades to exactly the hard cancellation it replaced, so a
       wedged/half-dead peer can't stall the takeover. Uses the same
       `encode_share_data_pdu` helper the ARC cookie does.
    2. **`recently_evicted` + `REPREEMPT_COOLDOWN` (the net).** Whether a
       given client honors the error info is client-dependent and unverified
       across the field, and an *infinite* flap is a bad enough failure to be
       worth a structural bound. So a peer evicted moments ago may not
       immediately preempt back: keyed on source IP (the source PORT changes
       on every reconnect, so it can't be part of the key), 5 s, and — the
       load-bearing detail — **each refused attempt RE-ARMS the window**. An
       auto-reconnect storm at ~1 s cadence therefore can never win the
       session back no matter how long it runs, while a human who closes the
       client and reconnects (a gap well over 5 s) still can. Cleared when a
       session ends on its own rather than being replaced. **Known
       limitation:** IP-keyed, so two distinct clients behind one NAT/public
       IP will briefly (≤5 s, re-armed) block each other's takeover right
       after an eviction. Accepted — it's a net under the real fix, not the
       mechanism itself, and the alternative (no bound at all) is worse.

       **Known limitation 2 — a MULTI-HOMED auto-reconnecting client DEFEATS
       the net (live-observed 2026-08-04 on the soak mini).** The inverse of
       limitation 1: the cooldown keys on source IP, so a SINGLE client
       reachable over more than one network path presents more than one IP and
       just alternates them to dodge it — barred on IP-a, it auto-reconnects on
       IP-b (a different key → not barred), re-takes the session, and the
       ping-pong the whole `recently_evicted` / `EvictedByOtherConnection`
       machinery exists to stop runs anyway. Observed with the macOS **Windows
       App** (`client_name=MacBook-Pro`) reaching the mini over BOTH LAN
       (192.168.0.185) AND ZeroTier (10.241.115.104): it (a) does NOT honor
       `ERRINFO_DISCONNECTED_BY_OTHERCONNECTION` — it auto-reconnects after
       eviction regardless (the notice's "client-dependent" caveat, now
       confirmed for this client), and (b) is multi-homed, so it repeatedly
       stole the session back from an mstsc peer (192.168.0.149) across the two
       IPs — one of which the guard WAS actively barring (`ignoring a reconnect
       from the peer just evicted` fired correctly on that path each time; it
       simply can't cover the other). Takeover ITSELF worked correctly
       throughout and the server stayed a single stable process under the storm
       — only the anti-flap NET is defeated, not preemption. No perfect
       server-side key exists at the preempt-decision point (source IP is the
       only per-connection-stable id there — `client_name` is spoofable /
       non-unique, and the session/logon identity isn't known until after the
       candidate authenticates). Practical fix is CLIENT-SIDE: fully QUIT the
       losing client (not just disconnect) / disable its auto-reconnect / point
       it at a single address. Possible server-side hardening, DEFERRED (do not
       touch mid-soak): key the cooldown on the candidate's negotiated logon
       identity instead of (or in addition to) source IP, or a global "just
       evicted → refuse ALL new preemptions for N ms" quench that a
       multi-homed client can't route around. Also compounds cosmetically with
       the mstsc first-connect cert-prompt broken-pipe (`auth
       did_not_complete [write all]` → immediate retry succeeds), which makes
       the winning peer's OWN reconnect read as "asks for credentials, then
       disconnects" on its first attempt before the retry lands.

    Covered by `src/conn_test.rs::second_client_preempts_the_live_session`
    (extended: now asserts the evicted session actually RECEIVES
    `ERRINFO_DISCONNECTED_BY_OTHERCONNECTION`, decoding the PDU rather than
    just checking the socket closed) and `::
    a_just_evicted_peer_cannot_immediately_preempt_back` (reproduces the live
    ping-pong: A connects, B preempts, A immediately reconnects → refused, B
    survives; note every loopback peer is 127.0.0.1, which is exactly the
    source-IP key the net uses). Both verified to fail without their
    respective fix and pass with it.

    **AMENDMENT 2026-08-09 — three bugs in the above, found by the automated
    reviewer on the upstream PR (Devolutions/IronRDP#1476) and confirmed
    present here.** The upstream and vendored copies had drifted only
    cosmetically, so all three applied verbatim:

    (a) **CRITICAL — unauthenticated remote hang of the accept loop.**
        `negotiate_candidate` blocks on socket reads, and `PreemptRace::Ended`
        did a bare `pending = probe.await`. A peer that completed the TCP
        handshake, cleared `on_accept` and then sent NOTHING parked
        `accept_begin` forever: accepts were already disabled for the rest of
        the session (the accept arm is gated on `!probing`), and once the live
        session ended the loop blocked on that await with no `select!` left —
        no accepts, no event drain, so not even `ServerEvent::Quit` could stop
        the server. One silent TCP connection wedged the listener until the
        process was restarted. **The health watchdog does NOT catch this**: the
        tokio runtime stays healthy and its probe still runs; only the accept
        loop is stuck, so `src/health.rs` sees nothing wrong. Fixed with TWO
        deliberately separate bounds — `CANDIDATE_NEGOTIATION_TIMEOUT` (10 s)
        caps the negotiation itself, and `CANDIDATE_HANDOFF_GRACE` (750 ms)
        caps the post-session-end wait, which is the window where the loop
        services nothing at all. Capping only the first is NOT enough (the
        loop is then deaf for the full negotiation budget). A candidate that
        can't finish inside the grace is dropped and simply reconnects.

    (b) **The eviction event could kill the WINNER.**
        `EvictedByOtherConnection` rides the server-global `ev_sender` but is
        consumed by whichever connection drains it next. An incumbent too
        wedged to take it within `EVICTION_GRACE` — and being wedged is
        precisely why it is being evicted — left it queued, and the winner's
        `client_loop` then drained it and disconnected itself, putting a bogus
        `ERRINFO_DISCONNECTED_BY_OTHERCONNECTION` on the wire to the client
        that had just taken over. `RdpServer::discard_stale_eviction_events()`
        drops leftovers immediately before serving a winner, re-queuing every
        other event in arrival order (collect-then-resend, so the re-sends
        don't land back in the queue being drained).

    (c) **The anti-storm cooldown locked out its own headline case.** The
        re-arm had no cap, so a client whose link dropped could never reclaim
        its own stale session while auto-reconnecting — exactly the scenario
        this feature exists for. `REPREEMPT_MAX_LOCKOUT` (30 s) bounds it from
        the eviction; within the cap the re-arm still throttles a storm, past
        it the bar lifts even under a continuing storm. The
        `ERRINFO_DISCONNECTED_BY_OTHERCONNECTION` notice remains the real fix
        for the loop; this heuristic is only a backstop, so bounding it is the
        right trade. `recently_evicted` is now `(IpAddr, evicted_at, last_try)`.

    Covered by `src/conn_test.rs::a_silent_candidate_cannot_wedge_the_accept_loop`
    (live session + a silent candidate + session end → a later client must
    still be served), verified to fail without the fix — it times out with
    "the loop never accepted it" — and pass with it.
