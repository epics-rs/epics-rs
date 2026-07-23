#!/bin/bash
# Boot one rtems-ca-ioc flip-comparison image. Own-pid discipline only; no
# pkill. Unique host port 5074 and MAC 52:54:00:12:34:7$IDX so it collides with
# no other panel (boot-ca.sh owns 5064; cmeasure 5164; topoB 4441/8011).
#   $1 = image   $2 = log   $3 = seconds   $4 = idx (0/1) for port+mac
set -u
. "$HOME/rtems-bringup/rigpid.sh"
IDX=${4:-0}
HOSTP=$((5074 + IDX))
UDPP=$((15074 + IDX))
rig_pidfile "$HOME/rtems-bringup/caflip$IDX.qemu.pid"
rig_kill_own
cd "$HOME/rtems-bringup"
rm -f "$2"
timeout --foreground -k 5 "$3" \
  qemu-system-arm -M xilinx-zynq-a9 -m 256M -no-reboot -nographic \
    -serial null -serial mon:stdio \
    -nic user,model=cadence_gem,mac=52:54:00:12:34:7$IDX,hostfwd=tcp:127.0.0.1:$HOSTP-:5064,hostfwd=udp:127.0.0.1:$UDPP-:5064 \
    -kernel "$1" > "$2" 2>&1 < /dev/null
echo "qemu exit=$? (host tcp $HOSTP)"
