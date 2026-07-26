#!/usr/bin/env bash
#
# vxworks-check.sh - The VxWorks 7 portability gate for the epics-rs workspace.
#
# The counterpart of `rtems-check.sh`: it answers "does the VxWorks closure
# still compile for x86_64-wrs-vxworks". Same two axes — a binary census that
# says WHAT is compiled, and a per-crate/per-bin loop that compiles it — and
# the same bidirectional-census discipline, because the failure it exists to
# stop is the same one: a target binary landing outside the gate.
#
# WHAT IS DIFFERENT FROM THE RTEMS GATE, AND WHY IT IS NOT A TRIPLE SWAP
#
# Three structural differences, none of them cosmetic:
#
#   * HALF THIS GATE CANNOT RUN IN CI. The RTEMS gate runs whole on a GitHub
#     runner because its toolchain is a public `nightly` + `rust-src` and its
#     libc fixes ride the workspace `[patch.crates-io]`. Here the libc fix has
#     to come from OUTSIDE the tree, by one of the two shapes the contract
#     below names, and neither is a public artefact a runner can fetch: a
#     prepared toolchain, or a checkout of a patched libc named by a
#     config-level patch. So the CI job runs the census and nothing else, and
#     this gate reports two different things depending on whether it can see a
#     toolchain. It never reports a bare success for the half it did not run.
#
#     Note which `[patch]` reaches `-Zbuild-std`, because an earlier revision of
#     this header got it wrong in a way that ruled out the operator path the
#     box actually uses. A MANIFEST patch (this workspace's root `Cargo.toml`)
#     does NOT reach it — build-std resolves std against rust-src's own
#     `library/Cargo.lock`, which no manifest of ours is part of. A CONFIG-LEVEL
#     patch — `~/.cargo/config.toml`, or `--config` on the command line — DOES.
#     MEASURED both ways: the box's RTEMS bring-up has carried
#     `[patch.crates-io] libc = { path = ".../libc-bringup" }` in
#     `~/.cargo/config.toml` with the comment "so -Zbuild-std also picks it up",
#     and on this target all eleven rows go green on a STOCK nightly under
#     `--config 'patch.crates-io.libc.path="…/libc-vx"'`.
#
#   * THERE IS NO SPEC-GENERATION MACHINERY. `x86_64-wrs-vxworks` is a builtin
#     triple whose `has-thread-local` is already true (measured via
#     `--print target-spec-json`), so the whole `rtems-tls-spec.sh` /
#     `-Zjson-target-spec` / `CARGO_TARGET_…_LINKER` stem apparatus — and with
#     it the retirement trip-wire that machinery exists to arm — has no
#     counterpart here. TARGET is a literal string. This is deletion, not a
#     port.
#
#   * THERE IS ONE BUILD CONFIGURATION, PERMANENTLY. See CONFIGS below. On
#     RTEMS the second (`image`) configuration exists because a build script
#     compiles C against a BSP and emits `--cfg rtems_boot_linked` to say it
#     did. The VxWorks statistics backend compiles no C of ours and declares
#     only symbols an RTP resolves from the C library it links unconditionally,
#     so there is nothing for a `vxworks_boot_linked` to gate. A second
#     configuration here would be an axis no build can be in. That is a
#     property of the port, not a gap in this script.
#
# Usage:
#   ./scripts/vxworks-check.sh          # census, then the target rows if able
#   ./scripts/vxworks-check.sh --quiet  # only report failures (and the skip)
#
#   VXWORKS_TOOLCHAIN=vx-nightly ./scripts/vxworks-check.sh
#   VXWORKS_TOOLCHAIN=nightly \
#     VXWORKS_CARGO_CONFIG='patch.crates-io.libc.path="/path/to/libc-vx"' \
#     ./scripts/vxworks-check.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# The VxWorks target this gate compiles for. A literal, unlike the RTEMS gate's
# generated spec path — see the header.
TARGET="x86_64-wrs-vxworks"

# ---------------------------------------------------------------------------
# The toolchain contract.
#
# A STOCK toolchain with a STOCK libc CANNOT produce green target rows here,
# for two independent upstream `libc` reasons:
#
#   * `pread`/`pwrite` were removed from libc's vxworks module in 0.2.187
#     (PR #5129, collateral of deprecating kernel-mode `off64_t`); present
#     <= 0.2.186. std imports both (`library/std/src/sys/fd/unix.rs:32,406`),
#     so any nightly whose rust-src lock pins libc >= 0.2.187 fails to build
#     std.
#   * `killpg` is referenced by std's own vxworks process code
#     (`library/std/src/sys/process/unix/vxworks.rs:179`) and is declared for
#     vxworks nowhere in libc, in any version. CONFIRMED — see
#     `doc/upstream-rust-targets/` for the trace and the shim-not-extern
#     conclusion.
#
# Which of the two you hit depends on the nightly, since build-std resolves
# libc from rust-src's own lock: a 2026-07-09 nightly pins 0.2.185 and shows
# only `killpg`; a current one pins 0.2.188 and shows both.
#
# TWO OPERATOR SHAPES, BOTH FIRST-CLASS. They differ in where the patched libc
# comes from, not in what is measured:
#
#   1. VXWORKS_TOOLCHAIN names a self-contained prepared toolchain whose
#      bundled rust-src already carries the fixes. Nothing else to set.
#   2. VXWORKS_TOOLCHAIN names a stock toolchain (`nightly`) and
#      VXWORKS_CARGO_CONFIG carries a config-level patch pointing at a checkout
#      of a patched libc. This works because a CONFIG-level `[patch]` reaches
#      `-Zbuild-std` where a manifest one does not — see the header. This is
#      the shape the bring-up box actually runs, and the shape the eleven
#      measured green rows were taken under.
#
# VXWORKS_TOOLCHAIN unset is the normal state on a dev machine and in CI. It is
# not an error and it is not a pass: the census rows below still run and still
# fail loudly, and the summary says which half did not.
TOOLCHAIN="${VXWORKS_TOOLCHAIN:-}"
CARGO_CONFIG="${VXWORKS_CARGO_CONFIG:-}"

# `-Zbuild-std` is required: there is no prebuilt std for this triple. Its
# argument is QUOTED because the comma in `std,panic_abort` is cargo's crate
# separator, not bash's: unquoted inside an array literal it reads as a comma
# where a space belongs, which is a real enough mistake that shellcheck flags
# it (SC2054). Quoting states the intent at the site; a disable directive
# would only silence the reader.
COMMON=(+"$TOOLCHAIN" check --no-default-features "-Zbuild-std=std,panic_abort" --target "$TARGET")

# `--locked` is load-bearing for the reason `rtems-check.sh` gives: this gate's
# claim is "the dependency set IN THIS TREE compiles for the target", and
# without it cargo may re-resolve mid-run and answer about a different set,
# leaving the new resolution behind as a working-tree `M Cargo.lock`.
#
# A config-level PATH patch is measured incompatible with it, and not by
# accident: the committed lock pins libc to a source, the path override
# replaces that source, so resolution MUST change and `--locked` exists
# precisely to refuse a resolution change (`error: cannot update the lock file
# ... because --locked was passed`). The two cannot both hold.
#
# So the flag is dropped for shape 2 and only for shape 2 — and never silently.
# What `--locked` was protecting is a real loss, so the notice says what the
# rows now measure instead, in the same breath as saying the flag is gone. It
# goes out with `echo`, not `log`, for the same reason the skip banner does:
# `--quiet` must not be able to hide a weakened claim.
if [[ -n "$CARGO_CONFIG" ]]; then
    COMMON+=(--config "$CARGO_CONFIG")
    echo "vxworks-check: --locked DROPPED - config-level path patch active (VXWORKS_CARGO_CONFIG); the lock cannot pin a path override, so these rows measure the PATCHED resolution, not the committed one."
else
    COMMON+=(--locked)
fi

# The composed invocation, so which flags are in play is observable rather than
# inferred from the two branches above. `${COMMON[*]:1}` drops the `+toolchain`
# element, which is a bare `+` when VXWORKS_TOOLCHAIN is unset and would print
# as a broken command line.
log_common() { log "vxworks-check: target-row invocation: cargo +${TOOLCHAIN:-<unset>} ${COMMON[*]:1}"; }

# The crates that must compile for the target.
#
# `epics-rtems-boot` IS in this list, which the gate spec for this script did
# not expect — it assumed the crate was RTEMS-only and that a separate
# `epics-vxworks-boot` would appear. That assumption is no longer true: this
# crate owns the per-OS IOC statistics funnel (`src/stats/`), whose VxWorks
# backend is the only thing on this target that reads descriptor usage, heap
# usage, or the task census. Its RTEMS half is `#[cfg]`-gated away here, and
# its build script returns early on any non-RTEMS target, so nothing about the
# BSP boot glue comes with it.
#
# Listed in its own right rather than left to arrive through
# `epics-libcom-rs`, which depends on it on this target only. Same reason
# `epics-libcom-rs` is listed rather than left to arrive through
# `epics-base-rs`: a crate reached as a dependency is resolved with the
# features its dependent asked for, so a break under this gate's own
# `--no-default-features` selection would be invisible.
CRATES=(epics-libcom-rs epics-base-rs epics-ca-rs epics-pva-rs epics-rtems-boot epics-bridge-rs)

# Per-crate feature selection, on top of COMMON's `--no-default-features`.
# Identical to the RTEMS gate's, and deliberately so: these are the same two
# target IOCs with the same record-link resolvers mounted, and a selection that
# drifted per OS would mean the two targets stopped being comparable.
#
# `epics-bridge-rs` with no features compiles `error`, `convert` and `lib.rs`
# and nothing else — the `qsrv` module is behind `qsrv-core` — so a featureless
# build would type-check none of the production lines this gate covers.
# `qsrv-core,pvalink` and not `qsrv`: the latter implies `qsrv-bin` machinery no
# target runs. `epics-ca-rs` selects `client-core` because `realtime-ca-ioc` mounts
# the `ca://` resolver, and `client-core` rather than `client` is the circuit,
# search engine, subscriptions and resolver WITHOUT the beacon/repeater/
# discovery stack a record link never reaches.
declare -A CRATE_FEATURES=(
    [epics-bridge-rs]="qsrv-core,pvalink"
    [epics-ca-rs]="client-core"
)

# Binaries built for the target, as `crate:bin` pairs.
#
# These keep their `rtems-` names on a VxWorks image. Decided rather than
# defaulted: they are the target IOCs regardless of which RTOS the image runs
# on, the box rig already stages them under different file names
# (`ca.vxe`/`pvaioc.vxe`), and a rename would touch every doc and rig path for
# no compilation benefit. Revisit is a naming decision, not this gate's.
BINS=(
    epics-ca-rs:realtime-ca-ioc
    # An `epics-bridge-rs` binary, not an `epics-pva-rs` one — it moved when it
    # grew a QSRV group source, and `epics-pva-rs` cannot depend on the bridge
    # without a cyclic package dependency. It carries
    # `required-features = ["qsrv-core", "pvalink"]`, supplied below from
    # CRATE_FEATURES; an explicit `--bin NAME` with those unmet is a hard error
    # rather than a silent skip, so dropping the selection fails loudly.
    epics-bridge-rs:realtime-pva-ioc
)

# Binaries in the crates above that are deliberately NOT built for the target.
#
# This list is what makes the check below a CENSUS rather than a search: every
# binary must be classified one way or the other, so there is no way for a new
# one to be absent from both and land outside the gate. Identical to the RTEMS
# gate's list, and for a reason stronger than symmetry — every entry is a host
# CLI tool gated by `required-features` or by source that names no RTOS at all,
# so the classification does not depend on which target is being checked.
#
# The bridge's five are spelled with underscores because the census computes
# its pairs from `basename src/bin/*.rs .rs`, and the bridge's `[[bin]]` entries
# give explicit `path`s whose file names use underscores while the bin names use
# hyphens. Hyphens here would fail the census twice over — "unclassified" and
# "stale" at once.
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
    epics-bridge-rs:ca_gateway_rs
    epics-bridge-rs:dual_gateway_rs
    epics-bridge-rs:dual_ioc_rs
    epics-bridge-rs:pva_gateway_rs
    epics-bridge-rs:qsrv_rs
)

QUIET=0
[[ "${1:-}" == "--quiet" ]] && QUIET=1

log() { [[ $QUIET -eq 1 ]] || echo "$@"; }

# ---------------------------------------------------------------------------
# Census: every binary in the VxWorks crates is in BINS or in HOST_ONLY.
#
# Host-runnable: pure filesystem and `grep`, no toolchain of any kind. This is
# the half that runs in CI.
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
    echo "error: these binaries are in a VxWorks crate but are classified neither" >&2
    echo "       as target binaries nor as host-only:" >&2
    printf '  %s\n' "${unclassified[@]}" >&2
    echo "Add each to BINS or to HOST_ONLY in $0. Being in neither is how a" >&2
    echo "target binary lands outside this gate." >&2
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

log "VxWorks binary census: ${#present[@]} binaries, all classified."
log_common

# ---------------------------------------------------------------------------
# The toolchain-gated half. Skipped LOUDLY when the contract is unmet.
#
# Printed with `echo`, not `log`, so `--quiet` cannot hide it. `--quiet` means
# "only report failures", and a silent skip is the one outcome that would be
# mistaken for a pass — which is the entire failure mode this banner exists to
# prevent.
if [[ -z "$TOOLCHAIN" ]]; then
    echo
    echo "==========================================================================="
    echo " VXWORKS TARGET ROWS SKIPPED - VXWORKS_TOOLCHAIN is not set"
    echo "==========================================================================="
    echo " The census above ran and passed. NOTHING was compiled for $TARGET."
    echo
    echo " This is the expected state on a dev machine and in CI, and it is not a"
    echo " failure. It is also not a pass: no statement has been made about whether"
    echo " this tree compiles for VxWorks."
    echo
    echo " To run the target rows you need a libc carrying two upstream fixes:"
    echo "     pread/pwrite restored to src/vxworks/  (removed upstream at 0.2.187)"
    echo "     killpg declared for vxworks            (std references it, libc does not)"
    echo
    echo " A stock toolchain with a stock libc cannot satisfy that. Either shape"
    echo " below supplies it, and they measure the same thing:"
    echo
    echo "   1. a prepared toolchain whose bundled rust-src carries the fixes:"
    echo "        VXWORKS_TOOLCHAIN=<name> $0"
    echo
    echo "   2. a stock toolchain plus a CONFIG-LEVEL patch naming a checkout of"
    echo "      a patched libc - this reaches -Zbuild-std where a manifest"
    echo "      [patch] does not, and is the shape the bring-up box runs:"
    echo "        VXWORKS_TOOLCHAIN=nightly \\"
    echo "        VXWORKS_CARGO_CONFIG='patch.crates-io.libc.path=\"/path/to/libc-vx\"' \\"
    echo "        $0"
    echo "      (shape 2 drops --locked - a path override must change resolution,"
    echo "      which is exactly what --locked refuses. The script says so when it"
    echo "      happens.)"
    echo "==========================================================================="
    echo
    echo "TARGET ROWS ARE BOX-ONLY (both shapes above need a patched libc from"
    echo "outside this tree - a prepared toolchain or a checkout to point at - and"
    echo "neither is a public artefact a runner can fetch, so no CI runner can close"
    echo "this gate; it is closed on the bring-up box)."
    echo "VxWorks gate: census PASSED. TARGET ROWS SKIPPED: no VXWORKS_TOOLCHAIN."
    exit 0
fi

if ! cargo "+$TOOLCHAIN" --version >/dev/null 2>&1; then
    echo "error: VXWORKS_TOOLCHAIN=$TOOLCHAIN names no installed toolchain." >&2
    echo "       cargo +$TOOLCHAIN --version failed." >&2
    exit 1
fi

# The build configurations a VxWorks image can be in. There is exactly one, and
# unlike the RTEMS gate's single-config history that is not a gap to be closed
# later.
#
# RTEMS has two because `epics-rtems-boot`'s build script compiles C against a
# BSP and emits `--cfg rtems_boot_linked` to say it did; everything behind that
# cfg — the `POSIX_Init` anchor, the C-backed statistics externs, the
# `_RTEMS_LIBC_*_LAYOUT` refusals — is compiled by nothing on a BSP-less
# machine unless the gate names the cfg from outside.
#
# The VxWorks statistics backend has no such half. It compiles no C of ours, so
# there is no "was it compiled" question for a cfg to answer, and every symbol
# it declares is one an RTP resolves from the C library it links
# unconditionally. `target_os = "vxworks"` alone is the whole selection. A
# `vxworks_boot_linked` would therefore be a configuration axis no build can be
# in, and adding one for symmetry with RTEMS would make this gate compile a
# state that does not exist.
CONFIGS=(portability)

BASE_RUSTFLAGS="${RUSTFLAGS:-}"

failed=()

for config in "${CONFIGS[@]}"; do
    export RUSTFLAGS="$BASE_RUSTFLAGS"

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
        # Each target binary is checked twice: once as shipped by default (a
        # clean IOC — no probe records, no probe threads) and once with
        # `bringup-probes`, the measurement rig the box's build scripts select.
        # Both are real images someone boots, so both are census, not choice.
        # On this target the probe axis is what compiles the console census
        # call sites that read the VxWorks task registry.
        for probe in "" bringup-probes; do
            sel="${CRATE_FEATURES[$crate]:-}"
            [[ -n "$probe" ]] && sel="${sel:+$sel,}$probe"
            feats=()
            [[ -n "$sel" ]] && feats=(--features "$sel")
            log "== [$config] $crate --bin $bin ${feats[*]-}"
            if ! cargo "${COMMON[@]}" -p "$crate" --bin "$bin" ${feats[@]+"${feats[@]}"}; then
                failed+=("[$config] $crate --bin $bin ${feats[*]-}")
            fi
        done
    done
done

if [[ ${#failed[@]} -gt 0 ]]; then
    echo "VxWorks gate FAILED:" >&2
    printf '  %s\n' "${failed[@]}" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# The PVA client probe: a RATCHET, not a pass/fail build.
#
# `CRATE_FEATURES` has no `epics-pva-rs` entry, so `--features client` is built
# nowhere else in this script and a regression in it would be unobserved. The
# count is the artefact:
#
#   * MORE than expected — a change moved the client further from the target.
#     Fatal; this is the regression the ratchet exists to catch.
#   * FEWER than expected — someone did the work and did not lower the number.
#     Also fatal, for the reason the binary census above is bidirectional: a
#     bound nobody updates stops being a measurement and becomes decoration.
#
# Zero is MEASURED on the box against the merged tree, not carried over from
# the RTEMS gate's identical-looking zero. The two zeroes have the same cause —
# the UDP search/beacon transport is gated out, so `SearchTransport` has its
# single `NameServersOnly` variant and "no UDP socket" is a fact about the type
# rather than a runtime branch — but on RTEMS that gating is spelled
# `cfg(target_os = "rtems")`, which is FALSE for `target_os = "vxworks"`. A gate
# that assumed the RTEMS zero would have been asking cargo to compile the
# hosted tokio/UDP path for a tier-3 target that has no tokio.
#
# What makes it zero here is the embedded-target cfg that widened those arms to
# cover both RTOSes. This ratchet is therefore only meaningful on a tree that
# carries that widening; run against one that does not, it reports the hosted
# path's error count and fails — correctly, because such a tree does not
# compile for this target.
#
# With zero errors rustc emits no "due to N previous errors" line, which the
# extraction below reads as 0.
VXWORKS_PVA_CLIENT_TARGET_ERRORS=0

log "== [probe] epics-pva-rs --lib --features client (ratchet)"
export RUSTFLAGS="$BASE_RUSTFLAGS"
# rustc's own summary line. Counting diagnostics out of `--message-format=json`
# instead means re-deriving the total from a field order cargo does not promise,
# and getting the "could not compile" summary — itself `"level":"error"` — in or
# out of it by accident.
client_errors=$(cargo "${COMMON[@]}" -p epics-pva-rs --lib --features client 2>&1 |
    grep -oP 'due to \K\d+(?= previous error)' || true)
client_errors=${client_errors:-0}

if [[ "$client_errors" -ne "$VXWORKS_PVA_CLIENT_TARGET_ERRORS" ]]; then
    echo "VxWorks gate FAILED: the PVA client's target error count moved." >&2
    echo "  expected: $VXWORKS_PVA_CLIENT_TARGET_ERRORS (VXWORKS_PVA_CLIENT_TARGET_ERRORS in $0)" >&2
    echo "  measured: $client_errors" >&2
    if [[ "$client_errors" -gt "$VXWORKS_PVA_CLIENT_TARGET_ERRORS" ]]; then
        echo "A change moved epics-pva-rs's client further from $TARGET. Reproduce with:" >&2
    else
        echo "Fewer errors than recorded — update the number so it keeps measuring:" >&2
    fi
    echo "  cargo ${COMMON[*]} -p epics-pva-rs --lib --features client" >&2
    exit 1
fi

log "VxWorks gate (toolchain $TOOLCHAIN): every crate and target binary compiles"
log "for $TARGET, in the portability configuration — which is the only"
log "configuration this target has, permanently: there is no C of ours to link,"
log "so there is no image-closure cfg for a second one to name."
log "PVA client (--features client): $VXWORKS_PVA_CLIENT_TARGET_ERRORS target errors (UDP transport cfg-gated out)."
log "CA client: built, not probed (CRATE_FEATURES[epics-ca-rs]=client-core)."
# Repeated at the end, not only at the top: a green summary is what gets pasted
# into a report, and it must carry the caveat that the resolution it measured
# was not the committed one.
#
# An `if` rather than `[[ ... ]] && echo`, because this is the script's last
# command: under `set -e` a false `[[ ]]` there would short-circuit to a
# non-zero exit and turn every unpatched green run red.
if [[ -n "$CARGO_CONFIG" ]]; then
    echo "Resolution: PATCHED via VXWORKS_CARGO_CONFIG, not the committed lock (--locked was dropped)."
fi
