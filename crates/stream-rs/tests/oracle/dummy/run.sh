#!/bin/bash
# Regenerate dummy.log from C StreamDevice.
#
# STREAM_TOP is a clean StreamDevice top. streamApp boots records "rec"
# (d.proto p on bus "dummy"), "other" (bus "dummyX") and "addr" (bus
# "dummy", address 3) with streamDebug on, then processes "rec" once. Kept
# are the lines of StreamBusInterface::find, of DummyInterface's checks,
# of StreamCore and the bus errors, without the thread name and file:line
# prefix, pointers masked as 0x*; the checks of the other bus interfaces
# are left out, rs having none registered beside DummyInterface.
set -euo pipefail
: "${STREAM_TOP:?set STREAM_TOP to a built StreamDevice}"
ARCH=${EPICS_HOST_ARCH:-linux-x86_64}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cat >"$work/d.db" <<DB
record(ai, "rec") { field(DTYP, "stream") field(INP, "@d.proto p dummy") }
record(ai, "other") { field(DTYP, "stream") field(INP, "@d.proto p dummyX") }
record(ai, "addr") { field(DTYP, "stream") field(INP, "@d.proto p dummy 3") }
DB
cat >"$work/d.cmd" <<CMD
dbLoadDatabase "$STREAM_TOP/dbd/streamApp.dbd"
streamApp_registerRecordDeviceDriver pdbbase
epicsEnvSet STREAM_PROTOCOL_PATH $here
var streamMsgTimeStamped 0
dbLoadRecords "$work/d.db"
var streamDebug 1
iocInit
dbpf rec.PROC 1
epicsThreadSleep 0.5
var streamDebug 0
exit
CMD
"$STREAM_TOP/bin/$ARCH/streamApp" "$work/d.cmd" </dev/null >/dev/null 2>"$work/d.err"
perl -ne 'next unless s/^\S+ //; s/^\S+\.cc?:\d+: //; s/0x[0-9a-f]+/0x*/g;
    next if /check (?!DummyInterface)|find (?!Dummy)\w+Interface (does not )?match/;
    print if /^StreamBusInterface::find|^StreamCore::|^Cannot find a bus/' \
    "$work/d.err" >"$here/dummy.log"
