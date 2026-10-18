#!/bin/bash
# Regenerate posts.log from C StreamDevice: the monitors devstringoutStream.c's
# readData posts from an @init rerun, outside a process.
#
# STREAM_TOP is a clean StreamDevice top. Records "a" (MPST and APST "On
# Change"), "b" (both "Always") and "c" (MPST "Always") run posts.proto's
# p on an asyn IP port to dev.py, whose replies to the @init query are
# v0, v1, v1, v2. iocInit runs @init (v0, no monitor yet); a process times
# out (TIMEOUT INVALID, the record's own monitor); then three iocPause/iocRun
# rerun @init (v1, v1, v2). "d" runs q in a process, once: its query's
# reply, then a second in that times out, so the record's own monitor posts
# VAL in TIMEOUT INVALID; readData ran with PACT set and posted nothing. camonitor watches VAL with DBE_VALUE, DBE_LOG and
# DBE_ALARM, and SEVR and STAT with DBE_VALUE. posts.log has "<mask> <pv>
# <value> <stat> <sevr>" for each event after the connect event, in each pv's order.
set -euo pipefail
: "${STREAM_TOP:?set STREAM_TOP to a built StreamDevice}"
: "${EPICS_BASE:?set EPICS_BASE}"
ARCH=${EPICS_HOST_ARCH:-linux-x86_64}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
pids=()
trap 'for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done; [ -n "${KEEP:-}" ] || rm -rf "$work"' EXIT
python3 -I "$here/dev.py" v0,v1,v1,v2 >"$work/port" &
pids+=($!)
until [ -s "$work/port" ]; do sleep 0.05; done
caport=$(python3 -I "$here/../rt/caport.py")
cat >"$work/p.db" <<EOD
record(stringout, "a") { field(DTYP, "stream") field(OUT, "@posts.proto p(a) P") field(VAL, "x") }
record(stringout, "b") { field(DTYP, "stream") field(OUT, "@posts.proto p(b) P") field(VAL, "x") field(MPST, "Always") field(APST, "Always") }
record(stringout, "c") { field(DTYP, "stream") field(OUT, "@posts.proto p(c) P") field(VAL, "x") field(MPST, "Always") }
record(stringout, "d") { field(DTYP, "stream") field(OUT, "@posts.proto q(d) P") field(VAL, "x") }
EOD
rerun="iocPause
iocRun
epicsThreadSleep 1"
cat >"$work/st.cmd" <<EOC
dbLoadDatabase "$STREAM_TOP/dbd/streamApp.dbd"
streamApp_registerRecordDeviceDriver pdbbase
epicsEnvSet STREAM_PROTOCOL_PATH $here
drvAsynIPPortConfigure "P", "127.0.0.1:$(cat "$work/port")", 0, 0, 0
dbLoadRecords "$work/p.db"
iocInit
epicsThreadSleep 3
dbpf a.PROC 1
dbpf b.PROC 1
dbpf c.PROC 1
dbpf d.PROC 1
epicsThreadSleep 1.5
$rerun
$rerun
$rerun
exit
EOC
export EPICS_CA_ADDR_LIST=127.0.0.1:$caport EPICS_CA_AUTO_ADDR_LIST=NO
cam() {
    "$EPICS_BASE/bin/$ARCH/camonitor" -t n -n -F '|' -m "$1" "${@:2}" >"$work/m$1" 2>/dev/null &
    pids+=($!)
}
EPICS_CA_SERVER_PORT=$caport "$STREAM_TOP/bin/$ARCH/streamApp" "$work/st.cmd" </dev/null >"$work/ioc.out" 2>&1 &
ioc=$!
cam v a b c d a.SEVR b.SEVR c.SEVR a.STAT b.STAT c.STAT
cam l a b c d
cam a a b c d
wait "$ioc" || true
for m in v l a; do
    # each pv's first event is its connect event
    # "pv||value|stat|sevr", the alarm empty for none
    perl -ne 'chomp; next unless /^([^|]+)\|\|([^|]*)\|([^|]*)\|([^|]*)$/;
        print "'"$m"' $1 $2 $3 $4\n" if $seen{$1}++' "$work/m$m"
done | sort -s -k1,1 -k2,2 >"$here/posts.log"
