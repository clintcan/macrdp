# vendor/ironrdp-dvc — divergence log

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

## Style / cleanliness

Copied verbatim from upstream and kept that way: the divergence is a **pure
addition** — `diff` against upstream `src/` shows **zero deletions or modifications**
of upstream lines. `rustfmt.toml` mirrors the IronRDP workspace config. Under
`cargo +nightly fmt`, upstream's own `client.rs` differs (upstream formats with stable
rustfmt, which skips the two nightly-only import options) — leave it, reformatting
upstream code would break the pure-addition property. The crate is a path dep, so the
root `cargo fmt`/`cargo test` don't reach it, and its lib is `test = false`; run its
tests on a scratch copy with `test = true` (`cargo test --lib`).

## Divergence

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

## Retiring this fork

Two routes. (a) Upstream (3): the lossy + empty generalisation is a natural
extension of `request_reliable_udp`; the no-routing part is macrdp-specific (it
exists for the TCP fallback), so an upstream version would more likely register
routing and add an explicit "release this channel back to TCP" call. (b) Adopt
upstream's own UDP routing (#1954) at the group-2 UDP swap, at which point the TCP
fallback is re-examined anyway. Either way, verify the fallback live on mstsc.
