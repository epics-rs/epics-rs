#!/bin/bash
# Regenerate the epics/<case>.log files from C StreamDevice at run time.
#
# STREAM_TOP is a clean StreamDevice top (not the expand.patch build).
# Each case of cases.txt boots an IOC, streamDebug 1 from before iocInit,
# with one record "rec" of the case's type, VAL and link (the text after
# '@', "<empty>" for none), on an asyn IP port "P" to ../rt/dev.py (which answers each request
# of the device table) or to a port nobody listens on. Kept from stderr:
# StreamEpics.cc's and StreamCore.cc's debug lines and the error lines (no
# source file), without the thread name and file:line prefix, but for
# attachBus's line (it prints a pointer), the bus's error lines and
# iocsh's own. After the run the log has "FIELD value" for VAL SEVR STAT
# UDF PACT PROC, from dbgf. C's scanValue(long&) line ends "failed\n"
# without a line end; the line of another file it runs into is cut off it.
# Options (a comma list, '-' for none): "intr" makes the record SCAN
# "I/O Intr", not processed by the script; "noproc" processes nothing;
# "proc2" puts PROC 2, not 1; "twice" processes a second time; "tpro"
# sets TPRO 2; "noval" leaves VAL out of the record, which then loads
# with UDF 1; "reload" runs streamReload after the processing; "report"
# runs streamReportRecord rec; "tointr" puts SCAN "I/O Intr" after the
# processing, "topassive" puts it back to "Passive"; "flip" puts SCAN
# "I/O Intr", 0.2 s later "Passive" and "I/O Intr" again, and waits 1.5 s.
# Arguments, if any, are the cases to run.
set -euo pipefail
: "${STREAM_TOP:?set STREAM_TOP to a built StreamDevice}"
ARCH=${EPICS_HOST_ARCH:-linux-x86_64}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
dev=
trap '[ -n "$dev" ] && kill "$dev" 2>/dev/null; [ -n "${KEEP:-}" ] || rm -rf "$work"' EXIT
# below the ephemeral range, and apart from run-rt.sh's
port=27300
while IFS=$'\t' read -r c rtype link val table opts; do
    case "$c" in '#'* | '') continue ;; esac
    # read collapses empty tab-separated fields
    [ "$link" = '<empty>' ] && link=
    if [ $# -gt 0 ] && [[ " $* " != *" $c "* ]]; then continue; fi
    port=$((port + 1))
    case "$rtype" in *o | *out | *oDirect) lf=OUT ;; *) lf=INP ;; esac
    scan=Passive proc="dbpf rec.PROC 1" tpro=
    has() { [[ ",$opts," == *",$1,"* ]]; }
    has intr && { scan="I/O Intr"; proc=; }
    has noproc && proc=
    has proc2 && proc="dbpf rec.PROC 2"
    has tpro && tpro='field(TPRO, "2")'
    valf="field(VAL, \"$val\")"
    has noval && valf=
    printf 'record(%s, "rec") {\n  field(DTYP, "stream")\n  field(%s, "@%s")\n  %s\n  field(SCAN, "%s")\n  %s\n}\n' \
        "$rtype" "$lf" "$link" "$valf" "$scan" "$tpro" >"$work/$c.db"
    after=
    has twice && after="$after
dbpf rec.PROC 1
epicsThreadSleep 0.5"
    has reload && after="$after
streamReload rec
epicsThreadSleep 0.5"
    has report && after="$after
streamReportRecord rec"
    has tointr && after="$after
dbpf rec.SCAN \"I/O Intr\"
epicsThreadSleep 0.5"
    has topassive && after="$after
dbpf rec.SCAN Passive
epicsThreadSleep 0.5"
    has flip && after="$after
dbpf rec.SCAN \"I/O Intr\"
epicsThreadSleep 0.2
dbpf rec.SCAN Passive
dbpf rec.SCAN \"I/O Intr\"
epicsThreadSleep 1.5"
    cat >"$work/$c.cmd" <<EOC
dbLoadDatabase "$STREAM_TOP/dbd/streamApp.dbd"
streamApp_registerRecordDeviceDriver pdbbase
epicsEnvSet STREAM_PROTOCOL_PATH $here
drvAsynIPPortConfigure "P", "127.0.0.1:$port", 0, 0, 0
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
    # Threads print a debug line in pieces, so two can interleave; a kept
    # line that is none of the two files' shows it, and the case is run again.
    for try in 1 2 3 4 5; do
        dev=
        if [ "$table" != - ]; then
            python3 -I "$here/../rt/dev.py" "$port" "$table" &
            dev=$!
            until python3 -c "import socket,sys; sys.exit(socket.socket().connect_ex(('127.0.0.1',$port)))"; do
                kill -0 "$dev"
                sleep 0.05
            done
        fi
        "$STREAM_TOP/bin/$ARCH/streamApp" "$work/$c.cmd" </dev/null >"$work/$c.out" 2>"$work/$c.err" || true
        if [ -n "$dev" ]; then kill "$dev" 2>/dev/null || true; wait "$dev" 2>/dev/null || true; dev=; fi
        perl -ne 'next unless $on ||= /Stream(Epics|Core)\.cc:\d+: /;
            next if /^\e\[|^iocsh Error: |^iocRun: |^iocInit: |^Starting iocInit|^#|StreamCore::attachBus\(/;
            next unless s/^\S+ //;
            s/( failed\\n)\S+ (?!Stream(Core|Epics)\.cc:)\S+\.cc?:\d+: .*/$1/;
            next if /^\S+ lockRequest: port \S+ not connected$/;
            if (s/^(\S+\.cc?):\d+: //) { print if $1 eq "StreamCore.cc" || $1 eq "StreamEpics.cc" } else { print }' \
            "$work/$c.err" >"$here/$c.log"
        # dbgf prints "DBF_TYPE:  value" (a string in quotes)
        # dbgf prints "DBF_TYPE:  value"; dbpf's echo of its put is one too
        perl -ne 'push @v, $1 if /^DBF_\w+:\s+(.*?)\s*$/;
            END { @f = qw(VAL SEVR STAT UDF PACT PROC); @v = @v[-6..-1];
                print "$f[$_] $v[$_]\n" for 0..5 }' \
            "$work/$c.out" >>"$here/$c.log"
        # the report goes to stdout
        if has report; then
            perl -ne 'print if /^rec: / .. /^$/' "$work/$c.out" >"$here/$c.report"
        fi
        grep -vF 'failed\n' "$here/$c.log" |
            grep -qvE '^(StreamCore::|Stream::|stream[A-Z]\w*\(|recordProcessCallback\(|drvStreamInit|rec: |(VAL|SEVR|STAT|UDF|PACT|PROC) |starting exception handler$|reparsing input |No match for )' ||
            break
    done
done <"$here/cases.txt"
