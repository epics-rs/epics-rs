#!/bin/bash
# Regenerate the epics/<case>.log files from C StreamDevice at run time.
#
# STREAM_TOP is a clean StreamDevice top (not the expand.patch build).
# Each case of cases.txt boots an IOC, streamDebug 1 from before iocInit,
# with one record "rec" of the case's type, VAL and link (the text after
# '@', "<empty>" for none), on an asyn IP port "P" to ../rt/dev.py (which answers each request
# of the device table) or to a port nobody listens on. dev.py takes a
# free port either way and the IOC starts no CA server (RSRV's UDP bind
# of a picked port could fail and cantProceed), so runs can overlap.
# The IOC writes stderr a line per write (rt/linewise.c), so two threads'
# debug lines cannot interleave.
# Kept from stderr:
# StreamEpics.cc's and StreamCore.cc's debug lines and the error lines (no
# source file), each as its thread's name, a tab and the line without the
# file:line prefix (the order across threads is the scheduler's), but for
# attachBus's line (it prints a pointer), the bus's error lines and
# iocsh's own, and errlog's lines, whose place among the others the
# errlog thread decides. After the run the log has "FIELD value" for VAL
# SEVR STAT UDF PACT PROC, from dbgf. C's scanValue(long&) line ends "failed\n"
# without a line end; the line of another file it runs into is cut off it.
# The protocol directory, this one, is written <dir>.
# Options (a comma list, '-' for none): "intr" makes the record SCAN
# "I/O Intr", not processed by the script; "noproc" processes nothing;
# "proc2" puts PROC 2, not 1; "twice" processes a second time; "tpro"
# sets TPRO 2; "noval" leaves VAL out of the record, which then loads
# with UDF 1; "reload" runs streamReload rec after the processing (its
# stdout lines go to <case>.reload), with the debug off for "quiet": the
# callback of the protocol it aborts prints into its lines; "report"
# runs streamReportRecord rec after all the others (its output goes to
# <case>.report); "tointr" puts SCAN "I/O Intr" after the processing,
# "topassive" puts it back to "Passive"; "flip" puts SCAN "I/O Intr",
# 0.2 s later "Passive", 0.2 s later "I/O Intr" again, and waits 1.5 s;
# "pauserun" runs iocPause, then iocRun 0.2 s later; "reconnect" reads
# the port from another asyn user, which finds the connection the device
# closed and has asyn connect again; "putval" leaves VAL out of the
# record (base sets no array field before iocInit) and puts it with dbpf
# instead of PROC, a put that processes the record.
# "errlog" changes nothing here: the C log keeps the errlog lines of
# every case, the rs test compares them only for a case that names it.
# Two optional columns follow: the record's other fields, "F=v" pairs
# separated by ';', and fields dbgf prints after PROC, separated by ';'
# ('-' for none).
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
cc -shared -fPIC -o "$work/linewise.so" "$here/../rt/linewise.c" -ldl
while IFS=$'\t' read -r c rtype link val table opts fields dump; do
    case "$c" in '#'* | '') continue ;; esac
    # read collapses empty tab-separated fields
    [ "$link" = '<empty>' ] && link=
    if [ $# -gt 0 ] && [[ " $* " != *" $c "* ]]; then continue; fi
    case "$rtype" in *o | *out | *oDirect) lf=OUT ;; *) lf=INP ;; esac
    scan=Passive proc="dbpf rec.PROC 1" tpro=
    has() { [[ ",$opts," == *",$1,"* ]]; }
    has intr && { scan="I/O Intr"; proc=; }
    has noproc && proc=
    has proc2 && proc="dbpf rec.PROC 2"
    has tpro && tpro='field(TPRO, "2")'
    valf="field(VAL, \"$val\")"
    has noval && valf=
    has putval && { valf=; proc="dbpf rec.VAL '$val'"; }
    moref=
    if [ -n "${fields:-}" ] && [ "$fields" != - ]; then
        IFS=';' read -ra fv <<<"$fields"
        for f in "${fv[@]}"; do moref="$moref field(${f%%=*}, \"${f#*=}\")"; done
    fi
    shown="VAL SEVR STAT UDF PACT PROC"
    [ -n "${dump:-}" ] && [ "$dump" != - ] && shown="$shown ${dump//;/ }"
    printf 'record(%s, "rec") {\n  field(DTYP, "stream")\n  field(%s, "@%s")\n  %s\n  field(SCAN, "%s")\n  %s\n %s\n}\n' \
        "$rtype" "$lf" "$link" "$valf" "$scan" "$tpro" "$moref" >"$work/$c.db"
    after=
    has twice && after="$after
dbpf rec.PROC 1
epicsThreadSleep 0.5"
    quiet=
    has quiet && quiet='var streamDebug 0'
    has reload && after="$after
$quiet
streamReload rec
epicsThreadSleep 0.5"
    has tointr && after="$after
dbpf rec.SCAN \"I/O Intr\"
epicsThreadSleep 0.5"
    has topassive && after="$after
dbpf rec.SCAN Passive
epicsThreadSleep 0.5"
    has pauserun && after="$after
iocPause
epicsThreadSleep 0.2
iocRun
epicsThreadSleep 0.5"
    has reconnect && after="$after
asynOctetConnect e P
asynOctetRead e
epicsThreadSleep 0.5"
    has flip && after="$after
dbpf rec.SCAN \"I/O Intr\"
epicsThreadSleep 0.2
dbpf rec.SCAN Passive
epicsThreadSleep 0.2
dbpf rec.SCAN \"I/O Intr\"
epicsThreadSleep 1.5"
    has report && after="$after
streamReportRecord rec"
    cat >"$work/$c.cmd.in" <<EOC
dbLoadDatabase "$STREAM_TOP/dbd/streamApp.dbd"
streamApp_registerRecordDeviceDriver pdbbase
epicsEnvSet STREAM_PROTOCOL_PATH $here
drvAsynIPPortConfigure "P", "127.0.0.1:@PORT@", 0, 0, 0
var streamMsgTimeStamped 0
var streamErrorDeadTime 0
dbLoadRecords "$work/$c.db"
var streamDebug 1
iocInit
$proc
epicsThreadSleep 0.5
$after
var streamDebug 0
$(for f in $shown; do echo "dbgf rec.$f"; done)
exit
EOC
    : >"$work/port"
    "${own[@]}" python3 -I "$here/../rt/dev.py" 0 "$table" >"$work/port" &
    dev=$!
    until [ -s "$work/port" ]; do
        kill -0 "$dev"
        sleep 0.05
    done
    sed "s/@PORT@/$(cat "$work/port")/" "$work/$c.cmd.in" >"$work/$c.cmd"
    EPICS_IOC_IGNORE_SERVERS=rsrv LD_PRELOAD="$work/linewise.so" \
        "${own[@]}" "$STREAM_TOP/bin/$ARCH/streamApp" "$work/$c.cmd" </dev/null >"$work/$c.out" 2>"$work/$c.err" &
    ioc=$!
    wait "$ioc" || true
    ioc=
    kill -- -"$dev" 2>/dev/null || true
    wait "$dev" 2>/dev/null || true
    dev=
    # rsrv's thread cas warns when another server holds the CA port;
    # errlog's thread prints its "sevr=" lines when it runs, kept last,
    # and writes each whole, which may land after the thread name of a
    # line printed in pieces: that name goes back on the line after it
    HERE=$here perl -ne '$_ = $carry . $_; $carry = ""; s/\Q$ENV{HERE}\E/<dir>/g;
        if (s/^(.*?)(sevr=\w+ )/$2/) { $carry = $1; push @e, $_; next } END { print map { "errlog\t$_" } @e }
        next unless $on ||= /Stream(Epics|Core)\.cc:\d+: /;
        # dbgf of an ENUM array, not the device, prints it
        next if /^recGblRecSupError: dbGet missing /;
        next if /^\e\[|^cas WARNING: |^iocsh Error: |^iocRun: |^iocPause: |^iocInit: |^Starting iocInit|^#|StreamCore::attachBus\(/;
        # the thread name, "-" for the lines of drvStreamInit, printed
        # before the thread name hook is set
        next unless s/^(\S+) //;
        $t = $1;
        $t =~ /\.cc?:\d+:$/ and ($_, $t) = ("$t $_", "-");
        # the errlog thread prints its lines with no thread name
        $t = "errlog" if $t eq "recGblRecordError:";
        s/( failed\\n)\S+ (?!Stream(Core|Epics)\.cc:)\S+\.cc?:\d+: .*/$1/;
        next if /^\S+ lockRequest: port \S+ not connected$/;
        s/127\.0\.0\.1:\d+/127.0.0.1:<port>/;
        if (s/^(\S+\.cc?):\d+: //) { print "$t\t$_" if $1 eq "StreamCore.cc" || $1 eq "StreamEpics.cc" } else { print "$t\t$_" }' \
        "$work/$c.err" >"$here/$c.log"
    # dbgf prints "DBF_TYPE:  value" (a string in quotes), dbpf's echo of
    # its put too, and an array "DBF_TYPE[NORD]:  a  b", wrapped at 80
    # columns, kept "[NORD] a b"
    SHOWN=$shown perl -ne 'if (/^DBF_\w+(\[\d+\])?:\s*(.*?)\s*$/) {
            my ($n, $v) = ($1, $2); $v = join " ", $n, split " ", $v if defined $n;
            push @v, $v; $arr = defined $n }
        elsif ($arr && !/^\e/) { $v[-1] = join " ", $v[-1], split " " } else { $arr = 0 }
        END { @f = split " ", $ENV{SHOWN}; @v = @v[-@f..-1];
            print "$f[$_] $v[$_]\n" for 0..$#f }' \
        "$work/$c.out" >>"$here/$c.log"
    # the reload's and the report's lines go to stdout
    if has reload; then
        grep -E '^rec: Protocol reload' "$work/$c.out" >"$here/$c.reload" || true
    fi
    if has report; then
        # to the next command's echo: the protocol has empty lines
        LINK=$link perl -ne 'print if (/^rec: \Q$ENV{LINK}\E$/ .. /^\e\[/) && !/^\e\[/' \
            "$work/$c.out" >"$here/$c.report"
    fi
done <"$here/cases.txt"
