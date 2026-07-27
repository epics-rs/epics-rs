#!/bin/bash
# run-arm-e12.sh <guest-ram> <label> <image> [pace] [ceiling] — one split-timing
# arm.  Same shape as run-arm-e11.sh but the image is an explicit argument,
# because the whole point of this arm is to run the SHIM-FREE control build
# (`ca-ctl`) past held=82, which the E11 control arms never reached: at 1280M
# the control walls at 81, one connection short of the knee, so the E11 data
# cannot say whether the 7.2 s plateau belongs to the server or to the rig's
# 1 Hz full-heap-table console dump.
set -o pipefail
R=$HOME/vx-rig-e10
RAM=$1
LABEL=$2
IMG=$3
PACE=${4:-1.0}
CEIL=${5:-200}
[ -n "$RAM" ] && [ -n "$LABEL" ] && [ -n "$IMG" ] || { echo "usage: $0 <ram> <label> <image> [pace] [ceiling]"; exit 1; }

CONSOLE=$R/logs/console-$IMG.log
BOOTSECS=$(python3 -c "print(int(180 + 9.0*$CEIL + 300))")

"$R/stop-e10.sh" > /dev/null 2>&1
rm -f "$CONSOLE"
VXRAM=$RAM nohup "$R/boot-e10.sh" "$IMG" "$BOOTSECS" > "$R/logs/arm-$LABEL.boot.log" 2>&1 &
BOOTPID=$!
echo "arm $LABEL image=$IMG ram=$RAM pace=$PACE ceiling=$CEIL bootsecs=$BOOTSECS"

READY=no
for _ in $(seq 240); do
    if grep -qE "serving [0-9]+ records on CA port" "$CONSOLE" 2>/dev/null; then READY=yes; break; fi
    sleep 1
done
echo "ioc-ready=$READY after $(grep -c . "$CONSOLE" 2>/dev/null) console lines"
if [ "$READY" != yes ]; then
    "$R/stop-e10.sh"; kill $BOOTPID 2>/dev/null
    cp "$CONSOLE" "$R/logs/arm-$LABEL.console.log" 2>/dev/null
    echo "arm $LABEL NOT-READY"; exit 2
fi

MAXSEC=$(python3 -c "print(int(9.0*$CEIL + 300))")
python3 "$R/rampprobe-e12.py" "$CEIL" "$LABEL" "$MAXSEC" "$PACE" 2>&1 \
    | tee "$R/logs/arm-$LABEL.probe.log"

sleep 15
"$R/stop-e10.sh"
kill $BOOTPID 2>/dev/null
cp "$CONSOLE" "$R/logs/arm-$LABEL.console.log"
echo "arm $LABEL done: console=$(wc -c < "$R/logs/arm-$LABEL.console.log") bytes"
