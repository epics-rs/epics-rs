#!/bin/bash
# stop-rtems-e10.sh — TERM (then KILL) only the qemu-system-arm pid that
# boot-rtems-e10.sh recorded, after re-reading /proc/<pid>/comm because pids
# are reused.  Two other panels' RTEMS guests run on this box and must
# survive; nothing here matches on a process NAME or a command-line pattern.
R=$HOME/vx-rig-e10
PIDF=$R/rtems-qemu.pid
[ -f "$PIDF" ] || exit 0
while read -r p; do
    [ -n "$p" ] || continue
    [ -r "/proc/$p/comm" ] || continue
    [ "$(cat "/proc/$p/comm")" = "qemu-system-arm" ] || continue
    kill "$p" 2>/dev/null && echo "stop-rtems: TERM own qemu pid $p"
done < "$PIDF"
sleep 3
while read -r p; do
    [ -n "$p" ] || continue
    [ -r "/proc/$p/comm" ] || continue
    [ "$(cat "/proc/$p/comm")" = "qemu-system-arm" ] || continue
    kill -9 "$p" 2>/dev/null && echo "stop-rtems: KILL own qemu pid $p"
done < "$PIDF"
rm -f "$PIDF"
