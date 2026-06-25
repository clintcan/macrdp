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
