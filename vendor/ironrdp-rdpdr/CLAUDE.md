# vendor/ironrdp-rdpdr — divergence log

Local fork of ironrdp-rdpdr 0.7.0, copied 2026-06-16 from upstream
Devolutions/IronRDP@879ffed and **re-synced 2026-08-05 to the a5d1c682 pin bump**
(upstream had ZERO source churn between the two revs — only Cargo.toml/CHANGELOG —
so the fork's source was already current; just the dep versions bumped). Pulled in
via `[patch.crates-io]` in the root `Cargo.toml`; has a standalone
`[patch.crates-io]` (core/error/pdu/svc → a5d1c682) for isolated build, ignored in
the macrdp workspace. Keep this vendor dir until divergence (1) is upstreamed AND
released.

Upstream `ironrdp-rdpdr` is **client-oriented**: `Rdpdr` is a
`SvcClientProcessor`, and the PDU `Encode`/`Decode` impls in `pdu::efs` only
cover the direction a *client* needs (encode client→server, decode
server→client). macrdp is the **server**, so it needs the opposite halves on a
few PDUs. The wire structs, field layouts, and constants are all upstream and
reused as-is; we only add the missing server-direction halves.

(1) Server-direction decode halves + accessors (NOT upstreamed):
    - `ClientNameRequest::decode` — server reads PAKID_CORE_CLIENT_NAME.
    - `ClientDeviceListAnnounce::decode` — server reads
      PAKID_CORE_DEVICELIST_ANNOUNCE (loops `DeviceAnnounceHeader::decode`).
    - `DeviceAnnounceHeader::decode` + `PreferredDosName::decode`, plus public
      accessors `device_id()` and `preferred_dos_name()`, and `device_type()`
      widened from `pub(crate)` to `pub`, so the server can read the announced
      device id / type / label.
    These pair with the **server-side `RdpdrServer` processor** that lives in
    `vendor/ironrdp-server/src/rdpdr.rs` (divergence (11) there) — kept out of
    this crate so the macrdp-facing factory/backend traits sit next to the other
    server channel factories. Outbound (server→client) reuses the existing
    `RdpdrPdu`/`*::encode` impls unchanged: `VersionAndIdPdu`, `CoreCapability`,
    `ServerDeviceAnnounceResponse` all have public fields and working `encode`,
    so the server constructs them directly.

    Phase 1b added the device-I/O halves: `encode` for `DeviceCreateRequest` /
    `DeviceReadRequest` / `DeviceCloseRequest` (write the `DeviceIoRequest`
    header + body), `decode` for `DeviceCreateResponse` / `DeviceReadResponse` /
    `DeviceCloseResponse`, and an `impl Encode + SvcEncode for
    ServerDriveIoRequest` (in `pdu/mod.rs`) that prepends the
    `PAKID_CORE_DEVICE_IOREQUEST` SharedHeader so the server can emit a request
    as an `SvcMessage`. Phase 1b-ii (list_dir) added `encode` for
    `ServerDriveQueryDirectoryRequest` and `decode` for `FileDirectoryInformation`
    (the directory-entry class the server requests); the query-directory response
    is decoded inline in the server's `RdpdrHandle::list_dir` (DeviceIoResponse +
    Length + one entry per response, looped until NO_MORE_FILES).

    Phase 2 (writes) added the server-direction halves for the write path:
    `encode` for `DeviceWriteRequest` and `ServerDriveSetInformationRequest`,
    `decode` for `DeviceWriteResponse`, `encode` for the set-information buffers
    `FileEndOfFileInformation` / `FileDispositionInformation` /
    `FileRenameInformation` / `FileAllocationInformation` (upstream had only
    their `decode`), the matching `FileInformationClass::encode` arms + a
    `FileInformationClass::level()` helper (maps a buffer to its
    `FileInformationClassLevel`), and the `DeviceWriteRequest` /
    `ServerDriveSetInformationRequest` arms in `ServerDriveIoRequest`'s
    `Encode`/`size` dispatch (`pdu/mod.rs`). `DeviceCreateRequest::encode`
    (Phase 1b) already carries the create-disposition, so create/mkdir reuse it.

    Upstreamable as a `SvcServerProcessor` peer to the client `Rdpdr` (offer the
    decode halves + the server processor together). De-vendor once a published
    ironrdp-rdpdr carries a server-side path.

(2) Server-direction MS-RDPESC (smart-card) halves in `pdu/esc/` (NOT
    upstreamed) — the smart-card analogue of (1), for the
    `--enable-smartcard-redirection` path. Upstream `pdu::esc` is client-oriented
    (decode `*Call`, encode `*Return`); macrdp is the server, so it needs the
    mirror halves. Added, all in `pdu/esc/`:
    - `rpce::HeaderlessEncode` for the `*Call` set the server sends:
      `EstablishContextCall`, `ContextCall` (release/cancel/is-valid),
      `ListReadersCall`, `GetStatusChangeCall`, `ConnectCall`,
      `HCardAndDispositionCall` (begin/end-transaction, disconnect), `StatusCall`,
      `TransmitCall`.
    - `rpce::HeaderlessDecode` + a `decode()` for the matching `*Return` set:
      `LongReturn`, `EstablishContextReturn`, `ListReadersReturn`,
      `GetStatusChangeReturn`, `ConnectReturn`, `StatusReturn`, `TransmitReturn`.
    - Supporting NDR encoders the encode side needs: `ndr::Encode` for
      `ConnectCommon` and `ReaderState`, plus `ndr::write_string_to_cursor` /
      `ndr::string_size` (the conformant+varying string *writer* mirroring
      `read_string_from_cursor` — MaximumCount/Offset/ActualCount + NUL-terminated
      string + 4-byte tail pad; the pad is position-based on write but, since
      every MS-RDPESC string field starts 4-byte aligned, equals
      `region.next_multiple_of(4)` for sizing).
    - `TryFrom<u32>` for `ReturnCode` and `CardState`, and `From<Scope> for u32`
      (the reverse conversions upstream only had one direction of).
    Byte-exactness is proven offline by `server_direction_tests` (18 round-trips:
    `*Call` = our encode -> upstream decode; `*Return` = upstream encode -> our
    decode). The server uses the **W (Unicode)** IOCTL variants, so reader/string
    fields marshal as UTF-16.

    The IOCTL envelope is also here now: `ScardCall::encode`/`size` (dispatch the
    chosen variant's RPCE `Pdu`) and **`ScardControlRequest`** in `pdu/mod.rs` — a
    server-direction DR_CONTROL_REQ (`IRP_MJ_DEVICE_CONTROL`) that prepends the
    `PAKID_CORE_DEVICE_IOREQUEST` `SharedHeader` + `DeviceIoRequest`, then
    Output/Input buffer lengths + `IoControlCode` + 20 reserved bytes + the
    marshaled call, and impls `Encode + SvcEncode` so it ships as an `SvcMessage`
    (peer to `ServerDriveIoRequest`). `From<ScardIoCtlCode> for u32` added.
    `scard_control_request_tests` proves the full envelope round-trips through the
    decode chain (`SharedHeader` -> `DeviceIoRequest` ->
    `DeviceControlRequest<ScardIoCtlCode>` -> `ScardCall`).

    Live-Windows conformance fixes (2026-06-18, found verifying against mstsc +
    a TPM virtual smart card — the offline round-trips couldn't catch these
    because they encode/decode symmetrically; real 64-bit Windows exercises NDR
    edges our own encoder never produced):
    - **Variable-length context/handle.** `ScardContext`/`ScardHandle` `value`
      changed from `u32` to `u64` + a `length: u8` (was hardcoded 4). Real 64-bit
      Windows `SCARDCONTEXT`/`SCARDHANDLE` are pointer-sized (8 bytes); the old
      decode rejected them with "unsupported value length". `read_cb` caps at 8.
    - **NULL-referent handling on every `[unique]` pointer's value section.** A
      NULL referent means NO deferred conformant array — reading a `MaximumCount`
      unconditionally consumes the next field's bytes. Fixed in: `StatusReturn`
      (Windows returns NULL `mszReaderNames` — ATR-only Status), `SCardIORequest`
      (empty `pbExtraBytes` → NULL referent, no deferred), `TransmitReturn` (NULL
      `pbRecvBuffer` when the card returns no data), and **`ScardContext`** (the
      embedded Context of a returned handle is empty: `cbContext=0` + NULL
      referent + no value). The `ScardContext` one was the killer: it made the
      connect handle decode with `cbHandle=0`, so Transmit sent an empty handle.
    Regression tests added for each (hand-built Windows-shaped bytes +
    8-byte-handle round-trips); 27 tests total. **VERIFIED end-to-end on mstsc**:
    full APDU transceive (GIDS SELECT → FCI + `90 00`) round-trips through the
    redirected reader.

    Server-side path (the "STILL TODO" below) is DONE — see
    `vendor/ironrdp-server/src/rdpdr.rs` divergence (11) smart-card phase: the
    `RdpdrHandle::scard_*` methods + completion router.

Cargo notes: the de-worked `Cargo.toml` inlines the workspace-inherited fields
(edition 2024, rust-version, license, …) and drops the `path = "../ironrdp-*"`
deps, resolving them through the root `[patch.crates-io]` git pins — same shape
as `vendor/ironrdp-acceptor`. Its `ironrdp-error = "0.1"` dep is why the root
adds an `ironrdp-error` git pin to `[patch.crates-io]`: without it, this crate
would pull `ironrdp-error` from crates.io and split it from the copy the other
ironrdp crates use transitively.
