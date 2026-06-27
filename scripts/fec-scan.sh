#!/bin/sh
# fec-scan.sh — scan an RDP UDP capture for RDPEUDP **FEC packets**, the P2.3
# go/no-go spike (see docs/rdp-udp-multitransport-feasibility.md → "P2.3 FEC
# capture runbook").
#
# The question this answers: does a *real Windows RDP server* actually emit FEC
# (parity) packets to a client on a lossy UDP flow? If YES, FEC is worth
# implementing and the capture also gives us the source+parity bytes to reverse-
# engineer the (undocumented) GF(256) coefficient table. If NO — only DATA/ACK and
# retransmits, no FEC datagrams over a lossy link — then a Windows server doesn't
# use FEC here either, so we don't build the encoder (MS-RDPEUDP makes FEC decode
# OPTIONAL and uses retransmission as the real reliability mechanism).
#
#   scripts/fec-scan.sh <capture.pcap>
#
# How it reads the wire: every RDPUDP datagram begins with RDPUDP_FEC_HEADER
# (8 bytes, big-endian): snSourceAck u32, uReceiveWindowSize u16, uFlags u16. The
# FEC bit is 0x10 (RDPUDP_FLAG_FEC); an FEC packet also sets DATA (0x08) + ACK
# (0x04) → uFlags 0x1C. We classify each UDP payload by uFlags, print a flag
# histogram, and dump every FEC datagram's full hex for offline analysis.
#
# Requires tshark (Wireshark CLI): brew install wireshark
set -eu

PCAP="${1:-}"
if [ -z "$PCAP" ] || [ ! -f "$PCAP" ]; then
    echo "usage: $0 <capture.pcap>" >&2
    exit 1
fi
if ! command -v tshark >/dev/null 2>&1; then
    echo "error: tshark (Wireshark CLI) not found — install with: brew install wireshark" >&2
    exit 1
fi

# Dump per-UDP-datagram: frame number, src→dst, and the raw UDP payload hex. We do
# NOT rely on Wireshark's rdpudp dissector (field names vary by version) — we parse
# the RDPUDP header ourselves from the payload bytes, which is version-stable.
#
# tshark output goes to a temp file passed to python by path: `python3 - <<heredoc`
# makes the heredoc python's *stdin*, so a piped `tshark | python3 -` would leave
# sys.stdin pointing at the program text, not the data — the data would be lost.
TMPOUT="$(mktemp)"
trap 'rm -f "$TMPOUT"' EXIT
tshark -r "$PCAP" -Y 'udp && udp.payload' \
    -T fields -e frame.number -e ip.src -e ip.dst -e udp.srcport -e udp.dstport -e udp.payload \
    2>/dev/null > "$TMPOUT"

python3 - "$TMPOUT" <<'PY'
import sys

SYN, FIN, ACK, DATA, FEC = 0x01, 0x02, 0x04, 0x08, 0x10
# Higher RDPUDP flag bits seen in the wild (SYNEX/ACK_OF_ACKS/CORRELATIONID/CWR/etc.)
NAMES = [(SYN,"SYN"),(FIN,"FIN"),(ACK,"ACK"),(DATA,"DATA"),(FEC,"FEC"),
         (0x20,"SYNLOSSY"),(0x40,"ACKDELAYED"),(0x80,"CORRID"),(0x100,"SYNEX")]

def flagstr(f):
    parts = [n for b, n in NAMES if f & b]
    extra = f & ~sum(b for b, _ in NAMES)
    if extra:
        parts.append("0x%x" % extra)
    return "|".join(parts) if parts else "0"

total = 0
hist = {}
fec = []

with open(sys.argv[1]) as _f:
    lines = _f.readlines()

for line in lines:
    cols = line.rstrip("\n").split("\t")
    if len(cols) < 6:
        continue
    frame, src, dst, sport, dport, payhex = cols[:6]
    payhex = payhex.replace(":", "").strip()
    if len(payhex) < 16:  # need at least the 8-byte FEC_HEADER
        continue
    try:
        pay = bytes.fromhex(payhex)
    except ValueError:
        continue
    uflags = int.from_bytes(pay[6:8], "big")
    sn_source_ack = int.from_bytes(pay[0:4], "big")
    total += 1
    hist[uflags] = hist.get(uflags, 0) + 1
    if uflags & FEC:
        fec.append((frame, src, sport, dst, dport, sn_source_ack, uflags, payhex))

print("=== RDPUDP datagrams: %d ===" % total)

if total == 0:
    print()
    print(">>> INCONCLUSIVE: no RDP UDP datagrams in this capture.")
    print(">>> This is NOT a NO-GO — the capture didn't contain the traffic we need.")
    print(">>> Likely causes: empty/failed capture (e.g. `tcpdump -i any` fails on")
    print(">>>   macOS — use the real interface), RDP ran TCP-only (the client never")
    print(">>>   opened a UDP flow), or RDP UDP used a different port. Recapture:")
    print(">>>   1) find your LAN interface:  ifconfig | grep -B5 'inet 192\\.168'")
    print(">>>   2) capture broadly:  sudo tcpdump -i <iface> -w cap.pcap host <server-ip>")
    print(">>>   3) verify it grows (ls -l cap.pcap > 24 bytes) while connected,")
    print(">>>      then confirm UDP appears:  tshark -r cap.pcap -Y 'udp' | head")
    sys.exit(0)

print("--- uFlags histogram (count  flags  =hex) ---")
for f in sorted(hist, key=lambda k: -hist[k]):
    print("  %6d  %-28s =0x%04x" % (hist[f], flagstr(f), f))

print()
if not fec:
    print(">>> NO FEC datagrams found (uFlags & 0x10 never set).")
    print(">>> go/no-go = NO-GO: the server is not sending FEC on this flow.")
    print(">>>   (Expected if it negotiated RDPEUDP2/v3, or chose retransmit-only.)")
    print(">>>   Conclusion: don't build the FEC encoder; the ARQ path is the lever.")
else:
    print(">>> FOUND %d FEC datagram(s) — go/no-go = GO." % len(fec))
    print(">>> Paste this pcap back so the GF(256) coefficient table can be worked")
    print(">>> out from the covered source packets + these parity bytes.")
    print("--- FEC datagrams (frame  src:port -> dst:port  snSourceAck  uFlags) ---")
    for frame, src, sport, dst, dport, sa, uf, payhex in fec[:50]:
        print("  frame %s  %s:%s -> %s:%s  snSourceAck=%d  uFlags=0x%04x" %
              (frame, src, sport, dst, dport, sa, uf))
        print("    payload: %s" % payhex)
    if len(fec) > 50:
        print("  … and %d more (showing first 50)" % (len(fec) - 50))
PY
