#!/bin/bash
# E8 rig: stop ONLY this panel's processes, by recorded pid, after re-checking
# what each pid actually is.
#
# ~/rtems-bringup/rigpid.sh is unsafe here as written: its rig_is_qemu
# hardcodes comm == "qemu-system-arm", so a VxWorks guest
# (qemu-system-x86_64) is silently skipped and the guests accumulate while the
# operator believes they were cleaned up.  This checks the comm it actually
# expects, per pidfile.  No pkill, ever -- the two long-running
# qemu-system-arm RTEMS guests on this box must survive.
set -u
D=$HOME/vx-rig-e8

kill_pidfile() {  # kill_pidfile <pidfile> <expected comm substring>
    local f="$1" want="$2" pid comm
    [ -f "$f" ] || { echo "no $f"; return 0; }
    pid=$(cat "$f")
    [ -n "$pid" ] || { echo "$f empty"; return 0; }
    if [ ! -d "/proc/$pid" ]; then
        echo "$f pid=$pid already gone"
        rm -f "$f"
        return 0
    fi
    comm=$(cat "/proc/$pid/comm" 2>/dev/null || echo "?")
    if [ "${comm#*"$want"}" != "$comm" ] || [ "$comm" = "$want" ]; then
        kill "$pid" 2>/dev/null && echo "killed $f pid=$pid comm=$comm"
        sleep 1
        [ -d "/proc/$pid" ] && { kill -9 "$pid" 2>/dev/null; echo "  SIGKILL $pid"; }
        rm -f "$f"
    else
        echo "REFUSING $f pid=$pid comm=$comm (expected $want) -- not ours"
    fi
}

kill_pidfile "$D/qemu.pid"      qemu-system-x86
kill_pidfile "$D/conbridge.pid" nc
kill_pidfile "$D/holder.pid"    sleep
kill_pidfile "$D/ftpd.pid"      python3
# ftpd forks a child per session; the parent's death does not take it (measured
# in the E10 round).  Kill only children of the recorded parent.
if [ -f "$D/ftpd.pid.dead" ]; then
    for c in $(cat "$D/ftpd.pid.dead"); do
        [ -d "/proc/$c" ] && kill -9 "$c" 2>/dev/null && echo "killed forked ftpd child $c"
    done
fi
echo "--- surviving qemu on this box (must still list 3308690 and 3309544) ---"
ps -eo pid,comm,etime | grep qemu-system | grep -v grep
