#!/bin/bash
# bandladder-e10.sh <ram>... — classify each guest RAM into one of the three
# start-up outcomes, by asking the kernel rather than the console.
#
# The 768M and 832M arms both look identical from outside: `rtpSp` returns a
# valid id and the console stays empty.  ED&R says they are not the same
# failure at all, so the ladder records, per arm:
#
#   LOADFAIL   rtpLib.c "RTP failed loading its segments" — main() never ran
#   ABORT      rtpSigLib.c abnormal termination — the RTP ran and abort()ed
#   RUNNING    rtpShow lists it STATE_NORMAL with its task count
set -o pipefail
R=$HOME/vx-rig-e10
IMG=${VXIMG:-ca-e11}

for RAM in "$@"; do
    L=$R/logs/nostart-band$RAM.console.log
    VXIMG=$IMG bash "$R/nostart-e10.sh" "$RAM" "band$RAM" > /dev/null 2>&1
    OSMEM=$(grep -o 'OS Memory Size: *~[0-9]*MB' "$L" | head -1)
    if grep -q 'failed loading its segments' "$L"; then
        VERDICT="LOADFAIL $(grep -o 'errno = 0x[0-9a-f]*' "$L" | head -1)"
    elif grep -q 'Abnormal termination of RTP' "$L"; then
        VERDICT="ABORT $(grep -o 'Injection Point: *rtp[A-Za-z]*\.c:[0-9]*' "$L" | tail -1)"
    elif grep -q 'STATE_NORMAL' "$L"; then
        VERDICT="RUNNING tasks=$(grep -m1 STATE_NORMAL "$L" | awk '{print $NF}')"
    else
        VERDICT="UNCLASSIFIED"
    fi
    AS=$(grep -o 'RTP Address Space: *0x[0-9a-f]* -> 0x[0-9a-f]*' "$L" | tail -1)
    echo "ram=$RAM  $OSMEM  $VERDICT  $AS  console=$(wc -c < "$L")B"
done
