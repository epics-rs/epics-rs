#!/bin/bash
# Regenerate bytes.out from C StreamDevice.
#
# STREAM_TOP is run.sh's StreamDevice top with HighConverter.cc (here) added
# to src and "FORMATS += High" to src/CONFIG_STREAM, which registers a
# converter on conversion byte 0xE9. streamApp boots R3 (stringout,
# h.proto hi, whose string holds a raw 0xE9 and an escaped \xe9) on bus
# "dummy" and runs dbior stream at interest 1 and 2; kept is the shell's
# stdout from the first "==L" marker on, without the command echo.
set -euo pipefail
: "${STREAM_TOP:?set STREAM_TOP to a built StreamDevice}"
ARCH=${EPICS_HOST_ARCH:-linux-x86_64}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cat >"$work/r.db" <<DB
record(stringout, "R3") { field(DTYP, "stream") field(OUT, "@h.proto hi dummy") }
DB
{
    echo "dbLoadDatabase \"$STREAM_TOP/dbd/streamApp.dbd\""
    echo "streamApp_registerRecordDeviceDriver pdbbase"
    echo "epicsEnvSet STREAM_PROTOCOL_PATH $here"
    echo "dbLoadRecords \"$work/r.db\""
    echo "iocInit"
    for l in 1 2; do
        echo "echo ==L$l"
        echo "dbior stream $l"
    done
    echo "echo ==END"
    echo "exit"
} >"$work/st.cmd"
"$STREAM_TOP/bin/$ARCH/streamApp" "$work/st.cmd" </dev/null 2>/dev/null |
    perl -ne '$on = 1 if /^==L/; print if $on && !/\e/' >"$here/bytes.out"
