#!/bin/bash
# Regenerate bare.out from C StreamDevice: streamApp, as run.sh builds it,
# with no records, runs a bare dbior at interest 0 and 1 — every driver in
# streamApp.dbd's drvList order (asyn.dbd's drvAsyn, then stream.dbd's
# stream) — and dbDumpDevice ai's device names into bare-ai.devices,
# asyn.dbd's rows before stream.dbd's.
set -euo pipefail
: "${STREAM_TOP:?set STREAM_TOP to a built StreamDevice}"
ARCH=${EPICS_HOST_ARCH:-linux-x86_64}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
{
    echo "dbLoadDatabase \"$STREAM_TOP/dbd/streamApp.dbd\""
    echo "streamApp_registerRecordDeviceDriver pdbbase"
    echo "iocInit"
    echo "echo ==L0"
    echo "dbior"
    echo "echo ==L1"
    echo "dbior \"\" 1"
    echo "echo ==END"
    echo "dbDumpDevice pdbbase ai"
    echo "exit"
} >"$work/st.cmd"
"$STREAM_TOP/bin/$ARCH/streamApp" "$work/st.cmd" </dev/null 2>/dev/null |
    perl -ne '$on = 1 if /^==L/; print if $on && !/\e/' >"$work/out"
sed -n '/^==L/,/^==END/p' "$work/out" >"$here/bare.out"
sed -n 's/^ *device name: *//p' "$work/out" >"$here/bare-ai.devices"
