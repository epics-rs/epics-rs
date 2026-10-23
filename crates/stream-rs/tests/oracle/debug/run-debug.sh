#!/bin/bash
# Regenerate the debug/<case>.log files from C StreamDevice at run time.
#
# STREAM_TOP is a StreamDevice top built with its default BUSSES (Debug
# among them) and sync-finish.patch: C leaves PACT 1 for good when the
# protocol finishes inside startProtocol, which the port does not copy, and
# the patch completes the record in that pass as the port does. Each case of cases.txt boots an IOC, streamDebug 1
# from before iocInit, with one record "rec" of the case's type on the
# case's link, puts PROC 1 unless the case is "intr", and keeps from stderr
# the debug lines of DebugInterface.cc, StreamBusInterface.cc,
# StreamCore.cc and StreamEpics.cc without the thread name and file:line
# prefix ("twice" processes a second time), but for attachBus's line (it prints a pointer), and the error
# lines. A debug line may hold a line end (writeRequest prints the output
# as it is): its continuation is kept with it. After the run the log has
# "FIELD value" for VAL SEVR STAT UDF PACT PROC, from dbgf. C's
# scanValue(long&) line ends with a backslash and an n and no line end
# (StreamCore.cc:1499), which the port does not copy: the line it runs
# into is split off it, and the two characters dropped.
# Arguments, if any, are the cases to run.
set -euo pipefail
: "${STREAM_TOP:?set STREAM_TOP to a built StreamDevice}"
ARCH=${EPICS_HOST_ARCH:-linux-x86_64}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap '[ -n "${KEEP:-}" ] || rm -rf "$work"' EXIT
while IFS=$'\t' read -r c rtype link opts; do
    case "$c" in '#'* | '') continue ;; esac
    if [ $# -gt 0 ] && [[ " $* " != *" $c "* ]]; then continue; fi
    scan=Passive proc="dbpf rec.PROC 1"
    [[ ",$opts," == *",intr,"* ]] && { scan="I/O Intr"; proc=; }
    after=
    [[ ",$opts," == *",twice,"* ]] && after="dbpf rec.PROC 1
epicsThreadSleep 0.5"
    printf 'record(%s, "rec") {\n  field(DTYP, "stream")\n  field(INP, "@%s")\n  field(SCAN, "%s")\n}\n' \
        "$rtype" "$link" "$scan" >"$work/$c.db"
    cat >"$work/$c.cmd" <<EOC
dbLoadDatabase "$STREAM_TOP/dbd/streamApp.dbd"
streamApp_registerRecordDeviceDriver pdbbase
epicsEnvSet STREAM_PROTOCOL_PATH $here
var streamMsgTimeStamped 0
var streamErrorDeadTime 0
dbLoadRecords "$work/$c.db"
var streamDebug 1
iocInit
$proc
epicsThreadSleep 0.5
$after
var streamDebug 0
dbgf rec.VAL
dbgf rec.SEVR
dbgf rec.STAT
dbgf rec.UDF
dbgf rec.PACT
dbgf rec.PROC
exit
EOC
    "$STREAM_TOP/bin/$ARCH/streamApp" "$work/$c.cmd" </dev/null >"$work/$c.out" 2>"$work/$c.err" || true
    perl -pe 's/ failed\\n(?=\S+ \S+\.cc?:\d+: )/ failed\n/' "$work/$c.err" |
    perl -ne 'next unless $on ||= /Stream(Epics|Core)\.cc:\d+: /;
        if ($cont) { print; $cont = 0 if /\)$/; next }
        next if /^\e\[|^iocsh Error: |^iocRun: |^iocInit: |^Starting iocInit|^#|StreamCore::attachBus\(/;
        next unless s/^\S+ //;
        if (s/^(\S+\.cc?):\d+: //) {
            next unless $1 =~ /^(StreamCore|StreamEpics|StreamBusInterface|DebugInterface)\.cc$/;
            $cont = 1 if /^DebugInterface::writeRequest/ && !/msec\)$/;
            print;
        } else { print }' >"$here/$c.log"
    perl -ne 'push @v, $1 if /^DBF_\w+:\s+(.*?)\s*$/;
        END { @f = qw(VAL SEVR STAT UDF PACT PROC); @v = @v[-6..-1];
            print "$f[$_] $v[$_]\n" for 0..5 }' \
        "$work/$c.out" >>"$here/$c.log"
done <"$here/cases.txt"
