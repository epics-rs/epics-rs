#!/bin/bash
# Build both keydtor images from the one source.
#   * keydtor-rtems.exe : arm-rtems6 kernel, xilinx_zynq_a9_qemu BSP
#   * keydtor-linux     : native glibc control
# Link flags for the RTEMS side are the epics-rs link contract
# (crates/epics-rtems-boot/src/contract.rs): the multilib selectors, -B<bsp>/lib
# for the linker script, -qrtems, --gc-sections, -u POSIX_Init.
# No libbsd here: this repro needs no network, so the image is kernel + libc only.
set -e
cd "$HOME/rtems-bringup/keydtor"

PREFIX=$HOME/rtems-bringup/tools
BSP=xilinx_zynq_a9_qemu
BSPLIB=$PREFIX/arm-rtems6/$BSP/lib
CC=$PREFIX/bin/arm-rtems6-gcc

ABI="-march=armv7-a -mthumb -mfpu=neon -mfloat-abi=hard -mtune=cortex-a9"

echo "=== rtems ==="
$CC $ABI \
  -I"$BSPLIB/include" \
  -O2 -g -Wall -Wextra \
  -DKEYDTOR_PLATFORM='"rtems6-armv7-xilinx_zynq_a9_qemu"' \
  -B"$BSPLIB" -qrtems -Wl,--gc-sections -u POSIX_Init \
  keydtor.c -o keydtor-rtems.exe
ls -l keydtor-rtems.exe

echo "=== linux ==="
gcc -O2 -g -Wall -Wextra -pthread \
  -DKEYDTOR_PLATFORM='"linux-glibc"' \
  keydtor.c -o keydtor-linux
ls -l keydtor-linux
gcc --version | head -1
ldd --version | head -1
