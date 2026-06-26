#!/bin/sh
# soak-lossy.sh — run macrdp configured for the P2.2 *lossy-delivery* UDP soak and
# surface the handshake/tunnel markers that tell you whether the lossy
# (`UdpFecL`) flow survives an emulated lossy link.
#
# Pairs with scripts/netshape.sh (the traffic shaper). Two terminals:
#
#   # terminal 1 — shape the link (both directions, TCP+UDP on the port):
#   sudo scripts/netshape.sh on --loss 5 --delay 100
#
#   # terminal 2 — run the server under the lossy-delivery soak config:
#   scripts/soak-lossy.sh --enable-udp-multitransport --enable-h264 \
#       --username "$USER" --password 'PASS' --bind 0.0.0.0:3390
#
#   # …connect mstsc from another machine, exercise it, then:
#   sudo scripts/netshape.sh off
#
# What it does: exports the two experimental env vars the lossy path needs
# (MACRDP_UDP_OFFER_FECL=1 so the client opens a lossy flow; MACRDP_UDP_LOSSY_DELIVERY=1
# so that flow drives DeliveryMode::Lossy), sets a soak-appropriate RUST_LOG if you
# haven't, runs macrdp with the args you pass through, tees the FULL log to a
# timestamped file, and live-prints only the soak-relevant lines so the signal isn't
# buried. All other args (--password, --bind, feature flags, …) pass straight through.
#
# Override knobs (env):
#   MACRDP   — path to the binary (default: target/release/macrdp)
#   RUST_LOG — logging filter (default: the soak filter below)
#   SOAK_LOG — full-log output path (default: ./soak-lossy-<timestamp>.log)
#
# A/B note: to compare against the *reliable* SM carrying the same lossy flow (the
# control), re-run with MACRDP_UDP_LOSSY_DELIVERY=0 in front of this script — the
# flow then rides the reliable SM (retransmits), which is the proven path.

set -eu

MACRDP="${MACRDP:-target/release/macrdp}"

if [ ! -x "$MACRDP" ]; then
    echo "error: macrdp binary not found / not executable at: $MACRDP" >&2
    echo "  build it first: cargo build --release" >&2
    echo "  (or set MACRDP=/path/to/macrdp)" >&2
    exit 1
fi

# The lossy path is experimental + env-gated; the soak turns both gates on.
export MACRDP_UDP_OFFER_FECL=1
export MACRDP_UDP_LOSSY_DELIVERY="${MACRDP_UDP_LOSSY_DELIVERY:-1}"

# Enough to see the handshake lifecycle + the reliability counters, without the
# per-keystroke input spam.
export RUST_LOG="${RUST_LOG:-info,ironrdp_server::multitransport=debug,ironrdp_acceptor=info,macrdp::h264=info,macrdp::audio=info}"

ts="$(date +%Y%m%d-%H%M%S)"
SOAK_LOG="${SOAK_LOG:-./soak-lossy-${ts}.log}"

# The lines worth watching during a lossy soak: the lossy-flow classification, the
# DTLS/tunnel handshake milestones (P2.1/P2.4 GREEN = it survived loss to established),
# the reliability counters (RTO retransmits should be ZERO for a pure-lossy flow — any
# reliable-flow retransmits still show), and the failure tells (broken pipe / reset /
# cookie reject / handshake never completing).
MARKERS='LOSSY delivery|MACRDP_UDP_LOSSY_DELIVERY|SPIKE GREEN|P2\.1 GREEN|P2\.4 GREEN|CREATEREQUEST|CREATERESPONSE|cookie not recognized|RTO retransmit|handshake established|Connection error|Broken pipe|Connection reset|reliable data delivered|receive-sequence mismatch'

echo "soak-lossy: MACRDP_UDP_OFFER_FECL=$MACRDP_UDP_OFFER_FECL MACRDP_UDP_LOSSY_DELIVERY=$MACRDP_UDP_LOSSY_DELIVERY"
echo "soak-lossy: binary=$MACRDP"
echo "soak-lossy: full log -> $SOAK_LOG (paste this if reporting)"
echo "soak-lossy: live view below is filtered to soak markers; the file has everything."
echo "soak-lossy: shape the link in another terminal: sudo scripts/netshape.sh on --loss 5 --delay 100"
echo "---"

# Full output to the file; live console filtered to the markers. `grep -E` is line
# buffered on a pipe under macOS, so lines appear as they happen.
"$MACRDP" "$@" 2>&1 | tee "$SOAK_LOG" | grep -E --line-buffered "$MARKERS" || true
