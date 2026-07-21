#!/usr/bin/env bash
#
# rtems-check.sh - The RTEMS portability gate for the epics-rs workspace.
#
# This is the gate that answers "does the RTEMS closure still compile for the
# target", on a machine with no RTEMS toolchain and no BSP. `cargo check` does
# not link, so it needs neither.
#
# WHY THIS FILE EXISTS
#
# The invocation used to live only in prose (doc/rtems-runtime-portability-
# design.md §8.1). Two things followed from that, and the second is why this
# script is here rather than another paragraph:
#
#   * The written form was `-p <crate> --lib`, and `--lib` never compiles
#     `src/bin/*.rs`. The one binary anyone boots on the target — rtems-ca-ioc
#     — was outside the gate for the whole branch, and a build break
#     (E0433, a missing `StackSizeClass` import from b594b18a) survived every
#     "RTEMS check green" report until the bring-up box tried to boot it.
#   * A gate that lives in a paragraph drifts. This one is executable, so the
#     scope is what runs rather than what someone remembered to type.
#
# WHY NOT --all-targets OR --bins
#
# Measured, not assumed: `cargo +nightly check -p epics-ca-rs --bins ... \
# --target armv7-rtems-eabihf` fails, and not in the linker — the host CLI
# tools (caget-rs, caput-rs, camonitor-rs, softioc-rs, ca-admin-rs, ca-soak)
# do not compile for RTEMS at all (E0432/E0433/E0308) and were never meant to.
# So the narrowest flag set that covers the target is `--lib` for each crate
# plus `--bin` for each binary that is actually built for RTEMS.
#
# Usage:
#   ./scripts/rtems-check.sh          # check every crate and target binary
#   ./scripts/rtems-check.sh --quiet  # only report failures

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

TARGET="armv7-rtems-eabihf"
# `-Zbuild-std` is required: there is no prebuilt std for this triple.
COMMON=(+nightly check --no-default-features -Zbuild-std=std,panic_abort --target "$TARGET")

# The crates that must compile for the target.
CRATES=(epics-base-rs epics-ca-rs epics-pva-rs epics-rtems-boot)

# Binaries built for the target, as `crate:bin` pairs. The drift check below
# fails if a `src/bin/*.rs` compiled for RTEMS is missing from this list.
BINS=(epics-ca-rs:rtems-ca-ioc)

QUIET=0
[[ "${1:-}" == "--quiet" ]] && QUIET=1

log() { [[ $QUIET -eq 1 ]] || echo "$@"; }

if ! cargo +nightly --version >/dev/null 2>&1; then
    echo "error: the nightly toolchain is required for -Zbuild-std." >&2
    exit 1
fi

# Drift check: every binary whose source is compiled for RTEMS must be gated.
missing=()
while IFS= read -r src; do
    crate="$(echo "$src" | cut -d/ -f2)"
    bin="$(basename "$src" .rs)"
    printf '%s\n' "${BINS[@]}" | grep -qx "$crate:$bin" || missing+=("$crate:$bin")
done < <(grep -rl 'target_os = "rtems"' crates/*/src/bin/ 2>/dev/null || true)

if [[ ${#missing[@]} -gt 0 ]]; then
    echo "error: these binaries are built for RTEMS but are not in this gate:" >&2
    printf '  %s\n' "${missing[@]}" >&2
    echo "Add them to BINS in $0 — an ungated target binary is how the last" >&2
    echo "build break reached the bring-up box." >&2
    exit 1
fi

failed=()

for crate in "${CRATES[@]}"; do
    log "== $crate --lib"
    if ! cargo "${COMMON[@]}" -p "$crate" --lib; then
        failed+=("$crate --lib")
    fi
done

for pair in "${BINS[@]}"; do
    crate="${pair%%:*}"
    bin="${pair##*:}"
    log "== $crate --bin $bin"
    if ! cargo "${COMMON[@]}" -p "$crate" --bin "$bin"; then
        failed+=("$crate --bin $bin")
    fi
done

if [[ ${#failed[@]} -gt 0 ]]; then
    echo "RTEMS gate FAILED:" >&2
    printf '  %s\n' "${failed[@]}" >&2
    exit 1
fi

log "RTEMS gate: every crate and target binary compiles for $TARGET."
