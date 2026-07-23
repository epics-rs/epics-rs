#!/bin/bash
# Console-only measurement boot. Own-pid discipline; no pkill; no hostfwd
# (guest dials its compiled-in refused NS at 10.0.2.2:15076 via SLIRP RST).
#   $1 = image   $2 = log   $3 = seconds   $4 = idx (unique mac/pidfile)
set -u
. "$HOME/rtems-bringup/rigpid.sh"
IDX=${4:-0}
rig_pidfile "$HOME/rtems-bringup/measure$IDX.qemu.pid"
rig_kill_own
cd "$HOME/rtems-bringup"
rm -f "$2"
timeout --foreground -k 5 "$3" \
  qemu-system-arm -M xilinx-zynq-a9 -m 256M -no-reboot -nographic \
    -serial null -serial mon:stdio \
    -nic user,model=cadence_gem,mac=52:54:00:12:34:8$IDX \
    -kernel "$1" > "$2" 2>&1 < /dev/null
echo "qemu exit=$?"
