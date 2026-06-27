#!/bin/sh
# fec-scan.sh — scan an RDP UDP capture for RDPEUDP **FEC packets** and report the
# negotiated transport version, the P2.3 go/no-go spike (see
# docs/rdp-udp-multitransport-feasibility.md → "P2.3 FEC capture runbook").
#
# The question: does a *real Windows RDP server* emit FEC (parity) packets on a UDP
# flow? If YES, FEC is worth implementing and the capture gives us the source+parity
# bytes to reverse-engineer the (undocumented) GF(256) coefficient table. If NO,
# don't build the encoder.
#
# IMPORTANT: this uses **Wireshark's RDPUDP dissector** (authoritative) — NOT a
# hand-rolled header parse. An earlier hand-parser read uFlags at a fixed offset and
# misread RDPUDP2 (v3) data packets as FEC (the FEC bit isn't where v1 puts it),
# producing false "GO"s. The dissector knows the per-version framing.
#
# Decisive fact baked into the verdict: FEC exists ONLY in RDPEUDP v1/v2. The
# second-generation protocol **RDPUDP2 (version 0x0101, Wireshark "UDPv2")** has NO
# FEC at all — [MS-RDPEUDP2] (Overview) states RDPUDP2 is reliable-only ("does not
# support 'Best-Efforts' mode or RDP-UDP-L, [so] it does not include a forward error
# correction (FEC) mechanism"; loss is recovered by retransmission), and modern
# mstsc/Windows negotiate it. So a capture that shows version 0x0101 is a structural
# NO-GO regardless of loss.
#
#   scripts/fec-scan.sh <capture.pcap> [udp_port]
#
# udp_port: the UDP port carrying RDP (multitransport rides the TCP RDP port). Auto-
# detected if omitted — needed e.g. for a VirtualBox NAT forward where the host side
# is 3390 forwarding to the guest's 3389.
#
# Requires tshark (Wireshark CLI): brew install wireshark
set -eu

PCAP="${1:-}"
PORT_ARG="${2:-}"
if [ -z "$PCAP" ] || [ ! -f "$PCAP" ]; then
    echo "usage: $0 <capture.pcap> [udp_port]" >&2
    exit 1
fi
if ! command -v tshark >/dev/null 2>&1; then
    echo "error: tshark (Wireshark CLI) not found — install with: brew install wireshark" >&2
    exit 1
fi

# Resolve how to make tshark dissect the RDP UDP as rdpudp. Prefer the heuristic /
# default port; else force-decode an explicit or auto-detected port.
rdpudp_count() { tshark -r "$PCAP" $1 -Y 'rdpudp' 2>/dev/null | grep -c . || true; }

DECODE=""
if [ -n "$PORT_ARG" ]; then
    DECODE="-d udp.port==$PORT_ARG,rdpudp"
elif [ "$(rdpudp_count "")" -gt 0 ]; then
    DECODE=""   # heuristic already dissects it (typically port 3389)
else
    # Try the busiest UDP port (covers NAT-forwarded captures, e.g. 3390).
    CAND="$(tshark -r "$PCAP" -Y 'udp' -T fields -e udp.dstport -e udp.srcport 2>/dev/null \
        | tr '\t' '\n' | grep -E '^[0-9]+$' | sort | uniq -c | sort -rn | awk 'NR==1{print $2}')"
    if [ -n "${CAND:-}" ] && [ "$(rdpudp_count "-d udp.port==$CAND,rdpudp")" -gt 0 ]; then
        DECODE="-d udp.port==$CAND,rdpudp"
        echo "(auto-detected RDP UDP on port $CAND)"
    fi
fi

TOTAL="$(rdpudp_count "$DECODE")"
echo "=== RDPUDP datagrams: $TOTAL ==="

if [ "$TOTAL" = "0" ]; then
    echo
    echo ">>> INCONCLUSIVE: no RDPUDP traffic dissected."
    echo ">>> NOT a NO-GO — the capture doesn't contain the RDP UDP session."
    echo ">>>   * pass the port explicitly:  $0 $PCAP <udp_port>"
    echo ">>>   * sanity: tshark -r $PCAP -q -z io,phs   (is there udp at all?)"
    echo ">>>   * VirtualBox host<->VM RDP rides the VM's adapter, not the physical"
    echo ">>>     LAN; NAT usually breaks RDP UDP — use Bridged/Host-Only, or capture"
    echo ">>>     inside the VM."
    exit 0
fi

# Negotiated version (from the SYNEX). 0x0001/0x0002 = FEC-capable v1/v2;
# 0x0101 = RDPUDP2 ("UDPv2") which has NO FEC.
VERSIONS="$(tshark -r "$PCAP" $DECODE -Y 'rdpudp.flags.synex==1' -T fields -e rdpudp.synex.version 2>/dev/null | sort -u | tr '\n' ' ')"
LOSSY="$(tshark -r "$PCAP" $DECODE -Y 'rdpudp.flags.synlossy==1' 2>/dev/null | grep -c . || true)"
FEC="$(tshark -r "$PCAP" $DECODE -Y 'rdpudp.flags.fec==1' 2>/dev/null | grep -c . || true)"
DATA="$(tshark -r "$PCAP" $DECODE -Y 'rdpudp.flags.data==1' 2>/dev/null | grep -c . || true)"
ACK="$(tshark -r "$PCAP" $DECODE -Y 'rdpudp.flags.ack==1' 2>/dev/null | grep -c . || true)"

echo "  negotiated version(s): ${VERSIONS:-<none>}   (0x0001/0x0002 = FEC-capable; 0x0101 = RDPUDP2 = NO FEC)"
echo "  data=$DATA  ack=$ACK  syn-lossy flows=$LOSSY  FEC packets=$FEC"
echo

if [ "$FEC" -gt 0 ]; then
    echo ">>> GO: $FEC FEC datagram(s) found. Extract the coefficient table:"
    tshark -r "$PCAP" $DECODE -Y 'rdpudp.flags.fec==1' \
        -T fields -e frame.number -e udp.srcport -e udp.dstport \
        -e rdpudp.fec.coded -e rdpudp.fec.sourcestart -e rdpudp.fec.range -e rdpudp.fec.fecindex \
        2>/dev/null | head -40 \
        | awk -F'\t' '{printf "  frame %-6s %s->%s  snCoded=%s snSourceStart=%s range=%s fecIndex=%s\n",$1,$2,$3,$4,$5,$6,$7}'
    echo ">>> Paste this pcap back to work out the GF(256) coefficients from the"
    echo ">>> covered source packets + these parity payloads."
    exit 0
fi

# No FEC. Distinguish the structural NO-GO (RDPUDP2) from a clean-link/v1 case.
case " $VERSIONS " in
    *" 0x0101 "*)
        echo ">>> NO-GO (structural): the flow negotiated RDPUDP2 (UDPv2, 0x0101),"
        echo ">>> which has NO FEC mechanism. A real Windows server/mstsc will never"
        echo ">>> use FEC on this version, with or without loss. Don't build the FEC"
        echo ">>> encoder — pivot to the ARQ path." ;;
    *)
        echo ">>> NO-GO (this capture): RDPEUDP v1/v2 but zero FEC packets. Either the"
        echo ">>> link was clean (no loss to recover) or the server chose retransmit."
        echo ">>> Recapture under induced loss before concluding; if still no FEC, the"
        echo ">>> server doesn't use it here." ;;
esac
