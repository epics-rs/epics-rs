#!/bin/bash
# Boot keydtor-rtems.exe under qemu. Derived from ~/rtems-bringup/boot-ca.sh,
# with three deliberate changes:
#   * NO pkill of any kind. boot-ca.sh opens with a blanket pkill of every
#     qemu-system-arm on the box; other panels have live guests here.
#   * NO networking at all (-net none). The image has no libbsd, needs no
#     address, and must not take a host port.
#   * qemu writes its own pid to ./qemu.pid, and the only process this script
#     ever signals is that pid.
#   $1 = log file   $2 = seconds
set -u
cd "$HOME/rtems-bringup/keydtor" || exit 1
LOG=${1:-keydtor-rtems.log}
SECS=${2:-90}
PIDF=$PWD/qemu.pid
rm -f "$LOG" "$PIDF"

timeout --foreground -k 5 "$SECS" \
  qemu-system-arm -M xilinx-zynq-a9 -m 256M -no-reboot -nographic \
    -serial null -serial mon:stdio \
    -net none \
    -pidfile "$PIDF" \
    -kernel keydtor-rtems.exe > "$LOG" 2>&1 < /dev/null
rc=$?
echo "qemu exit=$rc"

# Belt and braces, scoped to exactly one pid: the one qemu wrote itself.
if [ -f "$PIDF" ]; then
  QPID=$(cat "$PIDF")
  if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
    echo "still alive, killing own qemu pid $QPID"
    kill -TERM "$QPID"
  fi
  rm -f "$PIDF"
fi
