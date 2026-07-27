#!/bin/bash
# boot-rtems-e10.sh <image-basename> — boot ONE armv7 RTEMS guest for the
# CAS-client/CAS-event stack high-water measurement.
#
# RIG DISCIPLINE.  gv100 is SHARED and already runs two other panels' RTEMS
# guests (qemu-system-arm pids 3308690 and 3309544 at the time of writing).
# This records its own pid in $R/rtems-qemu.pid and kills only that pid, after
# re-reading /proc/<pid>/comm.  No pkill/killall of any kind is run, and no
# name sweep: a pid this rig did not start is never signalled.
#
# Resource block, disjoint from both the other guests (5064, 5075) and from
# this panel's own VxWorks rig (21534/25064/25075):
#   hostfwd tcp 127.0.0.1:25164 -> guest 5064   (CA TCP)
#   hostfwd udp 127.0.0.1:25165 -> guest 5064   (CA UDP name search)
#   MAC 52:54:00:12:39:10
set -e -o pipefail
R=$HOME/vx-rig-e10
IMG=${1:-caioc-e10}
KERNEL=$R/$IMG.exe
LOG=$R/logs/rtems-$IMG.log
PIDF=$R/rtems-qemu.pid

[ -f "$KERNEL" ] || { echo "no image $KERNEL"; exit 1; }
"$R/stop-rtems-e10.sh" || true
mkdir -p "$R/logs"
: > "$LOG"

setsid qemu-system-arm -M xilinx-zynq-a9 -m 256M -no-reboot -nographic \
  -serial null -serial mon:stdio \
  -nic "user,model=cadence_gem,mac=52:54:00:12:39:10,hostfwd=tcp:127.0.0.1:25164-:5064,hostfwd=udp:127.0.0.1:25165-:5064" \
  -kernel "$KERNEL" > "$LOG" 2>&1 < /dev/null &
QPID=$!
echo "$QPID" > "$PIDF"
disown
echo "rtems qemu pid=$QPID image=$IMG log=$LOG"
