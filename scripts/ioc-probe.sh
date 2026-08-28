#!/usr/bin/env bash
# Run one IOC under a single-instance guarantee, so a live probe measures the
# process it started and nothing else.
#
# WHY THIS EXISTS.  A probe that reads sockets or process state cannot tell a
# running IOC from a corpse of an earlier one still holding the port, and a
# stale listener does not announce itself: it makes a socket look present, a
# search look answered and a race look closed.  The way the corpses got there
# was one shell idiom --
#
#     ( cd "$dir" && "$bin" "$script" & echo $! > pid )
#
# `cd X && cmd &` backgrounds the LIST, which bash runs in a subshell, so `$!`
# is the subshell's pid.  Killing it leaves the IOC running, and the next
# round's `ss ... | grep :$PORT` counts every survivor.  That is how a census
# reported this port binding one more UDP socket than C when it binds one
# fewer.
#
# THE FIX IS STRUCTURAL, not a reminder.  Every start here goes through
# `ioc_start`, which backgrounds a subshell that `exec`s the binary -- the
# subshell is replaced, so the pid held IS the process.  Every start is
# bracketed by `ioc_require_clear`, which refuses to measure while anything
# holds the ports, and every reap re-asserts it.  A harness that cannot prove
# single instance before it measures must refuse to measure.
#
# USAGE
#   ioc-probe.sh run     [opts] -- <bin> [args...]   run to exit, echo rc
#   ioc-probe.sh census  [opts] -- <bin> [args...]   hold it up, list ITS sockets
#   ioc-probe.sh clear   [opts]                      assert the ports are free
#
# OPTIONS
#   --port N        a port the IOC is expected to own (repeatable)
#   --settle SEC    seconds to wait before censusing        (default 3)
#   --timeout SEC   seconds before the run is killed        (default 20)
#   --out FILE      stdout destination                      (default /dev/stdout)
#   --err FILE      stderr destination                      (default /dev/stderr)
#   --dir DIR       working directory for the IOC
#   --expect-listen assert the IOC itself owns every --port (census only)
#
# EXIT
#   0 ok  |  2 refused: ports were not clear, or a reap left them held
#   3 the IOC did not own a port it was required to  |  else the IOC's own rc
set -uo pipefail

PORTS=()
SETTLE=3
RUN_TIMEOUT=20
OUT=/dev/stdout
ERR=/dev/stderr
DIR=
EXPECT_LISTEN=0

die() { printf 'ioc-probe: %s\n' "$*" >&2; }

# Every socket on one port, with the owning pid, both protocols.
holders() {
    local port=$1
    { ss -lunp 2>/dev/null; ss -ltnp 2>/dev/null; } | grep -E "[:.]${port}[[:space:]]" || true
}

# Pids in a holders() listing.
holder_pids() {
    holders "$1" | grep -oE 'pid=[0-9]+' | cut -d= -f2 | sort -u
}

# MUST hold before any measurement, and again after every reap.
ioc_require_clear() {
    local when=$1 bad=0 port
    for port in "${PORTS[@]:-}"; do
        [ -n "$port" ] || continue
        local h
        h=$(holders "$port")
        if [ -n "$h" ]; then
            bad=1
            die "REFUSING TO MEASURE ($when): port $port is held --"
            printf '%s\n' "$h" | sed 's/^/  /' >&2
        fi
    done
    if [ "$bad" = 1 ]; then
        die "a live probe against a held port measures the holder, not the IOC."
        die "reap the survivors (pgrep -a softIoc; pgrep -a softioc-rs) and retry."
        return 2
    fi
    return 0
}

# Start the IOC so the pid returned IS the process.
#
# `( ... exec "$@" ) &` -- the subshell is REPLACED by the binary, so `$!` is
# the binary.  Backgrounding a compound list instead is the whole defect this
# file exists to make unrepeatable.
ioc_start() {
    if [ -n "$DIR" ]; then
        ( cd "$DIR" && exec "$@" ) > "$OUT" 2> "$ERR" < /dev/null &
    else
        ( exec "$@" ) > "$OUT" 2> "$ERR" < /dev/null &
    fi
    IOC_PID=$!
}

# Terminate the IOC and prove nothing of it is left on the ports.
ioc_reap() {
    local pid=$1
    if kill -0 "$pid" 2>/dev/null; then
        kill -TERM "$pid" 2>/dev/null
        for _ in $(seq 1 20); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.25
        done
        kill -0 "$pid" 2>/dev/null && kill -KILL "$pid" 2>/dev/null
    fi
    wait "$pid" 2>/dev/null
    sleep 0.5
    ioc_require_clear "after reap of pid $pid" || return 2
    return 0
}

cmd=${1:-}; shift || true
while [ $# -gt 0 ]; do
    case $1 in
        --port)    PORTS+=("$2"); shift 2 ;;
        --settle)  SETTLE=$2; shift 2 ;;
        --timeout) RUN_TIMEOUT=$2; shift 2 ;;
        --out)     OUT=$2; shift 2 ;;
        --err)     ERR=$2; shift 2 ;;
        --dir)     DIR=$2; shift 2 ;;
        --expect-listen) EXPECT_LISTEN=1; shift ;;
        --)        shift; break ;;
        *)         die "unknown option: $1"; exit 64 ;;
    esac
done

case $cmd in
clear)
    ioc_require_clear "on request" || exit 2
    echo "ports clear: ${PORTS[*]:-none named}"
    ;;

run)
    [ $# -gt 0 ] || { die "run needs -- <bin> [args...]"; exit 64; }
    ioc_require_clear "before start" || exit 2
    ioc_start timeout -k 2 "$RUN_TIMEOUT" "$@"
    wait "$IOC_PID"; rc=$?
    sleep 0.5
    ioc_require_clear "after run of pid $IOC_PID" || exit 2
    echo "$rc"
    ;;

census)
    [ $# -gt 0 ] || { die "census needs -- <bin> [args...]"; exit 64; }
    ioc_require_clear "before start" || exit 2
    ioc_start "$@"
    pid=$IOC_PID
    sleep "$SETTLE"
    if ! kill -0 "$pid" 2>/dev/null; then
        die "the IOC (pid $pid) exited before the census; see $ERR"
        ioc_require_clear "after early exit" || exit 2
        exit 3
    fi
    # ITS sockets, selected by pid, never by port text -- selecting by port is
    # what let another process's socket into the count.
    echo "# pid $pid"
    { ss -lunp 2>/dev/null; ss -ltnp 2>/dev/null; } \
        | grep -E "pid=${pid}," | sed 's/[[:space:]]\+/ /g' | sed 's/^/  /'
    rc=0
    if [ "$EXPECT_LISTEN" = 1 ]; then
        for port in "${PORTS[@]:-}"; do
            [ -n "$port" ] || continue
            if [ "$(holder_pids "$port")" != "$pid" ]; then
                die "port $port is NOT owned solely by pid $pid --"
                holders "$port" | sed 's/^/  /' >&2
                rc=3
            fi
        done
    fi
    ioc_reap "$pid" || exit 2
    exit $rc
    ;;

*)
    sed -n '2,45p' "$0" >&2
    exit 64
    ;;
esac
