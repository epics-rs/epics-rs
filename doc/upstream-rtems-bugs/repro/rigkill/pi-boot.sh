#!/bin/bash
# Boot one of the pi/ images. $1 = image basename, $2 = log basename.
# Ports: TCP 5064 and UDP 15076 -> guest 5064 (this panel's allocation).
#
# KILL SAFETY: the box is SHARED -- several panels run their own qemu guests
# at the same time.  This rig records every pid it starts in pi/pi.qemu.pid,
# and stop.sh kills only those (rigpid.sh re-checks /proc/PID/comm first, so a
# recycled pid cannot be mistaken for our guest).  Never pkill here:
# `pkill -x qemu-system-arm` takes down every other panel's guest, and
# `pkill -f <pattern>` additionally matches the ssh command line that is
# running the script.
cd $HOME/rtems-bringup/pi
IMG=${1:?image}
LOG=${2:-$IMG.log}
rm -f $LOG
. "$HOME/rtems-bringup/rigpid.sh"
rig_pidfile "$HOME/rtems-bringup/pi/pi.qemu.pid"
setsid qemu-system-arm -M xilinx-zynq-a9 -m 256M -no-reboot -nographic \
  -serial null -serial mon:stdio \
  -nic user,model=cadence_gem,hostfwd=tcp:127.0.0.1:5064-:5064,hostfwd=udp:127.0.0.1:15076-:5064 \
  -kernel $HOME/rtems-bringup/pi/$IMG > $LOG 2>&1 < /dev/null &
QPID=$!
rig_track $QPID
echo "BOOTED $IMG -> $LOG pid=$QPID (recorded in pi/pi.qemu.pid)"
exit 0
