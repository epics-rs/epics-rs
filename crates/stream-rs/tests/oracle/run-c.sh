#!/bin/bash
# Regenerate the <case>.stderr files from C StreamDevice.
#
# STREAM_TOP is a built StreamDevice top (bin/$ARCH/streamApp and
# dbd/streamApp.dbd). Each case boots one IOC whose records, one per line of
# cases.txt, load a protocol with DTYP stream; a "@record NAME" line adds an
# ai record for %(NAME.VAL) or %(NAME.RVAL) to find. What the parser printed
# during iocInit is kept: stderr without iocInit's own lines, without the
# @init handlers' failures to lock the unconnected port, and without the
# messages StreamEpics wraps StreamCore's in ("<rec>: Protocol parse
# error"). Run from
# the crate root, so the protocol path the messages quote is the rs test's.
#
# A real_* or ok_* case boots a second time with streamDebug 2, for <case>.compiled:
# per compiled command list, in record order, its handler ("(null)" for the
# protocol's own commands), a tab, and the code as StreamBuffer::expand
# prints it. Struct padding and addresses in the code are stack garbage, and
# a garbage '<' before two hex digits and '>' reads back as an expanded
# byte, so STREAM_TOP is built with expand.patch, which expands '<' too.
#
# The same cases boot a third time to run streamReportRecord, for
# <case>.report: per record, its header line and what printProtocol printed
# (the printStatus line between them is the bus's too, and is left out).
set -euo pipefail
: "${STREAM_TOP:?set STREAM_TOP to a built StreamDevice}"
ARCH=${EPICS_HOST_ARCH:-linux-x86_64}
cd "$(dirname "$0")/../.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
for c in $(awk '!/^#/ {print $1}' tests/oracle/cases.txt | uniq); do
    : >"$work/$c.db"
    n=0
    while read -r case file proto; do
        [ "$case" = "$c" ] || continue
        if [ "$file" = @record ]; then
            printf 'record(ai, "%s") {}\n' "$proto" >>"$work/$c.db"
            continue
        fi
        printf 'record(stringout, "rec%d") {\n  field(DTYP, "stream")\n  field(OUT, "@%s %s P")\n}\n' \
            "$n" "$file" "$proto" >>"$work/$c.db"
        n=$((n + 1))
    done < <(grep -v '^#' tests/oracle/cases.txt)
    cat >"$work/$c.cmd" <<EOF
dbLoadDatabase "$STREAM_TOP/dbd/streamApp.dbd"
streamApp_registerRecordDeviceDriver pdbbase
epicsEnvSet STREAM_PROTOCOL_PATH tests/oracle:tests/none;tests/proto
drvAsynIPPortConfigure "P", "127.0.0.1:9", 0, 1, 0
var streamMsgTimeStamped 0
#debug
dbLoadRecords "$work/$c.db"
iocInit
exit
EOF
    "$STREAM_TOP/bin/$ARCH/streamApp" "$work/$c.cmd" </dev/null >/dev/null 2>"$work/$c.raw" || true
    perl -0pe "s/_main_ rec\\d+: (Protocol parse error|Record initialization failed|Can't start \@init handler)\\n//g; s/_main_ rec\\d+ lockRequest: port P not connected\\n//g; s/^(Starting iocInit|iocRun: All initialization complete)\\n//mg" \
        "$work/$c.raw" >"tests/oracle/$c.stderr"
    case $c in real_* | ok_*) ;; *) continue ;; esac
    sed -i 's/^#debug$/var streamDebug 2/' "$work/$c.cmd"
    "$STREAM_TOP/bin/$ARCH/streamApp" "$work/$c.cmd" </dev/null >/dev/null 2>"$work/$c.debug" || true
    perl -ne 'if (/^_main_ StreamProtocol\.cc:\d+: commands (\S+): /) { $h = $1 }
        elsif (/^_main_ StreamProtocol\.cc:\d+: compiled to: (.*)$/) { print "$h\t$1\n" }' \
        "$work/$c.debug" >"tests/oracle/$c.compiled"
    sed -i 's/^var streamDebug 2$/#debug/; s/^iocInit$/iocInit\nstreamReportRecord "rec*"/' "$work/$c.cmd"
    "$STREAM_TOP/bin/$ARCH/streamApp" "$work/$c.cmd" </dev/null >"$work/$c.out" 2>/dev/null || true
    perl -ne 'if (/^rec\d+: /) { print; $s = 1; next }
        if ($s == 1) { $s = 2; next }
        if ($s == 2) { print; $s = 0 if /^}$/ }' \
        "$work/$c.out" >"tests/oracle/$c.report"
done
