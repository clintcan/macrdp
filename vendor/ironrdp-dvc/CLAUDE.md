# vendor/ironrdp-dvc — divergence log

Local fork of ironrdp-dvc 0.5.0, copied 2026-06-26 from upstream
Devolutions/IronRDP@879ffed (the same rev as the other git pins) and pulled in
via the root `Cargo.toml`. Keep this vendor dir until the divergence below is
upstreamed AND released.

**Patch wiring is two-sided.** Unlike the other vendored crates, ironrdp-dvc is a
**path dependency of the git-pinned ironrdp crates** (egfx / displaycontrol /
echo all depend on it *within the IronRDP git workspace*), so `[patch.crates-io]`
alone leaves two versions — the git copy those crates pull and the vendored copy
ironrdp-server pulls — which fail to unify (trait mismatch). The root Cargo.toml
therefore patches **both** sources to the single vendored copy:

```toml
[patch.crates-io]
ironrdp-dvc = { path = "vendor/ironrdp-dvc" }
[patch."https://github.com/Devolutions/IronRDP.git"]
ironrdp-dvc = { path = "vendor/ironrdp-dvc" }
```

## Style / cleanliness

Copied verbatim from upstream and kept that way: every change below is a **pure
addition** (new lines inserted into the enums / match arms / a new code block) —
`diff` against the upstream src shows **zero deletions or modifications** to
existing upstream lines. `rustfmt.toml` mirrors the IronRDP workspace config
(`max_width=120`, `imports_granularity="Module"`, `group_imports="StdExternalCrate"`)
so re-formatting doesn't churn the crate away from upstream. (The two import
opts are nightly-only; stable `cargo fmt` warns and skips them, which is fine —
the additions were written to already match upstream grouping.) The crate is a
path dep, so the root `cargo fmt`/`cargo test` don't reach it; it's exercised
through macrdp's build and its Soft-Sync codec is round-trip tested from the
macrdp crate (`src/multitransport.rs`, via the public `Drdynvc*Pdu` traits)
since the lib is `test = false`.

## Divergence

(1) Server-direction MS-RDPEDYC **Soft-Sync** support (NOT upstreamed) — needed
    to move dynamic virtual channels (EGFX) onto the UDP multitransport tunnel
    (the macrdp UDP-multitransport work, M5). Upstream models Soft-Sync's two
    `Cmd` values (`SoftSyncRequest=0x08`, `SoftSyncResponse=0x09`) in the `Cmd`
    enum + `TryFrom`, but neither `DrdynvcServerPdu` nor `DrdynvcClientPdu`
    carries a variant for them and the `decode` match arms fall through to
    `unsupported_value_err!` — so a server that receives the client's Soft-Sync
    Response **tears the connection down** (the error propagates out of
    `DrdynvcServer::process` → `svc.process(...)?` in the vendored server's
    `handle_x224`). KEY ARCHITECTURE FACT: Soft-Sync is **MS-RDPEDYC (drdynvc),
    NOT MS-RDPEMT/RDPEUDP** — both Soft-Sync PDUs ride the DRDYNVC static channel
    on the **main (TCP) connection**; only the channel *data* after the switch
    rides the UDP tunnel (as RDP_TUNNEL_DATA wrapping the same DRDYNVC DATA PDU).
    So the codec belongs here, in the drdynvc crate, next to every other DVC PDU.

    Added in `src/pdu.rs` (all `pub`, in module `pdu`):
    - constants `TUNNELTYPE_UDPFECR=0x01` / `TUNNELTYPE_UDPFECL=0x03`,
      `SOFT_SYNC_TCP_FLUSHED=0x01` / `SOFT_SYNC_CHANNEL_LIST_PRESENT=0x02`.
    - `SoftSyncChannelList { tunnel_type: u32, channel_ids: Vec<DynamicChannelId> }`
      (DYNVC_SOFT_SYNC_CHANNEL_LIST: TunnelType u32, NumberOfDVCs **u16**,
      ListOfDVCIds u32 each — all LE).
    - `SoftSyncRequestPdu { header, flags: u16, channel_lists }` — server→client
      DYNVC_SOFT_SYNC_REQUEST (Header byte `Cmd<<4`; Pad u8; `Length` u32 counting
      Length+Flags+NumberOfTunnels+lists; Flags u16; NumberOfTunnels **u16**;
      then the lists). `switch_to_udpfecr(channel_ids)` convenience builds the
      common "move these DVCs to the reliable tunnel" request — an **empty**
      `channel_ids` is a valid "flush TCP, migrate nothing" probe (no list,
      NumberOfTunnels=0, CHANNEL_LIST_PRESENT unset), the M5c safe spike. Wired into
      `DrdynvcServerPdu::SoftSyncRequest` (Encode/name/size only — the server
      never *decodes* its own request, so no `DrdynvcServerPdu::decode` arm).
    - `SoftSyncResponsePdu { header, tunnels: Vec<u32> }` — client→server
      DYNVC_SOFT_SYNC_RESPONSE (Header byte; Pad u8; NumberOfTunnels **u32** —
      asymmetric vs the request's u16; TunnelsToSwitch u32 each). Wired into
      `DrdynvcClientPdu::SoftSyncResponse` with a full Encode + `decode` arm
      (`Cmd::SoftSyncResponse => …`) so the server can read it without erroring.

    Added match arms (graceful handling, no behavior beyond a debug log):
    - `src/server.rs` `DrdynvcServer::process` — `DrdynvcClientPdu::SoftSyncResponse`:
      acknowledge it; no reply is required, and routing DVC data onto the UDP
      tunnel is the multitransport layer's job (macrdp), not drdynvc's.
    - `src/client.rs` `DrdynvcClient::process` — `DrdynvcServerPdu::SoftSyncRequest`:
      dead code for macrdp (we only use the *server* half) but the match must be
      exhaustive; log and ignore (a real client would reply + switch tunnels).

    The byte layouts are anchored by round-trip unit tests in macrdp's
    `src/multitransport.rs` (`soft_sync_request_encodes_to_exact_wire_bytes`,
    `soft_sync_response_decodes_from_wire_and_round_trips`). The earlier
    M5b-1 hand-rolled copies in `vendor/ironrdp-rdpeudp/src/softsync.rs` were
    removed in favor of this (correct) layer; see that crate's CLAUDE.md.

## Upstream candidacy

The Soft-Sync request/response PDUs and the two `process()` arms are a clean,
additive feature (server-side Soft-Sync). They're a good upstream PR once the
macrdp UDP-multitransport data path is proven end-to-end. Until then this stays
vendored.
