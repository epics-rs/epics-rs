#!/bin/bash
# Regenerate each <converter>.out from its .in with C StreamDevice's
# converters (oracle.cc); a *_debug.in runs with streamDebug 2. STREAM_TOP
# is a built StreamDevice top (include/ and lib/$ARCH/libstream),
# EPICS_BASE the base it was built against.
set -euo pipefail
: "${STREAM_TOP:?set STREAM_TOP to a built StreamDevice}"
: "${EPICS_BASE:?set EPICS_BASE}"
ARCH=${EPICS_HOST_ARCH:-linux-x86_64}
# TimestampConverter prints and scans local time; tests/timestamp_converter.rs
# sets the same zone.
export TZ=CET-1CEST,M3.5.0,M10.5.0/3
cd "$(dirname "$0")"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
g++ -I"$STREAM_TOP/include" -I"$EPICS_BASE/include" \
    -I"$EPICS_BASE/include/os/Linux" -I"$EPICS_BASE/include/compiler/gcc" \
    oracle.cc -o "$work/oracle" \
    -L"$STREAM_TOP/lib/$ARCH" -lstream -L"$EPICS_BASE/lib/$ARCH" -ldbCore -lCom \
    -Wl,-rpath,"$STREAM_TOP/lib/$ARCH:$EPICS_BASE/lib/$ARCH"
for in in *.in; do
    case $in in
    *_debug.in) debug=2 ;;
    *) debug= ;;
    esac
    STREAM_DEBUG=$debug "$work/oracle" <"$in" >"${in%.in}.out" 2>/dev/null
done
