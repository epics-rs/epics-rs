#!/bin/bash
# build-rtems-e10.sh — build realtime-ca-ioc for armv7-rtems-eabihf WITH the
# bring-up probes, so the guest prints `rt top` + `rt stackuse` from inside the
# image every 60 s.
#
# WHY NOT `scripts/embedded-image.sh rtems ca`: that script hardcodes
# FEATURES=client-core, i.e. the clean shipping image with no probe thread, and
# `scripts/**` belongs to another panel this round — so the recipe is
# replicated here rather than edited there.  Everything else (custom TLS spec,
# `--cfg rtems_boot_linked`, the derived libc-std patch, -Zbuild-std) is the
# same recipe, read out of scripts/embedded-image.sh at commit time.
#
# Distinct CARGO_TARGET_DIR and a distinct tree: this must not disturb the
# VxWorks E10/E11 rig sitting in $R/tree and $R/target.
set -e -o pipefail
R=$HOME/vx-rig-e10
T=$R/rtems-tree

export PATH=$HOME/.cargo/bin:$HOME/rtems-bringup/tools/bin:$PATH
export RTEMS_BSP_PREFIX=$HOME/rtems-bringup/tools
export CARGO_HOME=$R/cargo-home
export CARGO_TARGET_DIR=$R/rtems-target
export RUSTUP_TOOLCHAIN=nightly
export RUSTUP_HOME=$HOME/.rustup
unset RUSTC_BOOTSTRAP RUSTFLAGS RUSTC

PROFILE=${EMBEDDED_PROFILE:-release-embedded}
FEATS=${RTEMS_FEATS:-client-core,bringup-probes}
OUT=${1:-caioc-e10}

cd "$T"

# Custom spec carrying has-thread-local:true — the adopted deviation
# (doc/rtems-tls-spec-deviation.md).  The generator lives in this tree.
TARGET="$(./scripts/rtems-tls-spec.sh)"
STEM="$(basename "$TARGET" .json)"
ENVN="CARGO_TARGET_$(printf '%s' "$STEM" | tr '[:lower:]-' '[:upper:]_')_LINKER"
export "$ENVN=arm-rtems6-gcc"
export RUSTFLAGS="--cfg rtems_boot_linked"

mapfile -t CFG < <(./scripts/libc-std-patch.sh nightly)
[ "${#CFG[@]}" -gt 0 ] || { echo "libc-std-patch.sh printed nothing"; exit 1; }
ARGS=()
for l in "${CFG[@]}"; do ARGS+=(--config "$l"); done

cp Cargo.lock "$R/rtems-Cargo.lock.snap"
trap 'cp "$R/rtems-Cargo.lock.snap" "$T/Cargo.lock"' EXIT

cargo +nightly build --profile "$PROFILE" -j4 \
    --no-default-features -Zbuild-std=std,panic_abort -Zjson-target-spec \
    "${ARGS[@]}" \
    -p epics-ca-rs --bin realtime-ca-ioc --features "$FEATS" \
    --target "$TARGET"

IMG=$CARGO_TARGET_DIR/$STEM/$PROFILE/realtime-ca-ioc
[ -f "$IMG" ] || { echo "no image at $IMG"; exit 1; }
cp "$IMG" "$R/$OUT.exe"
stat -c 'IMAGE %n %s bytes' "$R/$OUT.exe"
echo "target=$TARGET features=$FEATS profile=$PROFILE"
