# vendor/ironrdp-rdpdr — divergence log

Local fork of ironrdp-rdpdr 0.7.0, **re-vendored 2026-10-01 from upstream
Devolutions/IronRDP@e258f6a0** (the pin-bump rev). Earlier vendorings: @a5d1c682
(2026-08-05), @879ffed (2026-06-16). Pulled in via `[patch.crates-io]` in the root
`Cargo.toml`; has a standalone `[patch.crates-io]` (core/error/pdu/svc → e258f6a0) for
isolated builds, ignored in the macrdp workspace (the root `[patch]` wins). Keep that rev in
sync.

## Shape: upstream verbatim + two new files

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

## Why a fork at all

macrdp's RDPDR server processor is its own (`vendor/ironrdp-server/src/rdpdr.rs`,
divergence (11) there), and it needs to build requests and parse replies in the server's
direction. Upstream has since made the EFS (drive) PDU layer two-way (#1779) and added its
own `RdpdrServer` (#1783/#1784); its ESC (smart card) layer is still client-only. Moving
macrdp onto upstream's `RdpdrServer` is a later, separate change (group 2 of the pin-bump
triage). Until then this crate supplies what our processor uses.

## Divergences

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

## Retired at e258f6a0 (upstream now has it)

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

## Testing

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

## Porting notes for the server rebase (vendored ironrdp-server)

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
