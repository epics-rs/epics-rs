#!/bin/bash
# run-arm-e11.sh <guest-ram> <label> [pace-seconds] [ceiling] — one E11 arm.
#
# Boots the ca-e11 image at <guest-ram>, waits for the IOC's second probe pass
# (so the shim has spoken at least once), drives rampprobe-e11.py against it,
# then stops the guest and files the console under the arm label.  Guest RAM is
# the ONLY thing that varies between arms; the image, the shim object and the
# driver are byte-identical across them.
#
# Kills only the pids this rig recorded, via stop-e10.sh.  No pkill/killall.
set -o pipefail
R=$HOME/vx-rig-e10
RAM=$1
LABEL=$2
PACE=${3:-1.0}
CEIL=${4:-200}
[ -n "$RAM" ] && [ -n "$LABEL" ] || { echo "usage: $0 <ram> <label> [pace] [ceiling]"; exit 1; }

# The image is a parameter so the shim-free control build can be driven by the
# same script: everything except the -Wl,--wrap link and the report call is the
# same source, so any difference in the wall belongs to the instrumentation.
IMG=${VXIMG:-ca-e11}
CONSOLE=$R/logs/console-$IMG.log
# Generous: above ~40 held connections the server's accept latency runs to
# several seconds per connection, so the ramp takes far longer than PACE*CEIL.
# The boot script must outlive the probe AND the 150 s post-mortem, or it tears
# the console down underneath them.
BOOTSECS=$(python3 -c "print(int(180 + 6.0*$CEIL + 300))")

"$R/stop-e10.sh" > /dev/null 2>&1
# The previous arm's console still holds a "C6 seq=2" line and boot-e10.sh only
# truncates it a moment after launch, so the readiness grep below would match
# the OLD run and drive the probe against a guest that has not booted.  Remove
# it here and make readiness require the file to exist again.
rm -f "$CONSOLE"
VXRAM=$RAM nohup "$R/boot-e10.sh" "$IMG" "$BOOTSECS" > "$R/logs/arm-$LABEL.boot.log" 2>&1 &
BOOTPID=$!
echo "arm $LABEL image=$IMG ram=$RAM pace=$PACE ceiling=$CEIL bootpid=$BOOTPID bootsecs=$BOOTSECS"

READY=no
for _ in $(seq 240); do
    if grep -q "C6 seq=2 " "$CONSOLE" 2>/dev/null; then READY=yes; break; fi
    sleep 1
done
echo "ioc-ready=$READY after $(grep -c . "$CONSOLE" 2>/dev/null) console lines"
if [ "$READY" != yes ]; then
    # An arm that never reaches the IOC is a result, not a bookkeeping error:
    # at low guest RAM the RTP can die during startup, and that console is the
    # evidence.  File it before tearing down, or the next arm overwrites it.
    "$R/stop-e10.sh"
    kill $BOOTPID 2>/dev/null
    cp "$CONSOLE" "$R/logs/arm-$LABEL.console.log" 2>/dev/null
    echo "arm $LABEL NOT-READY: console=$(wc -c < "$R/logs/arm-$LABEL.console.log") bytes"
    exit 2
fi

# The probe's own deadline has to track the ceiling for the same reason
# BOOTSECS does; a fixed 600 s stopped the ramp before the wall on any arm that
# could hold more than ~130 connections.
MAXSEC=$(python3 -c "print(int(6.0*$CEIL + 300))")
python3 "$R/rampprobe-e11.py" "$CEIL" "$LABEL" "$MAXSEC" "$PACE" 2>&1 \
    | tee "$R/logs/arm-$LABEL.probe.log"

sleep 15                       # let the console flush whatever the abort prints

# Post-mortem.  Above ~40 clients the IOC's own probe thread is starved by the
# CAS-TCP band and the console stops advancing, so an RTP that dies during the
# ramp leaves NOTHING on the console.  The kernel shell is a separate task and
# survives, and ED&R keeps the fatal record, so ask the kernel afterwards
# instead of trusting the console.  `rtpShow` says whether the RTP is still
# there at all; `edrShow` prints what killed it.
# The first attempt at this used a 19 s window and got nothing back at all, not
# even the shell's echo of the command, so the wait is 150 s: if the console is
# merely starved rather than dead it has to be given time to come back once the
# ramp's connections are released.
if [ -p "$R/con.in" ]; then
    { echo ""; sleep 30; echo "rtpShow"; sleep 40; echo "edrShow"; sleep 80; } > "$R/con.in"
fi

"$R/stop-e10.sh"
kill $BOOTPID 2>/dev/null
cp "$CONSOLE" "$R/logs/arm-$LABEL.console.log"
echo "arm $LABEL done: console=$(wc -c < "$R/logs/arm-$LABEL.console.log") bytes"
