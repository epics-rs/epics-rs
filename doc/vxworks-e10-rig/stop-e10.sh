#!/bin/bash
# stop-e10.sh — kill ONLY the pids this rig recorded, each re-checked by comm.
#
# gv100 is shared: two long-lived qemu-system-arm RTEMS guests belonging to
# another arm must survive, and two other panels run their own VxWorks guests.
# So there is no name-based kill here of any kind, and no pkill/killall.
R=$HOME/vx-rig-e10

kill_if() {  # <pidfile> <expected comm>
    local f=$1 want=$2 pid comm
    [ -f "$f" ] || return 0
    pid=$(cat "$f")
    if [ -n "$pid" ] && [ -r "/proc/$pid/comm" ]; then
        comm=$(cat "/proc/$pid/comm")
        if [ "$comm" = "$want" ]; then
            kill "$pid" 2>/dev/null && echo "killed $pid ($comm)"
            sleep 1
            [ -d "/proc/$pid" ] && { kill -9 "$pid" 2>/dev/null; echo "kill -9 $pid"; }
        else
            echo "SKIP $pid: comm=$comm, expected $want"
        fi
    fi
    rm -f "$f"
}

kill_if "$R/qemu.pid"   qemu-system-x86   # /proc/<pid>/comm truncates at 15 chars
kill_if "$R/nc.pid"     nc
kill_if "$R/holder.pid" sleep

# The ftpd forks a child per session; the parent kill does not take it (measured
# in the prior round, where the forked child survived), so children are
# collected by PARENT PID — never by name.
if [ -f "$R/ftpd.pid" ]; then
    P=$(cat "$R/ftpd.pid")
    for c in $(pgrep -P "$P" 2>/dev/null); do
        kill "$c" 2>/dev/null && echo "killed ftpd child $c"
    done
    if [ -r "/proc/$P/comm" ] && [ "$(cat /proc/$P/comm)" = "python3" ]; then
        kill "$P" 2>/dev/null && echo "killed ftpd $P"
    fi
    rm -f "$R/ftpd.pid"
fi
rm -f "$R/con.in"
