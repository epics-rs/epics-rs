#!/bin/bash
# rtemsladder-e10.sh <ram>... — the armv7-rtems thread/heap ladder.
#
# At the rig's default 256M the wall is NOT memory: 133 ramp connections plus 8
# monitor connections is 141, the `CAS_CLIENT_POOL_CAPACITY` constant, and the
# server refuses politely with ~9 MB of heap still free.  So the pool cap and
# the heap wall very nearly coincide at 256M, and which one binds is decided by
# a single client.  To find the real memory wall — and to see what the IOC does
# when it hits one — the ladder has to run arms where the heap runs out FIRST,
# i.e. below 256M, and one arm above it to confirm the cap binds cleanly when
# memory is ample.
#
# One guest at a time, own pidfile, own ports.  The two protected
# qemu-system-arm guests on this box are never signalled.
set -o pipefail
R=$HOME/vx-rig-e10
IMG=${RTEMSIMG:-caioc-e10}

for RAM in "$@"; do
    LOG=$R/logs/rtems-$IMG-$RAM.log
    "$R/stop-rtems-e10.sh" > /dev/null 2>&1
    RTEMSRAM=$RAM RTEMSTAG=$RAM bash "$R/boot-rtems-e10.sh" "$IMG" > /dev/null 2>&1

    READY=no
    for _ in $(seq 150); do
        grep -q "serving .* records on CA port" "$LOG" 2>/dev/null && { READY=yes; break; }
        sleep 2
    done
    if [ "$READY" != yes ]; then
        echo "ram=$RAM NOT-READY console=$(wc -c < "$LOG" 2>/dev/null)B last=$(tail -1 "$LOG" 2>/dev/null)"
        cp "$LOG" "$R/logs/ladder-$RAM.console.log" 2>/dev/null
        "$R/stop-rtems-e10.sh" > /dev/null 2>&1
        continue
    fi

    timeout 600 python3 "$R/rtemsramp-e10.py" 400 "r$RAM" 480 0.10 \
        > "$R/logs/ladder-$RAM.probe.log" 2>&1
    cp "$LOG" "$R/logs/ladder-$RAM.console.log"

    echo "=== ram=$RAM ==="
    grep -E "SAMPLE held=0 " "$R/logs/ladder-$RAM.probe.log" | head -1
    grep -E "WALL|per-client|DEADLINE" "$R/logs/ladder-$RAM.probe.log"
    grep -E "TOP held=" "$R/logs/ladder-$RAM.probe.log" | head -1
    # An IOC that died rather than refused says so on its own console.
    grep -E "FATAL|panic|memory allocation of|abort" "$R/logs/ladder-$RAM.console.log" | tail -3
    "$R/stop-rtems-e10.sh" > /dev/null 2>&1
done
