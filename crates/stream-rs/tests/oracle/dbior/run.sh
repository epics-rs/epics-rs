#!/bin/bash
# Regenerate dbior.out from C StreamDevice.
#
# STREAM_TOP is a StreamDevice top built with PCRE and shared libraries
# (link order puts the bus interfaces in CONFIG_STREAM's BUSSES order and
# RegexpConverter registers %/). streamApp boots R1 (ai, p.proto get) and
# R2 (stringout, p.proto put) on bus "dummy" and runs dbior stream at
# interest 0, 1, 2, 3 and 100; kept is the shell's stdout from the first
# "==L" marker on, without the command echo.
set -euo pipefail
: "${STREAM_TOP:?set STREAM_TOP to a built StreamDevice}"
ARCH=${EPICS_HOST_ARCH:-linux-x86_64}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cat >"$work/r.db" <<DB
record(ai, "R1") { field(DTYP, "stream") field(INP, "@p.proto get dummy") }
record(stringout, "R2") { field(DTYP, "stream") field(OUT, "@p.proto put dummy") }
DB
{
    echo "dbLoadDatabase \"$STREAM_TOP/dbd/streamApp.dbd\""
    echo "streamApp_registerRecordDeviceDriver pdbbase"
    echo "epicsEnvSet STREAM_PROTOCOL_PATH $here"
    echo "dbLoadRecords \"$work/r.db\""
    echo "iocInit"
    for l in 0 1 2 3 100; do
        echo "echo ==L$l"
        echo "dbior stream $l"
    done
    echo "echo ==END"
    echo "exit"
} >"$work/st.cmd"
"$STREAM_TOP/bin/$ARCH/streamApp" "$work/st.cmd" </dev/null 2>/dev/null |
    perl -ne '$on = 1 if /^==L/; print if $on && !/\e/' >"$here/dbior.out"
