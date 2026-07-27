#!/bin/bash
# E8 rig: build realtime-ca-ioc.vxe for x86_64-wrs-vxworks, optionally with the
# mutex/semaphore link-time interposition.
#
# Recorded as a script because the `--wrap` form previously existed only in a
# shell history, and that history is empty on this box -- an unreproducible
# measurement build is not a measurement.  Every flag below is load-bearing:
#
#   --no-default-features    the default set pulls host-only deps that do not
#                            build for VxWorks.
#   --profile release-embedded  the target's profile; a plain --release links
#                            differently.
#   -Zbuild-std=std,panic_abort  VxWorks has no prebuilt std.
#   --config patch...libc-std  a PATH patch onto a local libc checkout, NOT a
#                            git patch: `--config patch...libc-std.git=` gives
#                            HTTP 404.  The `.package="libc"` line renames it.
#   --features client-core,bringup-probes  bringup-probes is what compiles the
#                            E8 record set and the MTXPROBE interposers in.
#
# Usage:  ./build-e8.sh [plain|wrap]
#
# With `wrap`, the two __wrap_ symbols in realtime-ca-ioc.rs's
# `mutex_alloc_probe` get their linker flags through `cargo rustc`, so only the
# final crate relinks -- passing them via RUSTFLAGS would rebuild all of std.
# Without them the __wrap_* symbols are simply unreferenced and the image
# behaves exactly like the plain build.  The .vxe is stripped, so `nm` cannot
# confirm the interposition; the confirmation is behavioural, the MTXPROBE
# lines on the console.
set -eu
WT=$HOME/vx-rig-e8/wt
LIBC_PATCH=$HOME/vx-bringup/target25/libc-std-patch/31d5776f9952aa349813d7fbef3addae1bf0a5ef-0.2.185
export CARGO_TARGET_DIR=$HOME/vx-bringup/target25
MODE=${1:-plain}

# shellcheck disable=SC1091
. "$HOME/vx-bringup/vxenv25.sh"

cd "$WT"

FLAGS=(
    --profile release-embedded -j4
    --no-default-features
    -Zbuild-std=std,panic_abort
    --target x86_64-wrs-vxworks
    --config "patch.crates-io.libc-std.package=\"libc\""
    --config "patch.crates-io.libc-std.path=\"$LIBC_PATCH\""
    -p epics-ca-rs --bin realtime-ca-ioc
    --features client-core,bringup-probes
)

case "$MODE" in
    wrap)
        cargo +nightly rustc "${FLAGS[@]}" -- \
            -C link-arg=-Wl,--wrap=semMCreate \
            -C link-arg=-Wl,--wrap=pthread_mutex_lock
        ;;
    plain)
        cargo +nightly build "${FLAGS[@]}"
        ;;
    *)
        echo "usage: $0 [plain|wrap]" >&2
        exit 2
        ;;
esac

# The target spec appends `.vxe` itself, so the artefact is already named
# realtime-ca-ioc.vxe -- there is no extension-less binary to copy.
OUT=$CARGO_TARGET_DIR/x86_64-wrs-vxworks/release-embedded/realtime-ca-ioc.vxe
cp "$OUT" "$HOME/vx-rig-e8/ftp/root/realtime-ca-ioc.vxe"
ls -l "$HOME/vx-rig-e8/ftp/root/realtime-ca-ioc.vxe"
echo "mode=$MODE built from $(git -C "$WT" rev-parse --short HEAD) plus working-tree probe edits"
