#!/bin/bash
# nostart-e10.sh <guest-ram> <label> — why does the RTP load but print nothing?
#
# At 768M and 832M `rtpSp` returns a valid RTP id and the console then stays
# empty for the whole run: no banner, no panic, no ED&R line on the console.
# The console is the wrong witness for that, because the RTP's stdout is not
# what fails first — so this arm asks the KERNEL instead.  The kernel shell is
# a separate task, survives whatever the RTP does, and answers:
#
#   memShow      how much of the ~OS memory the kernel itself kept
#   rtpShow      whether an RTP object still exists a minute after rtpSp
#   edrShow      the ED&R record, which is where a fatal RTP fault lands even
#                when nothing reached the console
#   rtpMemShow   the RTP's own memory context, if the RTP is still there
#
# Everything is asked twice, ten seconds either side, so "the RTP existed and
# then did not" is distinguishable from "the RTP never got that far".
set -o pipefail
R=$HOME/vx-rig-e10
RAM=$1
LABEL=$2
[ -n "$RAM" ] && [ -n "$LABEL" ] || { echo "usage: $0 <ram> <label>"; exit 1; }
IMG=${VXIMG:-ca-e11}
CONSOLE=$R/logs/console-$IMG.log

"$R/stop-e10.sh" > /dev/null 2>&1
rm -f "$CONSOLE"
VXRAM=$RAM nohup "$R/boot-e10.sh" "$IMG" 320 > "$R/logs/nostart-$LABEL.boot.log" 2>&1 &
BOOTPID=$!
echo "nostart $LABEL ram=$RAM image=$IMG bootpid=$BOOTPID"

for _ in $(seq 120); do [ -p "$R/con.in" ] && break; sleep 1; done
# boot-e10.sh issues the rtpSp at ~t+27s; give it another 20 s to fail.
sleep 60
{
  echo ""
  sleep 3;  echo "memShow"
  sleep 8;  echo "rtpShow"
  sleep 8;  echo "rtpMemShow"
  sleep 8;  echo "edrShow"
  sleep 25; echo "rtpShow"
  sleep 10; echo "memShow"
  sleep 20
} > "$R/con.in"

sleep 20
"$R/stop-e10.sh"
kill $BOOTPID 2>/dev/null
cp "$CONSOLE" "$R/logs/nostart-$LABEL.console.log"
echo "nostart $LABEL done: console=$(wc -c < "$R/logs/nostart-$LABEL.console.log") bytes"
