#!/bin/bash
# Boot one tlsdtor image under qemu. Copied from ~/rtems-bringup/boot-ca.sh with
# two deliberate changes:
#   * NO hostfwd. This image needs no networking, and a forwarded port would
#     collide with the other panels' live guests.
#   * NO pkill. boot-ca.sh starts with a blanket pkill of other people's qemus;
#     this script kills only the qemu it started, through the timeout it runs
#     under.
#   $1 = image   $2 = log   $3 = seconds
set -u
cd "$HOME/rtems-bringup/tlsdtor2" || exit 1
rm -f "$2"
timeout --foreground -k 5 "$3" \
  qemu-system-arm -M xilinx-zynq-a9 -m 256M -no-reboot -nographic \
    -serial null -serial mon:stdio \
    -nic user,model=cadence_gem \
    -kernel "$1" > "$2" 2>&1 < /dev/null
echo "qemu exit=$?"
