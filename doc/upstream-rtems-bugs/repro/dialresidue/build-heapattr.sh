#!/bin/bash
# Build the heap-attribution measurement image.
#   $1 = output name under ~/rtems-bringup   $2 = C6_NAME_SERVERS value
#
# Same wiring as build-measure.sh (custom spec carrying has-thread-local:true,
# i.e. the FLIP image), plus the --wrap allocator interposition that makes the
# per-call-site accounting in csrc/heapattr.c the one every allocation lands in.
set -e -o pipefail
export PATH=$HOME/.cargo/bin:$HOME/rtems-bringup/tools/bin:$PATH
export RTEMS_BSP_PREFIX=$HOME/rtems-bringup/tools
cd $HOME/epics-rs
cargo update -p libc --precise 0.2.188 2>&1 | tail -1
export C6_NAME_SERVERS="$2"
TGT="$($HOME/rtems-bringup/rtems-tls-spec.sh)"
OUTDIR="$(basename "$TGT" .json)"
export CARGO_TARGET_ARMV7_RTEMS_EABIHF_TLS_LINKER=arm-rtems6-gcc

# calloc/realloc are deliberately NOT wrapped: RTEMS builds both on
# malloc()/free() from other translation units, so wrapping them too
# records every calloc twice under one pointer.
WRAPS="malloc free posix_memalign aligned_alloc"
LINKARGS=""
for w in $WRAPS; do LINKARGS="$LINKARGS -Clink-arg=-Wl,--wrap=$w"; done
export RUSTFLAGS="$LINKARGS"

cargo +nightly build --release --target "$TGT" \
    -Zbuild-std=std,panic_abort -Zjson-target-spec \
    --no-default-features --features client-core,bringup-probes \
    -p epics-ca-rs --bin rtems-ca-ioc 2>&1 | tail -25
cp target/$OUTDIR/release/rtems-ca-ioc $HOME/rtems-bringup/$1
echo "staged $1 with C6_NAME_SERVERS=$2 (target=$TGT)"
arm-rtems6-nm $HOME/rtems-bringup/$1 | grep -E "__wrap_(malloc|free)|epics_heapattr_report"
