#!/bin/sh
# soak-lossy-audio.sh — run macrdp configured for the P2.4b *lossy-audio* UDP soak
# and surface the audio-DVC / tunnel markers that tell you whether AAC audio rides
# the lossy (`UdpFecL`) DTLS tunnel cleanly over an emulated lossy link.
#
# This is the test that justifies the whole lossy phase: on a pure-TCP session
# every channel (video, input, audio, clipboard) multiplexes onto ONE TCP stream,
# so a single dropped segment head-of-line-blocks audio until it's retransmitted —
# you hear gaps / desync that grow with loss. With audio migrated onto the lossy
# UDP tunnel (this script), a lost wave is just *gone* (drop-stale is correct AND
# free on a live stream — no retransmit to wait on), so playback stays smooth.
#
# Pairs with scripts/netshape.sh (the traffic shaper). It is the audio sibling of
# scripts/soak-lossy.sh (which soaks the *transport* establishment); this one adds
# the lossy-audio gate (MACRDP_UDP_LOSSY_AUDIO=1), requires AAC, and watches the
# audio-DVC lifecycle markers + the A/B knob below.
#
# Two terminals:
#
#   # terminal 1 — shape the link (both directions, TCP+UDP on the port):
#   sudo scripts/netshape.sh on --loss 5 --delay 100
#
#   # terminal 2 — run the server under the lossy-AUDIO soak config:
#   scripts/soak-lossy-audio.sh --enable-udp-multitransport --enable-aac \
#       --enable-h264 --username "$USER" --password 'PASS' --bind 0.0.0.0:3390
#
#   # …connect mstsc from another machine, play steady audio (music/YouTube) on the
#   #   Mac, listen on the client, then:
#   sudo scripts/netshape.sh off
#
# --enable-aac AND --enable-h264 are REQUIRED: the lossy audio DVC negotiates AAC
# (MS-RDPEA needs v8 + AAC), and the Soft-Sync trigger rides the EGFX dispatch arm,
# so H.264 must be on for the migration to fire. Without --enable-aac the lossy
# audio DVC is never offered and audio simply stays on TCP (the control case).
#
# THE A/B (what makes this a soak, not just a smoke test):
#   * TREATMENT (default): MACRDP_UDP_LOSSY_AUDIO=1 → audio rides the lossy tunnel.
#       Expect: smooth audio under loss; the "streaming Wave2 audio over the LOSSY
#       UDP/DTLS tunnel" marker fires once; static rdpsnd goes silent.
#   * CONTROL: run with MACRDP_UDP_LOSSY_AUDIO=0 in front of this script → audio
#       stays on the static RDPSND TCP channel. Expect: audible gaps / desync that
#       worsen as --loss climbs. Same shaping, same content — only the transport
#       differs. That contrast IS the result.
#
# Override knobs (env):
#   MACRDP   — path to the binary (default: target/release/macrdp)
#   RUST_LOG — logging filter (default: the soak filter below)
#   SOAK_LOG — full-log output path (default: ./soak-lossy-audio-<timestamp>.log)
#   MACRDP_UDP_LOSSY_AUDIO    — 1 = treatment (default), 0 = control (audio on TCP)
#   MACRDP_UDP_LOSSY_DELIVERY — 1 = genuine send-once/no-retransmit lossy SM
#                               (default); 0 = lossy flow rides the reliable SM
#                               (retransmits — robust handshake, but the audio
#                               waves then HOL-block like TCP, so the win shrinks).
#                               NOTE the P2.2 caveat: in genuine lossy mode a
#                               dropped one-shot CREATERESPONSE/DTLS flight is not
#                               resent, so under heavy loss the tunnel may fail to
#                               establish at all — watch for "P2.4 GREEN" / the
#                               CREATERESPONSE marker to confirm it came up.
#   MACRDP_UDP_LOSSY_AUDIO_DUP — 1 = 1+1 redundancy: ship each lossy source
#                               datagram TWICE (same seq, byte-identical) so an
#                               independent-loss link costs us only at p². mstsc's
#                               DTLS anti-replay drops the duplicate (no double-
#                               play). The P2.3 FEC pivot (real Reed-Solomon FEC is
#                               structurally unavailable — modern Windows is
#                               RDPUDP2/no-FEC). Default 0 (off). Needs LOSSY
#                               delivery (it has no effect on the reliable SM).
#                               This is the second A/B axis: dup vs no-dup at a
#                               fixed loss to see if redundancy closes the gap.

set -eu

MACRDP="${MACRDP:-target/release/macrdp}"

if [ ! -x "$MACRDP" ]; then
    echo "error: macrdp binary not found / not executable at: $MACRDP" >&2
    echo "  build it first: cargo build --release" >&2
    echo "  (or set MACRDP=/path/to/macrdp)" >&2
    exit 1
fi

# Friendly nudge: the lossy audio DVC is only offered when --enable-aac is passed.
case " $* " in
    *" --enable-aac "*) ;;
    *)
        echo "warning: --enable-aac not in args — the lossy audio DVC will NOT be offered" >&2
        echo "         (audio will ride TCP; that's the CONTROL case, not the treatment)." >&2
        ;;
esac

# The lossy path is experimental + env-gated; the soak turns the gates on.
export MACRDP_UDP_OFFER_FECL=1
export MACRDP_UDP_LOSSY_DELIVERY="${MACRDP_UDP_LOSSY_DELIVERY:-1}"
export MACRDP_UDP_LOSSY_AUDIO="${MACRDP_UDP_LOSSY_AUDIO:-1}"
export MACRDP_UDP_LOSSY_AUDIO_DUP="${MACRDP_UDP_LOSSY_AUDIO_DUP:-0}"

# Enough to see the audio-DVC lifecycle + the tunnel/DTLS milestones + the
# reliability counters, without per-keystroke input spam. The audio-DVC "opened"
# line + the Soft-Sync send + the client's Soft-Sync Response are debug-level, so
# raise those targets; the GREEN/streaming markers are warn-level and always show.
export RUST_LOG="${RUST_LOG:-info,ironrdp_server::multitransport=debug,ironrdp_server::server=debug,ironrdp_dvc=debug,ironrdp_acceptor=info,macrdp::h264=info,macrdp::audio=info}"

ts="$(date +%Y%m%d-%H%M%S)"
SOAK_LOG="${SOAK_LOG:-./soak-lossy-audio-${ts}.log}"

# The lines worth watching during a lossy-audio soak, roughly in lifecycle order:
#   * the offer + reliable handshake (it must negotiate AAC over TCP first),
#   * the lossy DVC opening + the Soft-Sync onto UDPFECL + the client's response,
#   * the DTLS/tunnel establishment under loss (P2.1/P2.4 GREEN = it survived),
#   * THE payoff marker: "streaming Wave2 audio over the LOSSY UDP/DTLS tunnel",
#   * the failure tells (route failed / broken pipe / reset / cookie reject), and
#   * RTO retransmits (should be ~0 for a genuine lossy flow; any value here means
#     the reliable SM is still carrying it — check MACRDP_UDP_LOSSY_DELIVERY).
MARKERS='offering AUDIO_PLAYBACK_LOSSY_DVC|reliable audio DVC negotiated|lossy audio DVC opened|DYNVC_SOFT_SYNC_REQUEST|Soft-Sync Response|streaming Wave2 audio over the LOSSY|lossy audio wave route over UDP tunnel failed|LOSSY delivery|duplicate source datagrams|SPIKE GREEN|P2\.1 GREEN|P2\.4 GREEN|CREATEREQUEST|CREATERESPONSE|cookie not recognized|RTO retransmit|Broken pipe|Connection reset|Connection error'

echo "soak-lossy-audio: MACRDP_UDP_OFFER_FECL=$MACRDP_UDP_OFFER_FECL MACRDP_UDP_LOSSY_DELIVERY=$MACRDP_UDP_LOSSY_DELIVERY MACRDP_UDP_LOSSY_AUDIO=$MACRDP_UDP_LOSSY_AUDIO MACRDP_UDP_LOSSY_AUDIO_DUP=$MACRDP_UDP_LOSSY_AUDIO_DUP"
if [ "$MACRDP_UDP_LOSSY_AUDIO" = "1" ]; then
    echo "soak-lossy-audio: mode = TREATMENT (audio over the lossy UDP/DTLS tunnel)"
else
    echo "soak-lossy-audio: mode = CONTROL (audio on the static RDPSND TCP channel)"
fi
if [ "$MACRDP_UDP_LOSSY_AUDIO_DUP" = "1" ]; then
    echo "soak-lossy-audio: redundancy = ON (1+1 duplicate source datagrams; P2.3 FEC pivot)"
fi
echo "soak-lossy-audio: binary=$MACRDP"
echo "soak-lossy-audio: full log -> $SOAK_LOG (paste this if reporting)"
echo "soak-lossy-audio: live view below is filtered to soak markers; the file has everything."
echo "soak-lossy-audio: shape the link in another terminal: sudo scripts/netshape.sh on --loss 5 --delay 100"
echo "---"

# Full output to the file; live console filtered to the markers. `grep -E` is line
# buffered on a pipe under macOS, so lines appear as they happen.
"$MACRDP" "$@" 2>&1 | tee "$SOAK_LOG" | grep -E --line-buffered "$MARKERS" || true
