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
#     `src/bin/*.rs`. The one binary anyone boots on the target — realtime-ca-ioc
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

# The RTEMS target this gate compiles for.
#
# ADOPTED DEVIATION: by default this is the custom spec carrying
# `has-thread-local: true` (measured to take the per-thread heap leak from
# 136 B to 0 — `doc/rtems-tls-spec-deviation.md`, evidence in
# `doc/upstream-rtems-bugs/rust-std-rtems-tls-thread-leak.md`). Every RTEMS
# image this workspace ships is built with the flip, so the gate compiles the
# same native-TLS codegen the image uses: `has-thread-local` flips
# `cfg(target_thread_local)`, which selects a *different* std TLS path, and a
# gate on the builtin triple would type-check the path the image does not take.
#
# The spec is GENERATED from the active nightly (`scripts/rtems-tls-spec.sh`
# adds exactly the one key to the builtin spec print), never frozen, so it
# tracks whatever toolchain is installed rather than pinning one nightly's
# data-layout. `cargo check` does not link, so no BSP/linker is needed here.
#
# RETIREMENT: when upstream sets `has_thread_local: true` in
# `armv7_rtems_eabihf.rs`, `rtems-tls-spec.sh` refuses (the key is already
# present), which fails this gate loudly — the signal to set
# `RTEMS_USE_STOCK_SPEC=1` permanently, delete the spec wiring, and drop the
# deviation doc. `RTEMS_USE_STOCK_SPEC=1` also forces the pre-adoption builtin
# triple for a one-off comparison.
JSON_SPEC_FLAGS=()
if [[ "${RTEMS_USE_STOCK_SPEC:-0}" == "1" ]]; then
    TARGET="armv7-rtems-eabihf"
else
    TARGET="$(./scripts/rtems-tls-spec.sh)"
    JSON_SPEC_FLAGS=(-Zjson-target-spec)
    # check does not link, but export the linker for the JSON stem anyway so a
    # future `build` invocation through the same COMMON does not silently pick a
    # host linker. Stem = basename without .json, upper-cased, non-alnum -> _.
    _stem="$(basename "$TARGET" .json)"
    _env="CARGO_TARGET_$(printf '%s' "$_stem" | tr '[:lower:]-' '[:upper:]_')_LINKER"
    export "$_env=arm-rtems6-gcc"
fi
# `-Zbuild-std` is required: there is no prebuilt std for this triple. Its
# argument is QUOTED because the comma in `std,panic_abort` is cargo's crate
# separator, not bash's: unquoted inside an array literal it reads as a comma
# where a space belongs, which is a real enough mistake that shellcheck flags
# it (SC2054). Quoting states the intent at the site; a disable directive
# would only silence the reader.
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
COMMON=(+nightly check --locked --no-default-features "-Zbuild-std=std,panic_abort" ${JSON_SPEC_FLAGS[@]+"${JSON_SPEC_FLAGS[@]}"} --target "$TARGET")

# The crates that must compile for the target.
#
# `epics-libcom-rs` is the runtime/socket layer `epics-base-rs` re-exports
# (issue #55). It is listed in its own right rather than left to arrive as a
# dependency: a `-p epics-base-rs --lib` build compiles only the parts of it
# base names, so a target break in a module base does not reach — the whole
# `net` socket half is gated off RTEMS, for instance — would be invisible here
# exactly as `src/bin` was before this script existed.
CRATES=(epics-libcom-rs epics-base-rs epics-ca-rs epics-pva-rs epics-rtems-boot epics-bridge-rs)

# Per-crate feature selection, on top of COMMON's `--no-default-features`.
#
# For the first five crates the empty selection IS the target's configuration,
# so they are absent here. `epics-bridge-rs` is the first crate for which that
# is false, and silently, in the direction that reports green: with no features
# at all it compiles `error`, `convert` and `lib.rs` and nothing else — the
# `qsrv` module is behind `#[cfg(feature = "qsrv-core")]`, so a featureless
# build type-checks 0 of the 11,281 production lines this gate exists to cover.
#
# So the selection is stated rather than defaulted, and stated per crate rather
# than globally: it says out loud that this crate's target configuration is a
# CHOICE. `qsrv-core,pvalink` and not the full `qsrv`: `qsrv` additionally
# implies `qsrv-bin` machinery this target never runs, but `pvalink` — the
# pva:// record-link resolver — is now on the target (design stage 4). It pulls
# `epics-pva-rs/client`, whose UDP transport is cfg-gated out so the client
# compiles for the triple (the ratchet probe below measures zero), and does NOT
# pull `tls`/`pkcs12` (no `ring`, no `getrandom 0.2`) because the bridge's
# `epics-pva-rs` dependency is `default-features = false`. Before stage 4 this
# was `qsrv-core` alone, because `client_native` was 47 errors on this target
# (doc/qsrv-rtems-design.md §0 probe D); design §5 stage 4 closed them.
#
# `epics-ca-rs` selects `client-core` for the same reason, one protocol over:
# `realtime-ca-ioc` mounts the `ca://` record-link resolver (design stage C5), and
# `calink` drives a live `CaClient`. `client-core`, not `client`: the split is
# stated in `crates/epics-ca-rs/Cargo.toml` and it is the circuit, the search
# engine, subscriptions and the resolver WITHOUT the beacon monitor / repeater
# / service discovery / reverse-DNS stack a record link never reaches. This is
# the same selection the CA ratchet below measures, so the probe and the built
# binary agree by construction rather than by two people remembering.
declare -A CRATE_FEATURES=(
    [epics-bridge-rs]="qsrv-core,pvalink"
    [epics-ca-rs]="client-core"
)

# Binaries built for the target, as `crate:bin` pairs.
BINS=(
    epics-ca-rs:realtime-ca-ioc
    # `realtime-pva-ioc` is an `epics-bridge-rs` binary, not an `epics-pva-rs` one.
    # It moved when it grew a QSRV group source: the mount needs the bridge, and
    # `epics-pva-rs` cannot depend on the bridge without a cyclic package
    # dependency, which cargo rejects outright (measured — see
    # doc/qsrv-rtems-design.md §9.7). It is still the only target PVA binary, so
    # this stays one entry, not two.
    #
    # It carries `required-features = ["qsrv-core", "pvalink"]`, which the loop
    # below supplies from CRATE_FEATURES. That combination is safe here and was
    # measured, because the manifest comment the binary arrived with warned the
    # opposite: an unmet `required-features` makes cargo silently SKIP the
    # target for the plural forms (`--bins`, `--all-targets`), but an explicit
    # `--bin NAME` — which is what this loop issues — is a hard error:
    #   error: target `qsrv-rs` in package `epics-bridge-rs` requires the
    #          features: `qsrv-bin`
    # So a future edit that drops the feature selection fails loudly rather than
    # passing vacuously.
    epics-bridge-rs:realtime-pva-ioc
)

# Binaries in the crates above that are deliberately NOT built for the target.
#
# This list exists so the check below can be a CENSUS rather than a search.
# The previous version grepped `src/bin` for the literal `target_os = "rtems"`
# and demanded a match be in BINS — which only ever sees a binary that says
# "rtems" in its own source text. A binary gated in `Cargo.toml`
# (`required-features`), gated by a feature predicate, or gated by nothing at
# all was invisible to it and would land outside the gate exactly as
# `realtime-ca-ioc` did. Requiring every binary to be classified one way or the
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
    # SIXTH binary, `realtime-pva-ioc`, is the target's QSRV mount and is in BINS
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
        # `epics-bridge-rs:realtime-pva-ioc` the feature is also `required-features`,
        # so omitting it here does not quietly build the wrong thing — it fails
        # the run outright, which is the direction this gate wants to fail in.
        #
        # Each target binary is checked twice: once as shipped by default (a
        # clean IOC — no probe records, no probe threads) and once with
        # `bringup-probes`, the measurement rig the bring-up box's build
        # scripts select (doc/calink-rtems-design.md §11.7 item 3). Both are
        # real images someone boots, so both are census, not choice.
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
    echo "RTEMS gate FAILED:" >&2
    printf '  %s\n' "${failed[@]}" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# The PVA client probe: a RATCHET, not a pass/fail build.
#
# `doc/pvalink-rtems-design.md` §5 stage 2 asks for this crate's target
# selection to be "extended to include `client`". Taken literally — adding
# `client` to CRATE_FEATURES — the whole gate goes red, because the client's
# remaining errors are all UDP (newlib has no `recvmsg`/`cmsghdr`/`CMSG_*`, no
# `IP_PKTINFO` original-destination recovery) and §4.2 stages that work AFTER
# this one, deliberately. A gate that is red for work nobody has started yet
# reports nothing, and stages 3-5 all extend the green one above.
#
# So the selection is measured rather than built: the count is the artefact.
# §1.2 could only report "47, and that is a lower bound" because an unresolved
# import poisons its module and rustc then suppresses downstream errors in code
# naming its items — so a file reporting zero was ambiguous between "compiles"
# and "hidden". Pinning the number here is what stops that count drifting
# unobserved in either direction:
#
#   * MORE than expected — a change put the client further from the target.
#     That is the regression this exists to catch, and it is fatal.
#   * FEWER than expected — someone did the work and did not lower the number.
#     Also fatal, and for the reason the binary census above is bidirectional:
#     a bound nobody updates stops being a measurement and becomes decoration.
#
# History, `--no-default-features --features client -p epics-pva-rs --lib`:
#
#     47  before stage 1        29 primary + an 18-error [u8] cascade
#     29  after stages 1 and 2  cascade gone with search_engine's TcpStream
#     28  after server_conn's unconditional tokio::net import was removed
#      0  after stage 4         UDP transport cfg-gated out of the target build
#
# Stage 4 closed the remaining 28 (§5): the whole UDP SEARCH/beacon transport —
# `client_native::udp`, the legacy `client_native::search`, and every UDP-only
# item in `search_engine.rs` (the `SearchTransport::Udp` variant, `UdpTransport`
# and its `bind_udp`/broadcast, the bind/join/fanout helpers, `SearchTarget`) —
# is `#[cfg(not(target_os = "rtems"))]`. On the target `SearchTransport` has the
# single `NameServersOnly` variant, so "no UDP socket" is a fact about the type
# rather than a runtime branch, and the three UDP-config spawn entry points
# construct `NameServersOnly` through `env_transport`/`config_transport`. This is
# what lets the `epics-bridge-rs:realtime-pva-ioc --features qsrv-core,pvalink` bin
# above pull `epics-pva-rs/client` and still compile for the target.
#
# The count stays pinned even at zero: a probe that regressed the client back
# off the target would raise it, and this comparison is what turns that into a
# named failure rather than a silently-red build. With zero errors rustc emits
# no "due to N previous errors" line, which the extraction below reads as 0.
#
# Portability configuration only: every gate is name resolution / `cfg`, which
# `rtems_boot_linked` does not reach, so the second configuration would cost a
# full build to re-measure an identical number.
PVA_CLIENT_TARGET_ERRORS=0

log "== [probe] epics-pva-rs --lib --features client (ratchet)"
export RUSTFLAGS="$BASE_RUSTFLAGS"
# rustc's own summary line ("due to 28 previous errors"), which is the number
# §1.2 and §9 quote. Counting diagnostics out of `--message-format=json`
# instead means re-deriving it from a field order cargo does not promise, and
# getting the "could not compile" summary — itself `"level":"error"` — in or
# out of the total by accident. No line at all means zero errors, and the
# comparison below is what turns that into a failure rather than a `0`
# silently passing through some other arm.
client_errors=$(cargo "${COMMON[@]}" -p epics-pva-rs --lib --features client 2>&1 |
    grep -oP 'due to \K\d+(?= previous error)' || true)
client_errors=${client_errors:-0}

if [[ "$client_errors" -ne "$PVA_CLIENT_TARGET_ERRORS" ]]; then
    echo "RTEMS gate FAILED: the PVA client's target error count moved." >&2
    echo "  expected: $PVA_CLIENT_TARGET_ERRORS (PVA_CLIENT_TARGET_ERRORS in $0)" >&2
    echo "  measured: $client_errors" >&2
    if [[ "$client_errors" -gt "$PVA_CLIENT_TARGET_ERRORS" ]]; then
        echo "A change moved epics-pva-rs's client further from $TARGET. Reproduce with:" >&2
    else
        echo "Fewer errors than recorded — update the number so it keeps measuring:" >&2
    fi
    echo "  cargo ${COMMON[*]} -p epics-pva-rs --lib --features client" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# The CA client had the same RATCHET, and no longer needs one.
#
# `doc/calink-rtems-design.md` §6 stage C5 mounts the `ca://` record-link
# resolver in `realtime-ca-ioc`, which needs `epics-ca-rs`'s CLIENT to compile for
# the target — `calink` drives a live `CaClient` (§2.1). While that was work in
# progress the selection was MEASURED rather than built, exactly as the PVA one
# above still is, because a gate red for unstarted work reports nothing:
#
# `--no-default-features --features client-core -p epics-ca-rs --lib`:
#
#     22  before the split      measured with `client`/`channel`/`calink` merely
#                               un-gated: 7 of them are the discovery stack's
#     15  after the split       11 primary + a 4-error [u8] cascade in
#                               `search.rs`'s select! (design §1.3)
#     14  FIONREAD single owner  the client's flow-control probe now reaches
#                               `runtime::blocking_io::pending_bytes`, which
#                               supplies the constant `libc` omits for RTEMS
#     11  the `dial_ca` seam     the circuit's dial, its keepalive and its
#                               receive-queue probe became one compile-time
#                               transport choice; `tokio::net` and `socket2`
#                               are named only on the hosted arm
#     10  name-server dial       `run_nameserver_connection` came through the
#                               same seam, so `tokio::net` is gone from the
#                               client entirely
#      0  stage C5              UDP SEARCH cfg-gated out of the target build
#
# Stage C5 closed the remaining 10 the way PVA stage 4 closed its 28, and for
# the same reason: they were all UDP. On the target `search::SearchTransport`
# has the single `NameServersOnly` variant — `UdpTransport`, `bind_udp`, the
# fanout/DNS-refresh helpers, `run_search_engine` and the whole
# `EPICS_CA_ADDR_LIST` parse (`AddrEntry`, `resolve_host`,
# `parse_addr_list_with_hostnames`, `append_auto_addr_entries`) are
# `#[cfg(not(target_os = "rtems"))]`. "No UDP socket" is a fact about the type,
# not a runtime branch, and `CaClient::new_with_config` reaches
# `name_servers_only_search_engine` by `cfg` rather than by choice.
#
# The probe is RETIRED rather than left pinned at zero, and the replacement is
# STRICTER, not looser: `CRATE_FEATURES[epics-ca-rs]="client-core"` above makes
# the crate loop build this exact selection as a hard pass/fail — in BOTH the
# portability and the image configuration, where the probe only ever ran the
# first — and the binary loop then links `realtime-ca-ioc` on top of it. A count
# pinned at 0 and a build that must succeed are the same assertion; keeping
# both would mean a second check that can only ever fail after the first
# already did, which is the decoration this script's own census comment warns
# about. `tokio::net`, `socket2` and a raw `FIONREAD` re-entering the target
# client still fail the gate — one loop earlier, and twice.
#
# The PVA probe above stays, because it is not duplicative: CRATE_FEATURES has
# no `epics-pva-rs` entry, so `--features client` is built nowhere else.

if [[ "${RTEMS_USE_STOCK_SPEC:-0}" == "1" ]]; then
    log "RTEMS gate (STOCK builtin spec, RTEMS_USE_STOCK_SPEC=1): every crate and"
    log "target binary compiles for armv7-rtems-eabihf, in both the portability"
    log "and the image configuration."
else
    log "RTEMS gate (has-thread-local spec, doc/rtems-tls-spec-deviation.md): every"
    log "crate and target binary compiles for the generated native-TLS spec, in"
    log "both the portability and the image configuration."
fi
log "PVA client (--features client): $PVA_CLIENT_TARGET_ERRORS target errors (UDP transport cfg-gated out)."
log "CA client: built, not probed (CRATE_FEATURES[epics-ca-rs]=client-core)."
