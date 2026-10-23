#!/bin/bash
# Regenerate the asyn/<case>.log files from C StreamDevice.
#
# STREAM_TOP is a clean StreamDevice top. Each case boots streamApp with
# an asyn IP port "P" to dev.py (../rt/dev.py, given the case's device
# table) or to a port nobody listens on, and keeps AsynDriverInterface.cc's
# debug lines of stderr without the thread name and file:line prefix,
# pointers masked as 0x*, and the bus's error lines (those naming one of
# its functions, and connectToBus's EOS warning):
#   attach-connected, attach-disconnected, attach-noport: record "rec" of
#     a.proto attached at iocInit to P with and without a listener, and to
#     a port Q that does not exist; every line.
#   noeos: "rec" reloaded (streamReload) on P configured with noProcessEos,
#     whose getInputEos fails, so the protocol parsed at iocInit makes
#     connectToBus warn; every line.
#   lock: "rec" processed once; the lockRequest, lockHandler and unlock
#     lines.
#   locktimeout: "first" (lt.proto hold) owns P while waiting for a reply
#     that never comes, "second" (lt.proto wait) waits for P past its
#     LockTimeout; the lockRequest, lockHandler, handleTimeout and unlock
#     lines.
#   write: "rec" processed once; the writeRequest and writeHandler lines.
#   flush: as write, with dev.py greeting on connect, so writeHandler
#     flushes that early input; the writeHandler lines.
#   read, noreply, readtimeout: "rec" processed once against a reply, no
#     reply, and a reply without its terminator; the readRequest and
#     readHandler lines.
#   eos: as read with a.proto e3, whose 3-byte terminator asyn takes only
#     its last 2 bytes of; streamDebug 2, the readHandler lines.
#   eosrestore: as read with P's input EOS set to LF before iocInit;
#     streamDebug 2, the readHandler lines.
#   split: as read with a.proto s and an empty "\r\n" reply, whose CR the
#     one-byte peek delivers and asyn's EOS layer takes back (received=-1);
#     streamDebug 2, the readHandler lines.
#   splitrestore: as eos with an "\n\r" reply split the same way, so the
#     restored terminator starts before the buffer; streamDebug 2, the
#     readHandler lines.
#   connect: "rec" (a.proto cd) disconnects P, connects it again and
#     reads; the connect, disconnect and finish lines.
#   intr: "rec" (a.proto i) scanned "I/O Intr", dev.py greeting "1.5\r" on
#     connect; the asynReadHandler lines.
#   reinit: "rec" attached to P, then `streamReinit P`; the
#     exceptionHandler lines.
#   reinit-fail: P with no listener, then `streamReinit` with no port, on
#     a port Z that does not exist, and on P; streamReinit's own stderr
#     lines as they are (they have no prefix).
#   reconnect: "rec" attached to P with no listener and processed, so its
#     lock is refused; the listener then starts and asyn's autoconnect,
#     20 s on, connects P; the lockRequest and exceptionHandler lines.
#   syncrefused: "rec" on a synchronous port E (asyn's echoDriver, delay
#     0) processed once; the lockRequest and lockHandler lines. This case
#     runs $STREAM_TOP/bin/$ARCH/streamEcho, streamApp's dbd files plus
#     calcSupport.dbd and a `registrar(echoDriverRegister)` dbd, linked
#     with asyn's testSupport library.
# PORT sets the TCP port (default 27090), DEBUG the streamDebug level the
# cases other than eos and eosrestore use (default 1). Arguments, if any, are the cases
# to run.
set -euo pipefail
: "${STREAM_TOP:?set STREAM_TOP to a built StreamDevice}"
ARCH=${EPICS_HOST_ARCH:-linux-x86_64}
here=$(cd "$(dirname "$0")" && pwd)
port=${PORT:-27090}
work=$(mktemp -d)
dev=
trap '[ -n "$dev" ] && kill "$dev" 2>/dev/null; rm -rf "$work"' EXIT

# boot CASE TABLE DB PROC [APP PORTCMD DELAY SLEEP]: run APP (streamApp) on
# DB with PORTCMD (P to dev.py) and PROC after iocInit; dev.py, given TABLE
# (- for none), starts DELAY seconds before or, negative, after the IOC
start_dev() {
    python3 -I "$here/../rt/dev.py" "$port" "$1" &
    dev=$!
}
boot() {
    local c=$1 table=$2 db=$3 proc=$4 app=${5:-streamApp}
    local portcmd=${6:-"drvAsynIPPortConfigure \"P\", \"127.0.0.1:$port\", 0, 0, 0"}
    local delay=${7:-0} sleep=${8:-1.5} debug_early= debug_late="var streamDebug ${DEBUG:-1}"
    [ -z "$proc" ] || [ "$delay" -lt 0 ] && debug_early=$debug_late debug_late=
    printf '%s\n' "$db" >"$work/$c.db"
    cat >"$work/$c.cmd" <<EOF
dbLoadDatabase "$STREAM_TOP/dbd/$app.dbd"
${app}_registerRecordDeviceDriver pdbbase
epicsEnvSet STREAM_PROTOCOL_PATH $here
$portcmd
var streamMsgTimeStamped 0
dbLoadRecords "$work/$c.db"
$debug_early
iocInit
$debug_late
$proc
epicsThreadSleep $sleep
var streamDebug 0
exit
EOF
    if [ "$table" != - ] && [ "$delay" -ge 0 ]; then
        start_dev "$table"
        until python3 -c "import socket,sys; sys.exit(socket.socket().connect_ex(('127.0.0.1',$port)))"; do
            kill -0 "$dev"
            sleep 0.05
        done
    fi
    "$STREAM_TOP/bin/$ARCH/$app" "$work/$c.cmd" </dev/null >/dev/null 2>"$work/$c.err" &
    local ioc=$!
    if [ "$delay" -lt 0 ]; then
        sleep $((-delay))
        start_dev "$table"
    fi
    wait $ioc || true
    if [ -n "$dev" ]; then kill "$dev" 2>/dev/null || true; wait "$dev" 2>/dev/null || true; dev=; fi
}

# keep CASE REGEX: the case's bus lines matching REGEX
keep() {
    perl -ne 'next unless s/^\S+ //;
        if (/^\S+\.cc?:\d+: /) { next unless s/^AsynDriverInterface\.cc:\d+: // }
        else { next unless /^\S+ \w+: (pasynManager->|port )|^\S+: warning: No input EOS/ }
        s/0x[0-9a-f]+/0x*/g; print if /'"$2"'/' "$work/$1.err" >"$here/$1.log"
}

# keep_raw CASE REGEX: the case's stderr lines matching REGEX, as they are
keep_raw() { grep -E "$2" "$work/$1.err" >"$here/$1.log"; }

rec() { printf 'record(%s, "%s") { field(DTYP, "stream") field(%s, "@%s P") }' "$@"; }

for c in ${@:-attach-connected attach-disconnected attach-noport noeos lock locktimeout write flush read noreply readtimeout eos eosrestore split splitrestore connect intr reinit reinit-fail reconnect syncrefused}; do
    case $c in
    attach-connected) boot $c 'Q=>' "$(rec ai rec INP 'a.proto p')" "" && keep $c . ;;
    attach-disconnected) boot $c - "$(rec ai rec INP 'a.proto p')" "" && keep $c . ;;
    attach-noport) boot $c - "$(rec ai rec INP 'a.proto p' | sed 's/ P"/ Q"/')" "" && keep $c . ;;
    noeos)
        boot $c 'Q=>' "$(rec ai rec INP 'a.proto p')" "streamReload rec" streamApp \
            "drvAsynIPPortConfigure \"P\", \"127.0.0.1:$port\", 0, 0, 1"
        keep $c . ;;
    lock)
        boot $c 'X=>1\r' "$(rec ai rec INP 'a.proto p')" "dbpf rec.PROC 1"
        keep $c '::(lockRequest|lockHandler|unlock)\(' ;;
    locktimeout)
        boot $c 'Q=>' "$(rec ai first INP 'lt.proto hold'; echo; rec ao second OUT 'lt.proto wait')" \
            "dbpf first.PROC 1
epicsThreadSleep 0.1
dbpf second.PROC 1"
        keep $c '::(lockRequest|lockHandler|handleTimeout|unlock)\(' ;;
    write)
        boot $c 'X=>1\r' "$(rec ai rec INP 'a.proto p')" "dbpf rec.PROC 1"
        keep $c '::(writeRequest|writeHandler)\(' ;;
    flush)
        boot $c '@=>early\r;;X=>1\r' "$(rec ai rec INP 'a.proto p')" "dbpf rec.PROC 1"
        keep $c '::writeHandler\(' ;;
    read | noreply | readtimeout)
        case $c in read) t='X=>1.5\r' ;; noreply) t='Q=>' ;; *) t='X=>12' ;; esac
        boot $c "$t" "$(rec ai rec INP 'a.proto p')" "dbpf rec.PROC 1" streamApp "" 0 2
        keep $c '::(readRequest|readHandler)\(|::readRequest ' ;;
    eos)
        DEBUG=2 boot $c 'X=>1\r\n\r' "$(rec ai rec INP 'a.proto e3')" "dbpf rec.PROC 1"
        keep $c '::readHandler\(' ;;
    eosrestore)
        DEBUG=2 boot $c 'X=>1\r' "$(rec ai rec INP 'a.proto p')" "dbpf rec.PROC 1" streamApp \
            "drvAsynIPPortConfigure \"P\", \"127.0.0.1:$port\", 0, 0, 0
asynOctetSetInputEos P 0 \"\\n\""
        keep $c '::readHandler\(' ;;
    split)
        DEBUG=2 boot $c 'X=>\r\n' "$(rec stringin rec INP 'a.proto s')" "dbpf rec.PROC 1"
        keep $c '::readHandler\(' ;;
    splitrestore)
        DEBUG=2 boot $c 'X=>\n\r' "$(rec ai rec INP 'a.proto e3')" "dbpf rec.PROC 1"
        keep $c '::readHandler\(' ;;
    connect)
        boot $c 'X=>1\r' "$(rec ai rec INP 'a.proto cd')" "dbpf rec.PROC 1" streamApp "" 0 2
        keep $c '::(connectRequest|connectToAsynPort|disconnectRequest|disconnectHandler|finish)\(|::(dis)?connectRequest |::disconnectHandler |(dis)?connectRequest: |exceptionHandler\(' ;;
    intr)
        boot $c '@=>1.5\r' "$(rec ai rec INP 'a.proto i' | sed 's|) }$|) field(SCAN, "I/O Intr") }|')" ""
        keep $c '::asynReadHandler\(' ;;
    reinit)
        boot $c 'Q=>' "$(rec ai rec INP 'a.proto p')" "streamReinit P"
        keep $c '::exceptionHandler\(' ;;
    reinit-fail)
        boot $c - "" "streamReinit
streamReinit Z
streamReinit P"
        keep_raw $c '^(Usage: streamReinit|asynManager:|P addr )' ;;
    reconnect)
        boot $c 'X=>1\r' "$(rec ai rec INP 'a.proto p')" "dbpf rec.PROC 1" streamApp "" -2 26
        keep $c '::(lockRequest|exceptionHandler)\(|lockRequest: ' ;;
    syncrefused)
        boot $c - "$(rec ai rec INP 'a.proto p' | sed 's/ P"/ E"/')" "dbpf rec.PROC 1" \
            streamEcho 'echoDriverInit("E", 0, 0, 0)'
        keep $c '::(lockRequest|lockHandler)\(|lockHandler: ' ;;
    *) echo "no case $c" >&2; exit 1 ;;
    esac
done
