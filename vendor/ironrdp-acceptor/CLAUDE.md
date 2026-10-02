# vendor/ironrdp-acceptor — divergence log

Local fork of ironrdp-acceptor 0.10.0, **re-vendored 2026-10-01 from upstream
Devolutions/IronRDP@e258f6a0** (the pin-bump rev). `src/` is upstream verbatim except
`connection.rs`, which carries the two divergences below (every insertion is marked
`(macrdp divergence N)`). Earlier vendorings: @a5d1c682 (2026-08-05), @879ffed
(2026-06-12). Has a standalone `[patch.crates-io]` (core/pdu/svc/connector/async →
e258f6a0) for isolated builds, ignored in the macrdp workspace (the root `[patch]`
wins). Keep that rev in sync. `rustfmt.toml` mirrors the IronRDP workspace config
(added 2026-10-01 — without it rustfmt reformats upstream's 120-column code).

**Known, pre-existing, not ours:** `cargo clippy -- -D warnings` on this crate alone
flags a `collapsible_if` in upstream's credentials check (`connection.rs`, the
`if let Some(expected) = &self.creds` block). It's a newer-clippy lint (our 1.95 vs
upstream's 1.94); verified identical on pure upstream source. macrdp's own clippy run
doesn't lint path dependencies. Don't "fix" upstream code here.

## Testing

The lib is `test = false`, and the useful harness lives upstream in
`ironrdp-testsuite-core/tests/server/acceptor.rs`. To test a change, copy
`connection.rs` into the IronRDP clone (`~/Documents/Projects/Misc/IronRDP`) at the
pinned rev and run `cargo test -p ironrdp-testsuite-core server::acceptor`. The
2026-10-01 rebase did exactly that on the local branch
`scratch/macrdp-acceptor-divergences` (not pushed): four `macrdp_*` tests below, plus
upstream's 13 acceptor tests and the whole suite (1731) passing against this fork.
Both divergences were mutation-checked.

## Divergences

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

## Retired

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

## Porting notes for the server rebase (vendored ironrdp-server)

- Offer: `acceptor.set_multitransport_offer(Some(flag | MultiTransportFlags::SOFT_SYNC_TCP_TO_UDP))`
  where `flag` is `TRANSPORT_TYPE_UDP_FECR` or `_FECL`, plus
  `set_multitransport_requested_protocol(..)` for the lossy case.
- Cookie: implement `MultitransportSecurityRng` so `fill_security_cookie` registers the
  cookie in the multitransport `CookieRegistry` as it is generated — the UDP listener
  then knows it before the client's first datagram (no race), replacing `new_offer()`.
- Replace `result.multitransport_offered` with `acceptor.multitransport_request()` (or
  the `accept_finalize_with_multitransport` handler).
