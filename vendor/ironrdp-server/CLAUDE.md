# vendor/ironrdp-server — divergence log

Local fork of ironrdp-server 0.10.0, pulled in via `[patch.crates-io]` in
`Cargo.toml`. The audio-lag control in the dedicated `dispatch_audio` task
(carved out of `dispatch_server_events`) is the live divergence. Keep this
vendor dir until (2)/(3)/(4)/(5)/(6)/(7)/(8)/(9)/(10)/(11) below are upstreamed
AND released — #1276 landing is NOT sufficient.

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

(7) Opt-in QOI Rgb-only workaround for pre-PR-#1335 `ironrdp-session` clients
    (process-global `QOI_FORCE_RGB: AtomicBool`, public setter
    `set_qoi_force_rgb`, wired through macrdp's `--qoi-force-rgb` CLI flag —
    default OFF, so `qoi_encode` emits the natural `*a` `qoi::RawChannels`
    matching the source PixelFormat, identical to upstream). When the flag is
    set, every 4-byte input maps to its `*x` sibling so the QOI header
    advertises `Channels::Rgb` instead of `Channels::Rgba`. Context: upstream
    `ironrdp-session`'s `fast_path.rs::qoi_apply` Rgba arm is
    `warn!("Unsupported RGBA QOI data")` and drops the frame, so any client
    carrying that code negotiates QOI, gets `Rgba`, and renders blank (412
    RGBA-warn lines in ~12s on the loopback repro). PR #1335 ✅ MERGED 2026-06-01
    (commit `8a9ee626`) upstreams the Rgb behaviour as the default; the companion
    client-side patch landed as PR #1341 ✅ MERGED 2026-06-01 (commit `ef20ea4e`,
    branch `feat-client-rgba-qoi`) adding Rgba decode to `ironrdp-session` (plus a
    size-guard in `qoi_apply` against oversized payloads). Both are MERGED but NOT
    yet released — once a release ships them, the workaround + `--qoi-force-rgb`
    flag (commit `e22a617`) can be deleted. Until then, users pointing
    `ironrdp-viewer` at macrdp should pass `--qoi-force-rgb`; mstsc / MS Remote
    Desktop / Windows App / FreeRDP don't advertise QOI and are unaffected.

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

(9) Honor-client-desktop-size plumbing (NOT upstreamed; pairs with the
    `vendor/ironrdp-acceptor` divergence (1)): `RdpServer` gains a
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
    opts out). Offer upstream together with the acceptor change.

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
