#!/usr/bin/env bash
#
# rtems-tool-target.sh - print the toolchain target of the BSP prefix
# (`arm-rtems6` or `arm-rtems7`), the same rule `epics_rtems_boot::contract::
# tool_target_in` applies in the build scripts: the one series whose
# `<prefix>/<target>/<bsp>/lib` the prefix holds.
#
# The build scripts need it before cargo runs, to name the linker
# (`<target>-gcc`) for the target spec's stem; the crate needs it inside the
# build to name the compiler and the library directory. Two readers, one rule,
# read off the tree rather than configured — see TOOL_TARGETS in contract.rs.
#
# With RTEMS_BSP_PREFIX unset (the check-only configuration, which never
# links) it prints `arm-rtems6`: some name is required for the linker
# variable, and it is never invoked.

set -euo pipefail

PREFIX="${RTEMS_BSP_PREFIX:-}"
BSP="${RTEMS_BSP:-xilinx_zynq_a9_qemu}"

if [[ -z "$PREFIX" ]]; then
    echo arm-rtems6
    exit 0
fi

present=()
for target in arm-rtems6 arm-rtems7; do
    [[ -d "$PREFIX/$target/$BSP/lib" ]] && present+=("$target")
done
case "${#present[@]}" in
    1) echo "${present[0]}" ;;
    0) echo "rtems-tool-target: RTEMS_BSP_PREFIX=$PREFIX holds no arm-rtems6/$BSP/lib or arm-rtems7/$BSP/lib; build it with scripts/rtems-bsp.sh" >&2; exit 1 ;;
    *) echo "rtems-tool-target: RTEMS_BSP_PREFIX=$PREFIX holds $BSP/lib for ${present[*]}; keep one series per prefix" >&2; exit 1 ;;
esac
