#!/usr/bin/env bash
# Release-build the Linux x86_64 IOC binaries with the PGO profile.
#
#   scripts/pgo-train.sh   # once per tag: writes target/pgo/merged.profdata
#   scripts/pgo-build.sh   # target/x86_64-unknown-linux-gnu/release/...
#
# Refuses a profile older than the newest tracked source file. A function
# whose control flow changed since training has its counts discarded with one
# "hash mismatch" warning, and a new function gets no profile and no warning;
# either way the build succeeds and ships without the gain.
#
# Measurement builds (wpab2, PT, perf-stat) stay plain: under the profile
# run_process_frame is inlined into the scan sweep, so its +0x0 PT anchor
# counts 0 and the instruction counts no longer compare with C.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

TRIPLE=x86_64-unknown-linux-gnu
PROFILE="$REPO_ROOT/target/pgo/merged.profdata"

[[ -f "$PROFILE" ]] \
    || { echo "pgo-build.sh: no profile at $PROFILE; run scripts/pgo-train.sh first" >&2; exit 1; }
newest=$(git ls-files -z -- '*.rs' '*.dbd' '*.toml' Cargo.lock \
    | xargs -0 stat -c '%Y %n' | sort -n | tail -1)
if (( ${newest%% *} > $(stat -c %Y "$PROFILE") )); then
    echo "pgo-build.sh: profile is older than ${newest#* }; rerun scripts/pgo-train.sh" >&2
    exit 1
fi

# epics_rs_pgo marks the binary so a measurement rig can refuse it.
export RUSTFLAGS="-Cprofile-use=$PROFILE -Clink-arg=-Wl,--defsym=epics_rs_pgo=0"
EPICS_RS_BUILD_EXEC_BACKEND=thread cargo build --release --locked --target "$TRIPLE" \
    -p epics-ca-rs --bin realtime-ca-ioc
EPICS_RS_BUILD_EXEC_BACKEND=thread cargo build --release --locked --target "$TRIPLE" \
    -p epics-bridge-rs --features qsrv-core,pvalink --bin realtime-pva-ioc
cargo build --release --locked --target "$TRIPLE" -p epics-ca-rs --bin softioc-rs
