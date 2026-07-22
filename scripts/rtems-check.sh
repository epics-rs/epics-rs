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
# The same defect then repeated on the other axis. `--lib`/`--bin` says WHAT is
# compiled; it says nothing about WHICH CONFIGURATION, and the gate only ever
# compiled one of the two. Everything an image gets from
# `#[cfg(rtems_boot_linked)]` was outside it, so the gate stayed green while a
# real image build hard-failed with `error[E0080]: evaluation panicked: RTEMS
# libc layout bug`. Both axes are now a census: see CONFIGS below.
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
#
# `--locked` is load-bearing, not hygiene. This gate's whole claim is "the
# dependency set in this tree compiles for RTEMS", and without `--locked`
# cargo is free to re-resolve mid-run and answer about a *different* set,
# leaving the new resolution behind as a working-tree `M Cargo.lock` that the
# operator never asked for. That is exactly how an image build and this gate
# come to disagree: an image needs a patched `libc`
# (`[patch.crates-io]`, see the refusal messages in
# `crates/epics-rtems-boot/src/lib.rs`), adding one rewrites the lock, and a
# gate that silently accepts the rewrite reports on a resolution nobody
# committed. With `--locked` that divergence is a named error instead.
#
# Not done, and why: pinning `libc` in the committed lock was considered and
# is measured to buy nothing — under `--precise 0.2.188` BOTH layout refusals
# still fire, byte for byte, as they do at 0.2.186. No published `libc`
# satisfies the predicates; the one that does is the unmerged fork, and which
# fork lands is not this script's decision.
COMMON=(+nightly check --locked --no-default-features -Zbuild-std=std,panic_abort --target "$TARGET")

# The crates that must compile for the target.
CRATES=(epics-base-rs epics-ca-rs epics-pva-rs epics-rtems-boot epics-bridge-rs)

# Per-crate feature selection, on top of COMMON's `--no-default-features`.
#
# For the first four crates the empty selection IS the target's configuration,
# so they are absent here. `epics-bridge-rs` is the first crate for which that
# is false, and silently, in the direction that reports green: with no features
# at all it compiles `error`, `convert` and `lib.rs` and nothing else — the
# `qsrv` module is behind `#[cfg(feature = "qsrv-core")]`, so a featureless
# build type-checks 0 of the 11,281 production lines this gate exists to cover.
#
# So the selection is stated rather than defaulted, and stated per crate rather
# than globally: it says out loud that this crate's target configuration is a
# CHOICE. `qsrv-core` and not `qsrv` because `qsrv` implies `pvalink`, which
# needs `epics_pva_rs::client_native` — 47 errors on this target
# (doc/qsrv-rtems-design.md §0 probe D). See that document §2.2 for why the
# split is a named feature rather than an absence.
declare -A CRATE_FEATURES=(
    [epics-bridge-rs]="qsrv-core"
)

# Binaries built for the target, as `crate:bin` pairs.
BINS=(
    epics-ca-rs:rtems-ca-ioc
    # `rtems-pva-ioc` is an `epics-bridge-rs` binary, not an `epics-pva-rs` one.
    # It moved when it grew a QSRV group source: the mount needs the bridge, and
    # `epics-pva-rs` cannot depend on the bridge without a cyclic package
    # dependency, which cargo rejects outright (measured — see
    # doc/qsrv-rtems-design.md §9.7). It is still the only target PVA binary, so
    # this stays one entry, not two.
    #
    # It carries `required-features = ["qsrv-core"]`, which the loop below
    # supplies from CRATE_FEATURES. That combination is safe here and was
    # measured, because the manifest comment the binary arrived with warned the
    # opposite: an unmet `required-features` makes cargo silently SKIP the
    # target for the plural forms (`--bins`, `--all-targets`), but an explicit
    # `--bin NAME` — which is what this loop issues — is a hard error:
    #   error: target `qsrv-rs` in package `epics-bridge-rs` requires the
    #          features: `qsrv-bin`
    # So a future edit that drops the feature selection fails loudly rather than
    # passing vacuously.
    epics-bridge-rs:rtems-pva-ioc
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
    # The bridge's five binaries. All host-only, and for a reason the CLI
    # tools above do not share: each is `required-features`-gated on a feature
    # the target selection does not carry, so none is even reachable under
    # `--features qsrv-core`. `qsrv-rs` additionally needs `clap`,
    # `tracing-subscriber` and `run_ca_pva_qsrv_ioc` — the last of which is
    # `#[cfg(all(feature = "qsrv", not(target_os = "rtems")))]`. The crate's
    # SIXTH binary, `rtems-pva-ioc`, is the target's QSRV mount and is in BINS
    # above, not here.
    #
    # Spelled with underscores because the census computes its pairs from
    # `basename src/bin/*.rs .rs`, and the bridge's `[[bin]]` entries give
    # explicit `path`s whose file names use underscores while the bin NAMES use
    # hyphens (`src/bin/ca_gateway_rs.rs` -> bin `ca-gateway-rs`). Hyphens here
    # would fail the census twice over — "unclassified" and "stale" at once.
    epics-bridge-rs:ca_gateway_rs
    epics-bridge-rs:dual_gateway_rs
    epics-bridge-rs:dual_ioc_rs
    epics-bridge-rs:pva_gateway_rs
    epics-bridge-rs:qsrv_rs
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

# The two build configurations an RTEMS image can be in. The census above says
# WHAT is compiled; this says IN WHICH CONFIGURATION, and the second one is new.
#
# `portability` is what every dev machine gets: `RTEMS_BSP_PREFIX` unset, so
# `epics-rtems-boot`'s build script emits no `rtems_boot_linked` and the closure
# type-checks with no toolchain present.
#
# `image` is what a bootable image actually is — and until this loop it was
# compiled by NOTHING on any machine without a BSP. Everything behind
# `#[cfg(rtems_boot_linked)]` was outside every gate: the `POSIX_Init` anchor,
# `stats`' real extern bindings, and both `_RTEMS_LIBC_*_LAYOUT` refusals. That
# is the same defect as the `--lib` miss at the top of this file, and it cost
# the same way — this gate reported green while a real image build hard-failed
# with `error[E0080]: evaluation panicked: RTEMS libc layout bug`.
#
# Selected through RUSTFLAGS rather than by editing the cfg. `epics-rtems-boot`
# carries a test — `the_libc_layout_refusals_fire_for_every_image_that_can_boot`
# — pinning those refusals to exactly `cfg(all(target_os = "rtems",
# rtems_boot_linked))`, whose own comment says that widening the cfg deletes
# this very gate. Naming the cfg from outside compiles that arm without
# touching it, so one machine reaches both configurations and neither is
# weakened.
#
# NOT `--no-default-features`. The session handoff diagnosed the missing
# coverage as a feature-selection problem; measured, it is not.
# `epics-rtems-boot` declares no `[features]` at all, so the flag cannot reach
# its guards, and the flag is load-bearing for a different reason that must not
# be undone: `epics-pva-rs`'s default `tls` drags `ring` -> `getrandom 0.2`,
# which `compile_error!`s on this target.
#
# This pass is FATAL, not advisory. The failure it exists to catch is the one
# that actually happened (session handoff §7): a `libc` branch missing the
# `time_t` widening, which fires exactly one of the two refusals. Any scheme
# that tolerates "the refusals we already know about" tolerates that too, and
# is the vacuous pass in a new costume. Red here means this workspace cannot
# build a bootable image — a true statement about the workspace, not a
# complaint about the gate.
CONFIGS=(portability image)

# Whatever the operator set stays set; the configuration only ever adds.
BASE_RUSTFLAGS="${RUSTFLAGS:-}"

failed=()

for config in "${CONFIGS[@]}"; do
    case "$config" in
    image) export RUSTFLAGS="$BASE_RUSTFLAGS --cfg rtems_boot_linked" ;;
    *) export RUSTFLAGS="$BASE_RUSTFLAGS" ;;
    esac

    log "===== configuration: $config"

    for crate in "${CRATES[@]}"; do
        feats=()
        [[ -n "${CRATE_FEATURES[$crate]:-}" ]] && feats=(--features "${CRATE_FEATURES[$crate]}")
        log "== [$config] $crate --lib ${feats[*]-}"
        if ! cargo "${COMMON[@]}" -p "$crate" --lib ${feats[@]+"${feats[@]}"}; then
            failed+=("[$config] $crate --lib")
        fi
    done

    for pair in "${BINS[@]}"; do
        crate="${pair%%:*}"
        bin="${pair##*:}"
        # Same per-crate feature selection the `--lib` loop above applies, and
        # for the same reason: `COMMON` carries `--no-default-features`, so a
        # binary whose crate needs a feature to expose the modules it uses would
        # otherwise be built in a configuration nobody ships. For
        # `epics-bridge-rs:rtems-pva-ioc` the feature is also `required-features`,
        # so omitting it here does not quietly build the wrong thing — it fails
        # the run outright, which is the direction this gate wants to fail in.
        feats=()
        [[ -n "${CRATE_FEATURES[$crate]:-}" ]] && feats=(--features "${CRATE_FEATURES[$crate]}")
        log "== [$config] $crate --bin $bin ${feats[*]-}"
        if ! cargo "${COMMON[@]}" -p "$crate" --bin "$bin" ${feats[@]+"${feats[@]}"}; then
            failed+=("[$config] $crate --bin $bin")
        fi
    done
done

if [[ ${#failed[@]} -gt 0 ]]; then
    echo "RTEMS gate FAILED:" >&2
    printf '  %s\n' "${failed[@]}" >&2
    exit 1
fi

log "RTEMS gate: every crate and target binary compiles for $TARGET, in both"
log "the portability and the image configuration."
