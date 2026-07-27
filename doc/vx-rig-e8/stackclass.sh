#!/bin/bash
# E8 rig: ONE point of the CAS-client declared-stack sweep, end to end.
#
# The question this answers is e10-residue's: does the CA admission wall move
# *linearly with the declared stack size* of a client's threads, or only with
# the client count?  Only one input may vary between points, so the whole run --
# edit, build, boot, ramp, stop -- is a script rather than a sequence of
# hand-typed steps.  What varies is the `stack:` class of a role in
# `client_roster()` (crates/epics-ca-rs/src/server/blocking.rs); the guest RAM,
# the probe and the ramp arguments are held fixed by this file.  Results and the
# fit: doc/vxworks-ca-admission-wall-vs-declared-stack.md.
#
# The class is confirmed twice, not once: `rg` on the source before the build,
# and `STACKUSE ... name=CAS-client 0 size=<bytes>` printed by the image's own
# C6 census after the ramp.  A build that silently kept the old class is the
# one failure mode that would make the regression meaningless.
#
# The edit is a working-tree edit that is NEVER committed -- restore it with
# `./stackclass.sh restore`.  Restoration is `cp` from the backup taken on the
# first run, not `git checkout`: the rig tree carries other rounds' uncommitted
# probe edits and a checkout would take them with it.
#
# RIG DISCIPLINE: boots through ./boot-e8.sh and stops through ./stop-e8.sh,
# both of which act on recorded pids only.  No pkill, ever -- this box also
# runs two long-lived qemu-system-arm RTEMS guests that must survive.
#
# Usage:  ./stackclass.sh <Small|Medium|Big> [MEM] [TAG] [EVENTCLASS] [wall|census]
#         ./stackclass.sh restore
set -u
D=$HOME/vx-rig-e8
WT=$D/wt
F=$WT/crates/epics-ca-rs/src/server/blocking.rs
BK=$D/blocking.rs.orig-stackclass

if [ "${1:-}" = "restore" ]; then
    [ -f "$BK" ] || { echo "no backup at $BK"; exit 1; }
    cp "$BK" "$F"
    echo "restored $F from $BK"
    rg -n -A2 'suffix: "client"' "$F"
    exit 0
fi

CLASS=${1:?usage: $0 <Small|Medium|Big> [MEM] [TAG] [EVENTCLASS] | $0 restore}
MEM=${2:-1024M}
TAG=${3:-sc-$CLASS}
# The `event` role is held at Medium for the sweep proper.  It is settable only
# for the discriminator run that asks whether the wall follows the *client*
# thread's class or the total declared bytes of the set -- two models that the
# client-only sweep cannot separate.
ECLASS=${4:-Medium}
# `census` holds a load well below the wall long enough for the image's own C6
# pass to print the declared sizes of a LIVE worker set.  The wall runs cannot
# always supply that themselves: the run that dies at the wall has no live task
# left to enumerate, and the one that wedges reports `state=gone` for every
# registry entry by the time the census fires.  A separate boot keeps the wall
# numbers exactly as measured instead of perturbing them to get the proof.
MODE=${5:-wall}
case "$MODE" in
    wall)   RAMP_CEILING=200; RAMP_HOLD=0 ;;
    census) RAMP_CEILING=40;  RAMP_HOLD=75 ;;
    *) echo "bad mode $MODE (wall|census)"; exit 2 ;;
esac
PH=$D/phaseramp-$TAG.log
CON=$D/console-$TAG.log

for c in "$CLASS" "$ECLASS"; do
    case "$c" in Small|Medium|Big) ;; *) echo "bad class $c"; exit 2;; esac
done

[ -f "$BK" ] || cp "$F" "$BK"

python3 - "$F" "$CLASS" "$ECLASS" <<'PY'
import re, sys
path, cls, ecls = sys.argv[1], sys.argv[2], sys.argv[3]
src = open(path).read()
for role, want in (("client", cls), ("event", ecls)):
    pat = re.compile(r'(suffix: "%s",\n            stack: StackSizeClass::)(Small|Medium|Big)(,)' % role)
    src, n = pat.subn(lambda m: m.group(1) + want + m.group(3), src)
    assert n == 1, "expected exactly 1 %s roster site, found %d" % (role, n)
    print("set %s roster stack class to %s" % (role, want))
open(path, "w").write(src)
PY

echo "=== source confirmation"
rg -n -A2 'suffix: "(client|event)"' "$F" | tee "$D/build-$TAG.txt"

echo "=== build"
"$D/build-e8.sh" plain || exit 1
sha256sum "$D/ftp/root/realtime-ca-ioc.vxe" | tee -a "$D/build-$TAG.txt"

# One ftpd per panel, port 2141.  netDrv loads the RTP through it.
if ! pgrep -f "ftpd-e8.py" > /dev/null; then
    setsid python3 "$D/ftpd-e8.py" "$D/ftp/root" > "$D/ftpd.out" 2>&1 &
    echo $! > "$D/ftpd.pid"
    sleep 1
    echo "ftpd started pid=$(cat "$D/ftpd.pid")"
else
    echo "ftpd already up"
fi

echo "=== boot mem=$MEM"
"$D/boot-e8.sh" "$MEM" || exit 1

# The kernel is ready when the symbol table is in and the shell prompts.
for _ in $(seq 1 60); do
    grep -q "Adding .* symbols for standalone" "$D/console.log" && break
    sleep 1
done
grep -q "Adding .* symbols for standalone" "$D/console.log" || {
    echo "kernel never reached the shell prompt"; "$D/stop-e8.sh"; exit 1; }

# The symbol-table line lands BEFORE the shell prints its first `-> `, and a
# command written into the fifo in that window is echoed by the tty driver and
# then dropped by the not-yet-reading shell -- the console shows the text with
# no `value =` after it and the load never happens.  So settle, then confirm
# the load took by waiting for rtpSp's return value, and re-send if it did not.
sleep 6
for attempt in 1 2 3; do
    echo 'rtpSp "/host.host/realtime-ca-ioc.vxe"' > "$D/con.in"
    for _ in $(seq 1 30); do
        grep -q "^value = " "$D/console.log" && break
        sleep 1
    done
    grep -q "^value = " "$D/console.log" && { echo "rtpSp took on attempt $attempt"; break; }
    echo "rtpSp attempt $attempt produced no return value; re-sending"
done
grep -q "^value = " "$D/console.log" || {
    echo "rtpSp never returned; console tail:"; tail -12 "$D/console.log"
    echo "ftpd tail:"; tail -6 "$D/ftpd.out"; "$D/stop-e8.sh"; exit 1; }

# Ready = the CA server says it is serving.  A guest that dies in the loader or
# aborts on pthread_create EAGAIN never prints this line.
for _ in $(seq 1 120); do
    grep -q "serving .* records on CA port 5064" "$D/console.log" && break
    sleep 1
done
grep -q "serving .* records on CA port 5064" "$D/console.log" || {
    echo "IOC never announced the CA server; last console lines:"
    tail -20 "$D/console.log"; "$D/stop-e8.sh"; exit 1; }
sleep 3

echo "=== ramp"
python3 "$D/phaseramp.py" "$RAMP_CEILING" "$RAMP_HOLD" "$TAG" 300 > "$PH" 2>&1
tail -8 "$PH"

# The C6 census runs every 6th 10 s pass; wait for one that lands after the
# ramp so the declared stack of a real CAS-client task is on the record.
for _ in $(seq 1 90); do
    grep -q "STACKUSE .* name=CAS-client 0 size=" "$D/console.log" && break
    sleep 1
done
echo "=== declared stack, from the image's own census"
grep -E "STACKUSE .* name=CAS-(client|event) 0 size=" "$D/console.log" | tail -2

cp "$D/console.log" "$CON"
"$D/stop-e8.sh"
echo "=== done class=$CLASS mem=$MEM ramp=$PH console=$CON"
