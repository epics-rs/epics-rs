#!/bin/bash
# Regenerate the rt/<case>.log files from C StreamDevice at run time.
#
# STREAM_TOP is a clean StreamDevice top (not the expand.patch build).
# Each case of cases.txt boots an IOC with one record "rec" of the case's
# type and VAL, loading protocol p of rt/<case>.proto on an asyn IP port
# "P" to dev.py (which answers each request of the device table) or to a
# port nobody listens on. dev.py takes a free port either way and the IOC
# starts no CA server (RSRV's UDP bind of a picked port could fail and
# cantProceed), so runs can overlap. The record is processed once with
# streamDebug 1.
# The IOC writes stderr a line per write (rt/linewise.c), so two threads'
# debug lines cannot interleave.
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
# dev.py and the IOC each lead a process group (setsid), killed whole on
# any exit; the IOC runs in the background so a TERM or INT, which bash
# holds while a foreground command runs, reaches the trap at once. A
# SIGKILL of the rig skips the trap, so each also gets SIGKILL when the rig
# dies (pdeathsig); the PPID test covers a rig gone before the prctl.
own=(setsid setpriv --pdeathsig KILL sh -c '[ "$PPID" = "$0" ] && exec "$@"' "$$")
dev= ioc=
trap 'kill -- ${dev:+-$dev} ${ioc:+-$ioc} 2>/dev/null || true; [ -n "${KEEP:-}" ] || rm -rf "$work"' EXIT
trap 'exit 143' TERM
trap 'exit 130' INT
cc -shared -fPIC -o "$work/linewise.so" "$here/linewise.c" -ldl
while IFS=$'\t' read -r c rtype val table opts; do
    case "$c" in '#'* | '') continue ;; esac
    if [ $# -gt 0 ] && [[ " $* " != *" $c "* ]]; then continue; fi
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
    cat >"$work/$c.cmd.in" <<EOF
dbLoadDatabase "$STREAM_TOP/dbd/streamApp.dbd"
streamApp_registerRecordDeviceDriver pdbbase
epicsEnvSet STREAM_PROTOCOL_PATH $here
drvAsynIPPortConfigure "P", "127.0.0.1:@PORT@", 0, 0, 0
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
    : >"$work/port"
    "${own[@]}" python3 -I "$here/dev.py" 0 "$table" >"$work/port" &
    dev=$!
    until [ -s "$work/port" ]; do
        kill -0 "$dev"
        sleep 0.05
    done
    sed "s/@PORT@/$(cat "$work/port")/" "$work/$c.cmd.in" >"$work/$c.cmd"
    EPICS_IOC_IGNORE_SERVERS=rsrv LD_PRELOAD="$work/linewise.so" \
        "${own[@]}" "$STREAM_TOP/bin/$ARCH/streamApp" "$work/$c.cmd" </dev/null >/dev/null 2>"$work/$c.err" &
    ioc=$!
    wait "$ioc" || true
    ioc=
    kill -- -"$dev" 2>/dev/null || true
    wait "$dev" 2>/dev/null || true
    dev=
    perl -ne 'next unless $on ||= /StreamCore\.cc:\d+: /;
        next if /^\e\[|^iocsh Error: |^iocRun: |StreamCore::attachBus\(/;
        next unless s/^\S+ //;
        s/( failed\\n)\S+ (?!StreamCore\.cc:)\S+\.cc?:\d+: .*/$1/;
        next if /^\S+ lockRequest: port \S+ not connected$/;
        if (s/^(\S+\.cc?):\d+: //) { print if $1 eq "StreamCore.cc" } else { print }' \
        "$work/$c.err" >"$here/$c.log"
done <"$here/cases.txt"
