#!/usr/bin/env bash
#
# csrc-check.sh - The gate for the C in this workspace that a cargo command
# never compiles.
#
# WHAT THIS CLOSES
#
# `crates/epics-rtems-boot/csrc/` holds the RTEMS boot shim. Its build script
# compiles it only when RTEMS_BSP_PREFIX names a BSP install, and no gate in
# this repository sets that variable: `scripts/rtems-check.sh` is a
# `cargo check --target armv7-rtems-eabihf`, which type-checks Rust and never
# runs `cc`. So every line of that C was outside CI — a change to it could not
# turn a job red, and its first reader was the serial console of a board that
# would not boot.
#
# WHAT IS AND IS NOT REACHABLE FROM A RUNNER
#
# Compiling the shim FOR THE TARGET needs `arm-rtems6-gcc` and the BSP's own
# headers. No distribution packages that toolchain, RTEMS publishes source only
# — the release tree's `contrib/` holds a README and nothing else — and the
# Source Builder compiles one in hours into a 1.9 GB prefix. Linking needs
# `libbsd.a`, `librtemsbsp.a` and `librtemscpu.a` on top of that. Neither is
# reachable from a runner, and the on-target boot stays the shim's acceptance.
#
# What a runner CAN do is compile the shim FOR THE HOST, and this gate does it
# two ways, because the shim splits two ways:
#
#   `boot_args.c` calls no RTEMS API at all — it is the boot command line's
#   tokeniser, the whole configuration surface of a target image — and is in
#   its own translation unit precisely so this gate can compile it AND RUN it.
#   That half is checked for behaviour, not just compilation.
#
#   `rtems_init.c` is POSIX_Init, so it does call RTEMS; but what it calls is
#   48 declarations, and those are recorded verbatim under
#   `csrc/tests/rtems-api/`. Compiled against that record, on the host, every
#   name it uses is checked for existence, arity and types — including the
#   printf format checking RTEMS_PRINTFLIKE puts on rtems_panic. See that
#   directory's README.md for what this does and does not prove.
#
# `rtems_config.c` and `rtems_stats.c` remain outside: the first ends in
# `<rtems/confdefs.h>`, which generates the configuration table rather than
# declaring an API, and the second reaches into RTEMS score internals whose
# surface is too large and too unstable to record. Only an image build compiles
# those two.
#
# Warnings are errors here. The image build passes `.warnings(true)` and nothing
# reads its output; a warning that only ever appears inside a cross build on one
# machine is a warning nobody sees.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

CSRC=crates/epics-rtems-boot/csrc
CC_HOST=${CC:-cc}

# c99 rather than the compiler default: the shim is built by whatever
# `arm-rtems6-gcc` a site installed, so the source must not rely on one
# compiler's dialect. `-pedantic` is what makes that a check rather than a hope.
CFLAGS=(-std=c99 -pedantic -Wall -Wextra -Werror -I "$CSRC")

# rtems_init.c cannot be held to c99: the routing socket it waits on needs
# `u_char`/`u_short`/`u_long`, which the BSD headers expose only outside strict
# ISO mode. gnu17 is not a relaxation, it is the dialect the image build already
# uses — `cc` passes no `-std` and arm-rtems6-gcc defaults to it.
INIT_CFLAGS=(
    -std=gnu17 -pedantic -Wall -Wextra -Werror
    -Werror=implicit-function-declaration -Werror=implicit-int
    -I "$CSRC/tests/rtems-api" -I "$CSRC"
)

# Every arm of rtems_init.c's own `#if`s, so none of them is compiled for the
# first time on the board. The static-address path is where the routing socket
# and struct if_msghdr live, and the default DHCP build does not reach it.
INIT_CONFIGS=(
    ""
    '-DEPICS_RTEMS_STATIC_IP="10.0.0.2" -DEPICS_RTEMS_STATIC_NETMASK="255.255.255.0"'
    '-DEPICS_RTEMS_STATIC_IP="10.0.0.2" -DEPICS_RTEMS_STATIC_NETMASK="255.255.255.0" -DEPICS_RTEMS_STATIC_GATEWAY="10.0.0.1"'
    "-DEPICS_RTEMS_BSD_LOG_DEBUG=0"
)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The gate is only worth having while the split holds. A tokeniser moved back
# into rtems_init.c, or a second RTEMS-free helper added there, would be outside
# CI again with nothing to say so.
echo "==> checking that the gated sources stayed free of RTEMS headers"
if grep -n -E '^#[[:space:]]*include[[:space:]]*<(rtems|machine)/' \
        "$CSRC/boot_args.c" "$CSRC/boot_args.h" "$CSRC/tests/boot_args_test.c"; then
    echo "error: an RTEMS header reached a source this gate compiles on the host." >&2
    echo "       Either keep the RTEMS API out of it, or move that code to" >&2
    echo "       rtems_init.c and accept that only an image build compiles it." >&2
    exit 1
fi

echo "==> compiling the target-independent shim sources with $CC_HOST"
"$CC_HOST" "${CFLAGS[@]}" -c "$CSRC/boot_args.c" -o "$work/boot_args.o"

echo "==> building and running the boot-argument boundary set"
"$CC_HOST" "${CFLAGS[@]}" -o "$work/boot_args_test" \
    "$CSRC/tests/boot_args_test.c" "$work/boot_args.o"
"$work/boot_args_test"

./scripts/rtems-api-check.sh

echo "==> compiling rtems_init.c against the recorded RTEMS API with $CC_HOST"
for config in "${INIT_CONFIGS[@]}"; do
    echo "    [${config:-default}]"
    # Word splitting is the point: each entry is a -D list.
    # shellcheck disable=SC2086
    "$CC_HOST" "${INIT_CFLAGS[@]}" $config \
        -c "$CSRC/rtems_init.c" -o "$work/rtems_init.o"
done

echo "==> csrc-check: boot-argument path compiled and exercised on the host,"
echo "    rtems_init.c compiled against the recorded RTEMS API in every"
echo "    configuration it selects."
echo "    NOT covered here: rtems_config.c (generates its table from"
echo "    <rtems/confdefs.h>) and rtems_stats.c (RTEMS score internals) —" \
     "only an image build compiles them."
