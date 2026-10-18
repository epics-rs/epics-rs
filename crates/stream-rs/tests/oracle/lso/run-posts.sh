#!/bin/bash
# Regenerate posts.log from C StreamDevice: the monitors devlsoStream.c's
# readData posts from an @init rerun, outside a process.
#
# STREAM_TOP is a clean StreamDevice top. Records "a" (MPST and APST "On
# Change"), "b" (both "Always") and "c" (MPST "Always") run posts.proto's
# p on an asyn IP port to dev.py, whose replies to the @init query are
# v0, v1, v1, v22. iocInit runs @init (v0, no monitor yet); a process times
# out (TIMEOUT INVALID, the record's own monitor); then three iocPause/iocRun
# rerun @init (v1, v1, v22, which moves LEN). "d" runs q in a process, once: its query's
# reply, then a second in that times out, so the record's own monitor posts
# VAL in TIMEOUT INVALID; readData ran with PACT set and posted nothing. "e"
# holds "hello" and runs f in a process, once: v0 fails "%!8s" after the
# scan wrote "v0", so VAL is "v0<NUL>lo" under the old LEN, which camonitor
# "c" shows as e.VAL$, element by element. camonitor watches VAL with DBE_VALUE, DBE_LOG and
# DBE_ALARM, and SEVR, STAT and LEN with DBE_VALUE. posts.log has "<mask> <pv>
# <value> <stat> <sevr>" for each event after the connect event, in each pv's order.
set -euo pipefail
: "${STREAM_TOP:?set STREAM_TOP to a built StreamDevice}"
: "${EPICS_BASE:?set EPICS_BASE}"
ARCH=${EPICS_HOST_ARCH:-linux-x86_64}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
pids=()
trap 'for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done; [ -n "${KEEP:-}" ] || rm -rf "$work"' EXIT
python3 -I "$here/dev.py" v0,v1,v1,v22 >"$work/port" &
pids+=($!)
until [ -s "$work/port" ]; do sleep 0.05; done
caport=$(python3 -I "$here/../rt/caport.py")
cat >"$work/p.db" <<EOD
record(lso, "a") { field(DTYP, "stream") field(OUT, "@posts.proto p(a) P") }
record(lso, "b") { field(DTYP, "stream") field(OUT, "@posts.proto p(b) P") field(MPST, "Always") field(APST, "Always") }
record(lso, "c") { field(DTYP, "stream") field(OUT, "@posts.proto p(c) P") field(MPST, "Always") }
record(lso, "d") { field(DTYP, "stream") field(OUT, "@posts.proto q(d) P") }
record(lso, "e") { field(DTYP, "stream") field(OUT, "@posts.proto f(e) P") field(DOL, {const:'hello'}) }
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
dbpf e.PROC 1
epicsThreadSleep 1.5
$rerun
$rerun
$rerun
exit
EOC
export EPICS_CA_ADDR_LIST=127.0.0.1:$caport EPICS_CA_AUTO_ADDR_LIST=NO
cam() {
    local s=-S
    [ "$1" != c ] || s=
    "$EPICS_BASE/bin/$ARCH/camonitor" -t n -n $s -F '|' -m "${1/c/va}" "${@:2}" >"$work/m$1" 2>/dev/null &
    pids+=($!)
}
EPICS_CA_SERVER_PORT=$caport "$STREAM_TOP/bin/$ARCH/streamApp" "$work/st.cmd" </dev/null >"$work/ioc.out" 2>&1 &
ioc=$!
cam v a b c d a.SEVR b.SEVR c.SEVR a.STAT b.STAT c.STAT a.LEN b.LEN c.LEN d.LEN
cam l a b c d
cam a a b c d
cam c e.VAL\$
wait "$ioc" || true
for m in v l a; do
    # each pv's first event is its connect event
    # "pv||value|stat|sevr", the alarm empty for none; an empty value
    # loses its column
    perl -ne 'chomp; next unless /^([^|]+)\|\|(?:([^|]*)\|)?([^|]*)\|([^|]*)$/;
        print "'"$m"' $1 $2 $3 $4\n" if $seen{$1}++' "$work/m$m"
done >"$work/posts"
# "pv||count|elements...|stat|sevr", logged under the record's name
perl -ne 'chomp; my @f = split /\|/, $_, -1; next unless @f > 4 && $seen{$f[0]}++;
    (my $pv = $f[0]) =~ s/\.VAL\$$//;
    print "c $pv @f[2..$#f-2] $f[-2] $f[-1]\n"' "$work/mc" >>"$work/posts"
sort -s -k1,1 -k2,2 "$work/posts" >"$here/posts.log"
