#!/bin/bash
# Regenerate bo/posts.log, the monitors devboStream.c's readData posts from
# an @init run, from C StreamDevice.
#
# STREAM_TOP is a clean StreamDevice top. One bo "rec" (posts.proto pi) on
# an asyn IP port to a device that answers each "I" with the next of
# 5 7 0 0 0 3 and each "W..." with "x" (a mismatch: the process fails
# INVALID). camonitor, one per field of VAL RVAL RBV SEVR and per mask
# v l a, watches while the IOC runs these steps, each @init rerun by
# iocPause and iocRun:
#   A: RVAL and RBV change, VAL does not
#   B: all three change
#   C: nothing changes
#   D: after a failed process, the alarm clears, nothing else changes
#   E: after a failed process, the alarm clears and all three change
# The log has one line "STEP FIELD MASK values..." for each monitor that
# took an update in a step. Run it in a network namespace: the IOC and
# the device take free ports.
set -euo pipefail
: "${STREAM_TOP:?set STREAM_TOP to a built StreamDevice}"
ARCH=${EPICS_HOST_ARCH:-linux-x86_64}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
pids=()
trap 'for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done; rm -rf "$work"' EXIT

python3 -I - "$work/port" <<'PY' &
import socket, sys
s = socket.socket(); s.bind(('127.0.0.1', 0)); s.listen(1)
open(sys.argv[1], 'w').write(str(s.getsockname()[1]))
replies = [b'5', b'7', b'0', b'0', b'0', b'3']
c, _ = s.accept(); buf = b''
while True:
    d = c.recv(1000)
    if not d: break
    buf += d
    while b'\r\n' in buf:
        line, buf = buf.split(b'\r\n', 1)
        c.sendall((replies.pop(0) if line == b'I' else b'x') + b'\r\n')
PY
pids+=($!)
until [ -s "$work/port" ]; do sleep 0.05; done

caport=$(python3 -I "$here/../rt/caport.py")
export EPICS_CA_SERVER_PORT=$caport EPICS_CA_ADDR_LIST=127.0.0.1 EPICS_CA_AUTO_ADDR_LIST=NO
export EPICS_CAS_AUTO_BEACON_ADDR_LIST=NO EPICS_CAS_BEACON_ADDR_LIST=127.255.255.255
cat >"$work/rec.db" <<EOD
record(bo, "rec") { field(DTYP, "stream") field(OUT, "@posts.proto pi P") field(ZNAM, "zero") field(ONAM, "one") }
EOD
cat >"$work/st.cmd" <<EOC
dbLoadDatabase "$STREAM_TOP/dbd/streamApp.dbd"
streamApp_registerRecordDeviceDriver pdbbase
epicsEnvSet STREAM_PROTOCOL_PATH $here
drvAsynIPPortConfigure "P", "127.0.0.1:$(cat "$work/port")", 0, 0, 0
dbLoadRecords "$work/rec.db"
iocInit
EOC
mkfifo "$work/in"
"$STREAM_TOP/bin/$ARCH/streamApp" "$work/st.cmd" <"$work/in" >"$work/ioc.out" 2>&1 &
pids+=($!)
exec 3>"$work/in"
sleep 1.5

fields="VAL RVAL RBV SEVR"
for f in $fields; do
    for m in v l a; do
        camonitor -n -t n -m $m rec.$f >"$work/$f.$m" 2>&1 &
        pids+=($!)
    done
done
sleep 1.5

# the monitors' lines so far
declare -A seen
mark() { for f in $fields; do for m in v l a; do seen[$f.$m]=$(wc -l <"$work/$f.$m"); done; done; }
# the values each monitor took since the mark, as step $1
took() {
    for f in $fields; do
        for m in v l a; do
            vals=$(tail -n +$((seen[$f.$m] + 1)) "$work/$f.$m" | awk '{print $2}' | tr '\n' ' ')
            [ -z "$vals" ] || echo "$1 $f $m ${vals% }"
        done
    done
}
rerun() { mark; printf 'iocPause\niocRun\n' >&3; sleep 1; took "$1"; }
fail() { printf 'dbpf rec.PROC 1\n' >&3; sleep 1.5; }

{
    rerun A
    rerun B
    rerun C
    fail; rerun D
    fail; rerun E
} >"$here/posts.log"
printf 'exit\n' >&3
