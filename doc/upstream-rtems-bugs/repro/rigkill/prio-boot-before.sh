#!/bin/bash
# BEFORE image: pre-CAS-TCP/CAS-UDP-banding rtems-ca-ioc, no instrumentation.
#
# KILL SAFETY: the box is SHARED.  Record the pid we start in prio/prio.qemu.pid
# so stop-mine.sh can kill exactly this guest (rigpid.sh).  Never pkill.
cd $HOME/rtems-bringup/prio
rm -f before.log
. "$HOME/rtems-bringup/rigpid.sh"
rig_pidfile "$HOME/rtems-bringup/prio/prio.qemu.pid"
setsid qemu-system-arm -M xilinx-zynq-a9 -m 256M -no-reboot -nographic \
  -serial null -serial mon:stdio \
  -nic user,model=cadence_gem,hostfwd=tcp:127.0.0.1:5064-:5064,hostfwd=udp:127.0.0.1:15076-:5064 \
  -kernel $HOME/rtems-bringup/caioc.exe > before.log 2>&1 < /dev/null &
QPID=$!
rig_track $QPID
echo "BOOTED caioc.exe -> before.log pid=$QPID (recorded in prio/prio.qemu.pid)"
exit 0
