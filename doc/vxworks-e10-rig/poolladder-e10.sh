#!/bin/bash
# poolladder-e10.sh <ram>... — the kernel's free pool, with NO RTP loaded, at
# each guest RAM.  This is the number the 768M/832M non-boot has to be measured
# against: the loader has to find room for the image's segments in that pool,
# and the image is a fixed 35,736,400 bytes whatever the guest RAM is.
set -o pipefail
R=$HOME/vx-rig-e10
IMG=${VXIMG:-ca-e11}
CONSOLE=$R/logs/console-$IMG.log

for RAM in "$@"; do
    "$R/stop-e10.sh" > /dev/null 2>&1
    rm -f "$CONSOLE"
    VXNOLAUNCH=1 VXRAM=$RAM nohup "$R/boot-e10.sh" "$IMG" 90 \
        > "$R/logs/pool-$RAM.boot.log" 2>&1 &
    BOOTPID=$!
    for _ in $(seq 90); do [ -p "$R/con.in" ] && break; sleep 1; done
    sleep 35
    { echo ""; sleep 3; echo "memShow"; sleep 12; } > "$R/con.in"
    sleep 6
    echo "=== ram=$RAM ==="
    grep -E "OS Memory Size|^ free |^ alloc " "$CONSOLE"
    cp "$CONSOLE" "$R/logs/pool-$RAM.console.log"
    "$R/stop-e10.sh" > /dev/null 2>&1
    kill $BOOTPID 2>/dev/null
done
