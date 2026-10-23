#!/bin/bash
# Regenerate each <group>.out from its .in with C StreamDevice's
# RegexpConverter (oracle.cc). STREAM_TOP is a StreamDevice top built with
# PCRE_INCLUDE/PCRE_LIB pointing at a libpcre compiled from
# crates/stream-pcre-sys/pcre with the defines in its build.rs; EPICS_BASE
# is the base it was built against.
set -euo pipefail
: "${STREAM_TOP:?set STREAM_TOP to a StreamDevice built with PCRE}"
: "${EPICS_BASE:?set EPICS_BASE}"
ARCH=${EPICS_HOST_ARCH:-linux-x86_64}
cd "$(dirname "$0")"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
g++ -I"$STREAM_TOP/include" -I"$EPICS_BASE/include" \
    -I"$EPICS_BASE/include/os/Linux" -I"$EPICS_BASE/include/compiler/gcc" \
    oracle.cc -o "$work/oracle" \
    -L"$STREAM_TOP/lib/$ARCH" -lstream -L"$EPICS_BASE/lib/$ARCH" -ldbCore -lCom \
    -Wl,-rpath,"$STREAM_TOP/lib/$ARCH:$EPICS_BASE/lib/$ARCH"
for in in *.in; do
    "$work/oracle" <"$in" >"${in%.in}.out" 2>/dev/null
done
