#!/usr/bin/env bash
# Train the PGO profile for the shipped Linux x86_64 IOC binaries.
#
#   scripts/pgo-train.sh              # 60 s per segment (~75 min)
#   scripts/pgo-train.sh --secs 20    # shorter segments, same workload
#
# Builds -Cprofile-generate copies of realtime-ca-ioc, realtime-pva-ioc and
# softioc-rs, runs each on the training workload below, and merges every
# .profraw into target/pgo/merged.profdata, which scripts/pgo-build.sh reads.
#
# Workload per IOC: each of 10 record/link shapes processed at 10 Hz x 2001
# records, unmonitored and with one monitor per record, a client put stream, a
# calcout DB-link chain, and the monitored-ao rig (MDEL=-1, every cycle posts)
# weighted as much as the other 22 segments together. The mixed segments alone
# lost the rig's gain; the rig alone regressed seq's instruction count.
#
# The IOCs have no shutdown path and the profile runtime writes only from an
# atexit handler, so each IOC is stopped through scripts/pgo/stopshim.c
# (LD_PRELOAD, exit(0) on a stop file); a signalled IOC leaves an empty
# profile.
#
# Needs llvm-profdata from the toolchain's llvm-tools component and a C
# compiler; the script refuses without them and never installs anything.
# Training IOCs serve on private ports (PGO_CA_PORT, PGO_PVA_PORT) bound to
# 127.0.0.1 so a host running other IOCs is not disturbed.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

SECS=60
while [[ $# -gt 0 ]]; do
    case $1 in
        --secs) SECS=$2; shift 2 ;;
        *) echo "usage: scripts/pgo-train.sh [--secs N]" >&2; exit 2 ;;
    esac
done

TRIPLE=x86_64-unknown-linux-gnu
[[ "$(rustc -vV | sed -n 's/^host: //p')" == "$TRIPLE" ]] \
    || { echo "pgo-train.sh: PGO is trained and shipped for $TRIPLE hosts only" >&2; exit 1; }
PROFDATA="$(rustc --print sysroot)/lib/rustlib/$TRIPLE/bin/llvm-profdata"
[[ -x "$PROFDATA" ]] \
    || { echo "pgo-train.sh: no llvm-profdata at $PROFDATA (rustup component add llvm-tools)" >&2; exit 1; }
CC=${CC:-cc}
command -v "$CC" >/dev/null || { echo "pgo-train.sh: no C compiler ($CC) to build scripts/pgo/stopshim.c" >&2; exit 1; }

OUT="$REPO_ROOT/target/pgo"
T="$OUT/train"
rm -rf "$T"
mkdir -p "$T/raw" "$T/dbs"

"$CC" -O2 -shared -fPIC -o "$T/stopshim.so" scripts/pgo/stopshim.c -lpthread

echo "==> Building the instrumented IOCs and the training clients"
# realtime-* ship on the thread exec backend and softioc-rs on the default
# one; each set builds in its own target dir so neither rebuilds the other.
gen() { # gen <target-dir> <cargo build args...>
    local dir=$1; shift
    CARGO_TARGET_DIR="$dir" RUSTFLAGS="-Cprofile-generate=$T/raw" \
        cargo build --release --locked --target "$TRIPLE" "$@"
}
EPICS_RS_BUILD_EXEC_BACKEND=thread gen "$OUT/gen-thread" \
    -p epics-ca-rs --bin realtime-ca-ioc
EPICS_RS_BUILD_EXEC_BACKEND=thread gen "$OUT/gen-thread" \
    -p epics-bridge-rs --features qsrv-core,pvalink,calink-core --bin realtime-pva-ioc
gen "$OUT/gen" -p epics-ca-rs --bin softioc-rs
CARGO_TARGET_DIR="$OUT/tools" cargo build --release --locked \
    -p epics-ca-rs --bin caget-rs --bin caput-rs --bin camonitor-rs
CARGO_TARGET_DIR="$OUT/tools" cargo build --release --locked \
    -p epics-pva-rs --bin pvget-rs --bin pvput-rs --bin pvmonitor-rs
TOOLS="$OUT/tools/release"

# The 2001-record training databases. Each X record runs at 10 Hz; the
# header records are the link sources and sinks the X records name.
header='record(calc, "TRN:SRC") { field(CALC, "VAL+1") field(VAL, "1") }
record(calc, "TRN:SRC2") { field(CALC, "VAL+2") field(VAL, "2") }
record(calc, "TRN:SINK") { field(CALC, "VAL") }
record(calc, "TRN:SINK2") { field(CALC, "VAL") }
record(stringout, "TRN:STRSINK") { }
record(calc, "TRN:WIT0000") { field(SCAN, ".1 second") field(CALC, "VAL+1") }'
declare -A BODY=(
    [ai-nolink]='record(ai, "TRN:X%04d") { field(SCAN, ".1 second") field(INP, "0") field(LINR, "LINEAR") field(ESLO, "2") }'
    [ai-link]='record(ai, "TRN:X%04d") { field(SCAN, ".1 second") field(INP, "TRN:SRC") field(LINR, "LINEAR") field(ESLO, "2") }'
    [calc-nolink]='record(calc, "TRN:X%04d") { field(SCAN, ".1 second") field(CALC, "VAL+1") }'
    [calc-link]='record(calc, "TRN:X%04d") { field(SCAN, ".1 second") field(INPA, "TRN:SRC") field(CALC, "VAL+1+A*0") }'
    [ao]='record(ao, "TRN:X%04d") { field(SCAN, ".1 second") field(VAL, "1") field(OUT, "TRN:SINK") field(LINR, "LINEAR") field(ESLO, "2") }'
    [bo]='record(bo, "TRN:X%04d") { field(SCAN, ".1 second") field(VAL, "1") field(OUT, "TRN:SINK") field(ZNAM, "OFF") field(ONAM, "ON") }'
    [longout]='record(longout, "TRN:X%04d") { field(SCAN, ".1 second") field(VAL, "1") field(OUT, "TRN:SINK") }'
    [stringout]='record(stringout, "TRN:X%04d") { field(SCAN, ".1 second") field(VAL, "train") field(OUT, "TRN:STRSINK") }'
    [dfanout]='record(dfanout, "TRN:X%04d") { field(SCAN, ".1 second") field(DOL, "TRN:SRC") field(OMSL, "closed_loop") field(OUTA, "TRN:SINK") field(OUTB, "TRN:SINK2") }'
    [seq]='record(seq, "TRN:X%04d") { field(SCAN, ".1 second") field(DOL1, "TRN:SRC") field(LNK1, "TRN:SINK") field(DOL2, "TRN:SRC2") field(LNK2, "TRN:SINK2") field(SELM, "All") }'
    [ao-post]='record(ao, "TRN:X%04d") { field(SCAN, ".1 second") field(MDEL, "-1") field(VAL, "1") field(OUT, "TRN:SINK") field(LINR, "LINEAR") field(ESLO, "2") }'
    [calcout]='record(calcout, "TRN:X%04d") { field(SCAN, ".1 second") field(CALC, "A+1") field(INPA, "TRN:SRC") field(OUT, "TRN:SINK") field(OOPT, "Every Time") }'
)
NREC=2001
for k in "${!BODY[@]}"; do
    # shellcheck disable=SC2059 # the body is the format: %04d is the record index
    { echo "$header"; for ((i = 0; i < NREC; i++)); do printf "${BODY[$k]}\n" "$i"; done; } > "$T/dbs/$k.db"
done
NAMES=$(for ((i = 0; i < NREC; i++)); do printf 'TRN:X%04d ' "$i"; done)

export EPICS_CA_SERVER_PORT=${PGO_CA_PORT:-15464} EPICS_CA_REPEATER_PORT=$((${PGO_CA_PORT:-15464} + 1))
export EPICS_CA_ADDR_LIST=127.0.0.1 EPICS_CA_AUTO_ADDR_LIST=NO
export EPICS_PVA_SERVER_PORT=${PGO_PVA_PORT:-15475} EPICS_PVA_BROADCAST_PORT=$((${PGO_PVA_PORT:-15475} + 1))
export EPICS_PVAS_BROADCAST_PORT=$EPICS_PVA_BROADCAST_PORT
export EPICS_PVA_ADDR_LIST=127.0.0.1 EPICS_PVA_AUTO_ADDR_LIST=NO

pid='' cpid='' ppid_=''
trap 'kill $ppid_ $cpid $pid 2>/dev/null || true' EXIT

# segment <ioc-bin> <ca|pva> <db> <mon 0|1> <put 0|1> <tag>
segment() {
    local bin=$1 db=$3 mon=$4 put=$5 tag=$6 n=0 stop="$T/$6.stop" get=caget-rs camon=camonitor-rs cput=caput-rs
    [[ $2 == pva ]] && get=pvget-rs camon=pvmonitor-rs cput=pvput-rs
    case $(basename "$bin") in
        softioc-rs) set -- "$bin" -S -d "$db" ;;
        *) set -- "$bin" "$db" ;;
    esac
    LD_PRELOAD="$T/stopshim.so" PGO_STOP="$stop" LLVM_PROFILE_FILE="$T/raw/$tag-%p.profraw" \
        "$@" > "$T/$tag.log" 2>&1 < /dev/null &
    pid=$!
    until "$TOOLS/$get" -w 1 TRN:WIT0000 > /dev/null 2>&1; do
        n=$((n + 1))
        if ! kill -0 "$pid" 2> /dev/null || ((n > 60)); then
            echo "pgo-train.sh: $tag: IOC never answered (see $T/$tag.log)" >&2; exit 1
        fi
        sleep 0.5
    done
    if ((mon)); then
        # shellcheck disable=SC2086 # one argument per record name
        "$TOOLS/$camon" $NAMES > /dev/null 2>&1 &
        cpid=$!
    fi
    if ((put)); then
        ( end=$((SECONDS + SECS)); k=0
          while ((SECONDS < end)); do
              for ((i = 0; i < NREC; i += 7)); do
                  "$TOOLS/$cput" "$(printf 'TRN:X%04d' "$i")" "$k" > /dev/null 2>&1 || true
                  ((SECONDS < end)) || break
              done
              k=$((k + 1))
          done ) &
        ppid_=$!
    fi
    sleep "$SECS"
    [[ -n $ppid_ ]] && { wait $ppid_ || true; ppid_=; }
    [[ -n $cpid ]] && { kill $cpid 2>/dev/null || true; wait $cpid 2>/dev/null || true; cpid=; }
    touch "$stop"
    for ((n = 0; n < 60; n++)); do kill -0 $pid 2>/dev/null || break; sleep 0.5; done
    if kill -0 $pid 2>/dev/null; then
        echo "pgo-train.sh: $tag: IOC did not exit through the stop shim; its profile is lost" >&2; exit 1
    fi
    wait $pid || true
    pid=
    echo "    $tag"
}

start=$SECONDS
for ioc in "$OUT/gen-thread/$TRIPLE/release/realtime-ca-ioc:ca" \
           "$OUT/gen-thread/$TRIPLE/release/realtime-pva-ioc:pva" \
           "$OUT/gen/$TRIPLE/release/softioc-rs:ca"; do
    bin=${ioc%:*} proto=${ioc##*:} name=$(basename "${ioc%:*}")
    echo "==> Training $name ($SECS s per segment)"
    for mon in 0 1; do
        for k in ai-nolink ai-link calc-nolink calc-link ao bo longout stringout dfanout seq; do
            segment "$bin" "$proto" "$T/dbs/$k.db" $mon 0 "$name-$k-m$mon"
        done
    done
    segment "$bin" "$proto" "$T/dbs/ao.db" 0 1 "$name-put"
    segment "$bin" "$proto" "$T/dbs/calcout.db" 0 0 "$name-dblink"
    segment "$bin" "$proto" "$T/dbs/ao-post.db" 1 0 "$name-rig"
done

inputs=()
for f in "$T"/raw/*.profraw; do
    case $f in *-rig-*) inputs+=("-weighted-input=22,$f") ;; *) inputs+=("$f") ;; esac
done
"$PROFDATA" merge -o "$OUT/merged.profdata" "${inputs[@]}"
echo "==> Training took $((SECONDS - start)) s; profile: $OUT/merged.profdata ($(du -h "$OUT/merged.profdata" | cut -f1))"
