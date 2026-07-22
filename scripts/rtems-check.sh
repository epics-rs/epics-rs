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

# Binaries built for the target, as `crate:bin` pairs.
BINS=(
    epics-ca-rs:rtems-ca-ioc
)

# Binaries in the crates above that are deliberately NOT built for the target.
#
# This list exists so the check below can be a CENSUS rather than a search.
# The previous version grepped `src/bin` for the literal `target_os = "rtems"`
# and demanded a match be in BINS — which only ever sees a binary that says
# "rtems" in its own source text. A binary gated in `Cargo.toml`
# (`required-features`), gated by a feature predicate, or gated by nothing at
# all was invisible to it and would land outside the gate exactly as
# `rtems-ca-ioc` did. Requiring every binary to be classified one way or the
# other removes the way to be absent: a new `src/bin/*.rs` fails this script
# until someone states which side it is on.
#
# Every entry here is a host CLI tool. Measured, not assumed: they do not
# compile for `armv7-rtems-eabihf` at all (E0432/E0433/E0308) and were never
# meant to — see WHY NOT --all-targets above.
HOST_ONLY=(
    epics-ca-rs:ca-admin-rs
    epics-ca-rs:ca-lint-rs
    epics-ca-rs:ca-repeater-rs
    epics-ca-rs:ca-replay-rs
    epics-ca-rs:ca-soak
    epics-ca-rs:ca-soak-observed
    epics-ca-rs:caget-rs
    epics-ca-rs:cainfo-rs
    epics-ca-rs:camonitor-rs
    epics-ca-rs:caput-rs
    epics-ca-rs:softioc-rs
    epics-pva-rs:mshim-rs
    epics-pva-rs:pvcall-rs
    epics-pva-rs:pvget-rs
    epics-pva-rs:pvinfo-rs
    epics-pva-rs:pvlist-rs
    epics-pva-rs:pvmonitor-rs
    epics-pva-rs:pvput-rs
    epics-pva-rs:pvxvct-rs
)

QUIET=0
[[ "${1:-}" == "--quiet" ]] && QUIET=1

log() { [[ $QUIET -eq 1 ]] || echo "$@"; }

if ! cargo +nightly --version >/dev/null 2>&1; then
    echo "error: the nightly toolchain is required for -Zbuild-std." >&2
    exit 1
fi

# Census: every binary in the RTEMS crates is in BINS or in HOST_ONLY.
CLASSIFIED=("${BINS[@]}" "${HOST_ONLY[@]}")
present=()
unclassified=()
for crate in "${CRATES[@]}"; do
    for src in "crates/$crate/src/bin"/*.rs; do
        [[ -e "$src" ]] || continue
        pair="$crate:$(basename "$src" .rs)"
        present+=("$pair")
        printf '%s\n' "${CLASSIFIED[@]}" | grep -qx "$pair" || unclassified+=("$pair")
    done
done

if [[ ${#unclassified[@]} -gt 0 ]]; then
    echo "error: these binaries are in an RTEMS crate but are classified neither" >&2
    echo "       as target binaries nor as host-only:" >&2
    printf '  %s\n' "${unclassified[@]}" >&2
    echo "Add each to BINS or to HOST_ONLY in $0. Being in neither is how a" >&2
    echo "target binary lands outside this gate — which is how the last build" >&2
    echo "break reached the bring-up box." >&2
    exit 1
fi

# The other direction: a listed binary whose source is gone. Left unchecked, a
# rename would silently stop being covered while both lists still looked full.
stale=()
for pair in "${CLASSIFIED[@]}"; do
    printf '%s\n' "${present[@]}" | grep -qx "$pair" || stale+=("$pair")
done

if [[ ${#stale[@]} -gt 0 ]]; then
    echo "error: these binaries are listed in $0 but have no source file:" >&2
    printf '  %s\n' "${stale[@]}" >&2
    echo "Remove or rename the entry — a stale one covers nothing." >&2
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
