#!/bin/sh
# netshape.sh — apply / clear an emulated lossy, high-latency network condition on
# the macrdp listening port, for soak-testing RDP UDP multitransport (EGFX over
# the reliable RDPEUDP tunnel) against a real client.
#
# It shapes BOTH directions of BOTH protocols (TCP + UDP) on the chosen port using
# macOS's built-in pf + dnctl/dummynet — no extra software. Shaping the Mac side
# is enough: every packet between the RDP client and macrdp crosses this host.
#
# WHY this is the test that matters: in a pure-TCP session every channel (video,
# input, audio, clipboard) multiplexes onto ONE TCP stream, so a single dropped
# segment head-of-line-blocks *everything* until it's retransmitted. With EGFX
# migrated onto UDP (--udp-migrate-egfx), video rides its own reliable
# channel: loss on the control TCP no longer freezes the picture, and a lost video
# packet only delays that packet. This script lets you see that difference.
#
# Requires sudo (pf + dnctl are privileged). Review before running.
#
#   sudo scripts/netshape.sh on  [--loss PCT] [--delay MS] [--bw RATE] [--port N]
#   sudo scripts/netshape.sh off
#   scripts/netshape.sh status
#
# Examples:
#   sudo scripts/netshape.sh on --loss 5 --delay 100     # 5% loss, +100ms each way
#   sudo scripts/netshape.sh on --loss 2 --delay 60 --bw 20Mbit/s
#   sudo scripts/netshape.sh off                         # restore the network
#
# NOTE: --delay is per-pipe and applied to EACH direction, so round-trip latency
# rises by ~2x the value. --loss is per-packet drop probability per direction.
#
# A note on scope: this enables pf for the duration of the test. On a stock Mac pf
# is idle/disabled by default; `off` flushes our anchor + pipes and restores the
# default ruleset. If you run a custom firewall, review the `off` path first.

set -eu

ANCHOR="macrdp-netshape"
PIPE_IN=1
PIPE_OUT=2

PORT=3390
LOSS=0      # percent
DELAY=0     # ms, each direction
BW=0        # 0 = unlimited; else e.g. 20Mbit/s, 1500Kbit/s

usage() {
    sed -n '2,40p' "$0"
    exit "${1:-1}"
}

require_root() {
    if [ "$(id -u)" != "0" ]; then
        echo "error: must run as root (use sudo)" >&2
        exit 1
    fi
}

cmd="${1:-}"
[ -n "$cmd" ] || usage 1
shift || true

# Parse flags (only meaningful for `on`).
while [ $# -gt 0 ]; do
    case "$1" in
        --loss)  LOSS="$2"; shift 2 ;;
        --delay) DELAY="$2"; shift 2 ;;
        --bw)    BW="$2"; shift 2 ;;
        --port)  PORT="$2"; shift 2 ;;
        -h|--help) usage 0 ;;
        *) echo "unknown arg: $1" >&2; usage 1 ;;
    esac
done

# dummynet wants a 0..1 probability for packet-loss-rate.
plr() { awk "BEGIN { printf \"%.4f\", $LOSS / 100 }"; }

bw_arg() {
    if [ "$BW" = "0" ]; then echo ""; else echo "bw $BW"; fi
}

case "$cmd" in
    on)
        require_root
        echo "shaping port $PORT: loss=${LOSS}% delay=${DELAY}ms/dir bw=${BW}"

        # Two pipes (in / out) so each direction is shaped independently.
        # shellcheck disable=SC2046
        dnctl pipe "$PIPE_IN"  config $(bw_arg) delay "$DELAY" plr "$(plr)"
        # shellcheck disable=SC2046
        dnctl pipe "$PIPE_OUT" config $(bw_arg) delay "$DELAY" plr "$(plr)"

        # Load the default ruleset plus our dummynet anchor declarations, then
        # populate the anchor with the per-port classification rules.
        {
            cat /etc/pf.conf 2>/dev/null || true
            echo "dummynet-anchor \"$ANCHOR\""
            echo "anchor \"$ANCHOR\""
        } | pfctl -q -f -

        # Direction matters: client→server packets have DESTINATION port $PORT
        # (match them on the in pass), server→client packets have SOURCE port
        # $PORT (match them on the out pass). The original version used
        # `to any port` on BOTH rules, which (a) never matched the
        # server→client direction at all — video/audio data was NEVER shaped —
        # and (b) piped client→server twice on loopback (in + out pass),
        # doubling the configured delay. Found 2026-07-04 while lab-reproducing
        # the VPN video issue: the "500 Kbit" pipe carried 6 Mbps of video.
        echo "dummynet in  quick proto { tcp udp } from any to any port $PORT pipe $PIPE_IN
dummynet out quick proto { tcp udp } from any port $PORT to any pipe $PIPE_OUT" \
            | pfctl -q -a "$ANCHOR" -f -

        pfctl -q -E 2>/dev/null || pfctl -q -e 2>/dev/null || true
        echo "done. clear with: sudo $0 off"
        ;;

    off)
        require_root
        echo "clearing shaping on anchor $ANCHOR"
        pfctl -q -a "$ANCHOR" -F all 2>/dev/null || true
        dnctl -q pipe "$PIPE_IN"  delete 2>/dev/null || true
        dnctl -q pipe "$PIPE_OUT" delete 2>/dev/null || true
        # Restore the stock ruleset (drops our anchor declarations).
        pfctl -q -f /etc/pf.conf 2>/dev/null || true
        # Leave pf as we found it on a stock Mac (idle). If you rely on pf, re-enable.
        pfctl -q -d 2>/dev/null || true
        echo "done."
        ;;

    status)
        echo "=== dnctl pipes ==="
        dnctl -q list 2>/dev/null || dnctl list 2>/dev/null || echo "(none / need sudo)"
        echo "=== pf anchor $ANCHOR ==="
        pfctl -q -a "$ANCHOR" -s rules 2>/dev/null || echo "(none / need sudo)"
        ;;

    *)
        usage 1
        ;;
esac
