#!/bin/bash
# Regenerate the rt/<case>.log files from C StreamDevice at run time.
#
# STREAM_TOP is a clean StreamDevice top (not the expand.patch build).
# Each case of cases.txt boots an IOC with one record "rec" of the case's
# type and VAL, loading protocol p of rt/<case>.proto on an asyn IP port
# "P" to dev.py (which answers each request of the device table) or to a
# port nobody listens on. The record is processed once with streamDebug 1.
# Kept from stderr, from the first StreamCore line on: StreamCore.cc's
# debug lines and the error lines (no source file), without the thread
# name and file:line prefix. Debug lines of other files are the record
# layer's and the bus's, and are left out, as are the bus's error lines
# (AsynDriverInterface's "<rec> lockRequest: port P not connected") and
# iocsh's own for a failing exec command (a coloured line, "iocsh Error").
# C's scanValue(long&) line ends "failed\n" without a line end; the line
# of another file it runs into is cut off it.
# Options this script reads: "init" turns streamDebug on before iocInit,
# so the @init run is kept (but for attachBus's line, which prints a
# pointer); "twice" processes the record a second time, "thrice" a third
# time 2.1 s later; "deadtime=N" sets streamErrorDeadTime; "intr" makes
# the record SCAN "I/O Intr", not processed by the script.
# Arguments, if any, are the cases to run.
set -euo pipefail
: "${STREAM_TOP:?set STREAM_TOP to a built StreamDevice}"
ARCH=${EPICS_HOST_ARCH:-linux-x86_64}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
dev=
trap '[ -n "$dev" ] && kill "$dev" 2>/dev/null; [ -n "${KEEP:-}" ] || rm -rf "$work"' EXIT
# below the ephemeral range, where a client socket can hold the port
port=27100
while IFS=$'\t' read -r c rtype val table opts; do
    case "$c" in '#'* | '') continue ;; esac
    if [ $# -gt 0 ] && [[ " $* " != *" $c "* ]]; then continue; fi
    port=$((port + 1))
    case "$rtype" in *o | *out | *oDirect) link=OUT ;; *) link=INP ;; esac
    scan=Passive proc="dbpf rec.PROC 1"
    case ",$opts," in *,intr,*) scan="I/O Intr" proc= ;; esac
    printf 'record(%s, "rec") {\n  field(DTYP, "stream")\n  field(%s, "@%s.proto p P")\n  field(VAL, "%s")\n  field(SCAN, "%s")\n}\n' \
        "$rtype" "$link" "$c" "$val" "$scan" >"$work/$c.db"
    early= late="var streamDebug 1" again=
    case ",$opts," in *,init,*) early=$late late= ;; esac
    case ",$opts," in *,twice,* | *,thrice,*) again="dbpf rec.PROC 1
epicsThreadSleep 0.5" ;; esac
    case ",$opts," in *,thrice,*) again="$again
epicsThreadSleep 2.1
dbpf rec.PROC 1
epicsThreadSleep 0.5" ;; esac
    dead=$(printf '%s\n' "$opts" | tr , '\n' | sed -n 's/^deadtime=//p')
    cat >"$work/$c.cmd" <<EOF
dbLoadDatabase "$STREAM_TOP/dbd/streamApp.dbd"
streamApp_registerRecordDeviceDriver pdbbase
epicsEnvSet STREAM_PROTOCOL_PATH $here
drvAsynIPPortConfigure "P", "127.0.0.1:$port", 0, 0, 0
var streamMsgTimeStamped 0
var streamErrorDeadTime ${dead:-0}
dbLoadRecords "$work/$c.db"
$early
iocInit
$late
$proc
epicsThreadSleep 0.5
$again
var streamDebug 0
exit
EOF
    # Threads print a debug line in pieces, so two can interleave; a kept
    # line that is none of StreamCore's shows it (scanValue's "failed\n"
    # line, which C runs into the next, aside), and the case is run again.
    for try in 1 2 3 4 5; do
        dev=
        if [ "$table" != - ]; then
            python3 -I "$here/dev.py" "$port" "$table" &
            dev=$!
            until python3 -c "import socket,sys; sys.exit(socket.socket().connect_ex(('127.0.0.1',$port)))"; do
                kill -0 "$dev"
                sleep 0.05
            done
        fi
        "$STREAM_TOP/bin/$ARCH/streamApp" "$work/$c.cmd" </dev/null >/dev/null 2>"$work/$c.err" || true
        if [ -n "$dev" ]; then kill "$dev" 2>/dev/null || true; wait "$dev" 2>/dev/null || true; dev=; fi
        perl -ne 'next unless $on ||= /StreamCore\.cc:\d+: /;
            next if /^\e\[|^iocsh Error: |^iocRun: |StreamCore::attachBus\(/;
            next unless s/^\S+ //;
            s/( failed\\n)\S+ (?!StreamCore\.cc:)\S+\.cc?:\d+: .*/$1/;
            next if /^\S+ lockRequest: port \S+ not connected$/;
            if (s/^(\S+\.cc?):\d+: //) { print if $1 eq "StreamCore.cc" } else { print }' \
            "$work/$c.err" >"$here/$c.log"
        grep -vF 'failed\n' "$here/$c.log" |
            grep -qvE '^(StreamCore::|rec: |starting exception handler$|reparsing input |No match for )' ||
            break
    done
done <"$here/cases.txt"
