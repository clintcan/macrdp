#!/bin/sh
# soak-monitor.sh — sample + analyze macrdp resource usage over a long-running
# (Tier 2.4) soak, to catch the things a multi-day run is *for*: memory / fd /
# thread creep, leaked SCStreams or NFS mounts, runaway log growth, and crash /
# resync red flags. See docs/production-readiness-roadmap.md (Tier 2.4).
#
# The point of the sampler: leaks are INVISIBLE after the fact unless you record
# the trend WHILE the soak runs. Start `monitor` on the soak machine at (or any
# time during) the run; analyze the CSV + rotated logs afterward.
#
# Usage:
#   scripts/soak-monitor.sh monitor [--interval SECS] [--out FILE] [--pid PID]
#       Sample every SECS (default 60) until the process exits, appending a CSV
#       row each tick. Default target = the newest running macrdp; pass --pid to
#       pin one (under --fork-workers, point it at the long-lived SUPERVISOR; the
#       `procs` column also tracks worker accumulation).
#
#   scripts/soak-monitor.sh analyze [--csv FILE] [--logs GLOB]
#       Summarize the CSV trend (first/last/min/max/delta of each metric) and
#       grep the logs (default ~/Library/Logs/macrdp.log*) for red flags.
#
# macOS-only (uses ps -M / lsof / stat -f). No deps beyond the base system.

set -u

DEFAULT_LOGS="$HOME/Library/Logs/macrdp.log"

# Match the macrdp SERVER binary exactly — `macrdp` followed by a space (args) or
# end of the argv — so it does NOT match `macrdptray` (the controller) or
# `macrdphud` (the switcher helper), of which `macrdp` is a prefix.
MACRDP_RE='MacOS/macrdp( |$)|target/(release|debug)/macrdp( |$)'

find_pid() {
    pgrep -n -f "$MACRDP_RE" 2>/dev/null
}

sample_row() {
    # $1 = pid. Echo one CSV row, or nothing if the pid is gone.
    pid="$1"
    kill -0 "$pid" 2>/dev/null || return 1
    rss=$(ps -o rss= -p "$pid" 2>/dev/null | awk '{print int($1/1024)}')
    thr=$(ps -M "$pid" 2>/dev/null | tail -n +2 | wc -l | tr -d ' ')
    fds=$(lsof -p "$pid" 2>/dev/null | tail -n +2 | wc -l | tr -d ' ')
    scs=$(lsof -p "$pid" 2>/dev/null | grep -ci 'ScreenCaptureKit\|CoreMedia')
    procs=$(pgrep -f "$MACRDP_RE" 2>/dev/null | wc -l | tr -d ' ')
    nfs=$(mount 2>/dev/null | grep -c 'macrdp-rdpdr\|localhost:/')
    tmp=$(ls -d "${TMPDIR:-/tmp/}"macrdp-* /tmp/macrdp-* 2>/dev/null | wc -l | tr -d ' ')
    logmb=$(stat -f%z "$DEFAULT_LOGS" 2>/dev/null | awk '{print int($1/1048576)}')
    echo "$(date '+%Y-%m-%dT%H:%M:%S'),${rss:-0},${thr:-0},${fds:-0},${scs:-0},${procs:-0},${nfs:-0},${tmp:-0},${logmb:-0}"
}

cmd_monitor() {
    interval=60
    out="$HOME/macrdp-soak-$(date '+%Y%m%d-%H%M%S').csv"
    pid=""
    while [ $# -gt 0 ]; do
        case "$1" in
            --interval) interval="$2"; shift 2 ;;
            --out) out="$2"; shift 2 ;;
            --pid) pid="$2"; shift 2 ;;
            *) echo "unknown arg: $1" >&2; exit 2 ;;
        esac
    done
    [ -n "$pid" ] || pid="$(find_pid)"
    if [ -z "$pid" ]; then
        echo "no running macrdp process found (pass --pid)" >&2
        exit 1
    fi
    echo "monitoring pid $pid every ${interval}s -> $out" >&2
    echo "ts,rss_mb,threads,fds,scstreams,procs,nfs_mounts,tmp_dirs,log_mb" > "$out"
    while row="$(sample_row "$pid")"; do
        echo "$row" >> "$out"
        echo "$row"
        sleep "$interval"
    done
    echo "pid $pid exited; samples in $out" >&2
}

# Print "first last min max delta" for a 1-based CSV column index $2 of file $1.
col_stats() {
    awk -F, -v c="$2" 'NR>1 && $c!="" {
        v=$c+0
        if (n==0){first=v;min=v;max=v}
        if (v<min)min=v; if (v>max)max=v
        last=v; n++
    } END {
        if(n==0){print "n/a"; exit}
        printf "first=%-8s last=%-8s min=%-8s max=%-8s delta=%+d\n", first, last, min, max, last-first
    }' "$1"
}

cmd_analyze() {
    csv=""
    logs="${DEFAULT_LOGS}*"
    while [ $# -gt 0 ]; do
        case "$1" in
            --csv) csv="$2"; shift 2 ;;
            --logs) logs="$2"; shift 2 ;;
            *) echo "unknown arg: $1" >&2; exit 2 ;;
        esac
    done
    # Newest soak CSV if not given.
    [ -n "$csv" ] || csv="$(ls -t "$HOME"/macrdp-soak-*.csv 2>/dev/null | head -1)"

    if [ -n "$csv" ] && [ -f "$csv" ]; then
        rows=$(( $(wc -l < "$csv") - 1 ))
        echo "==> resource trend ($csv, $rows samples)"
        echo "  rss_mb     : $(col_stats "$csv" 2)"
        echo "  threads    : $(col_stats "$csv" 3)"
        echo "  fds        : $(col_stats "$csv" 4)"
        echo "  scstreams  : $(col_stats "$csv" 5)"
        echo "  procs      : $(col_stats "$csv" 6)"
        echo "  nfs_mounts : $(col_stats "$csv" 7)"
        echo "  tmp_dirs   : $(col_stats "$csv" 8)"
        echo "  log_mb     : $(col_stats "$csv" 9)"
        echo "  (a steadily-climbing rss/fds/threads delta = a leak; the rest should return to baseline)"
    else
        echo "==> no soak CSV found (run 'monitor' next time, or pass --csv)"
    fi

    # Strip ANSI color (interactive logs) + the leading timestamp so identical
    # messages dedup cleanly. perl is always present on macOS.
    strip() { perl -pe 's/\e\[[0-9;]*m//g' | sed -E 's/^[0-9][0-9T:.+Z-]* +//'; }

    echo
    echo "==> CRITICAL log hits (these should be ZERO over a clean soak): $logs"
    # shellcheck disable=SC2086
    n=$(grep -hiE 'panic|fatal|SIGSEGV|SIGABRT|abort trap|objc_exception|assertion failed' $logs 2>/dev/null \
        | strip | sort | uniq -c | sort -rn | tee /dev/stderr | wc -l | tr -d ' ')
    [ "$n" -eq 0 ] && echo "  none — good"

    echo
    echo "==> WATCH log events (occasional is fine; a steady per-hour CLIMB is the concern):"
    # shellcheck disable=SC2086
    grep -hiE 'writer stalled|self-heal|de-migrat|watchdog|resync|too many open files|EMFILE' $logs 2>/dev/null \
        | strip | sort | uniq -c | sort -rn | head -20
    echo
    echo "  tip: re-run analyze later and compare WATCH counts — flat/slow = healthy,"
    echo "  growing fast = a real issue. Any CRITICAL hit means dig into that log."
}

case "${1:-}" in
    monitor) shift; cmd_monitor "$@" ;;
    analyze) shift; cmd_analyze "$@" ;;
    *) sed -n '2,30p' "$0"; exit 2 ;;
esac
