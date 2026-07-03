#!/usr/bin/env bash
#
# publish.sh - Publish the epics-rs workspace crates to crates.io.
#
# The 16 library crates are published in dependency (topological) order so
# each crate's internal dependencies already exist on crates.io by the time it
# uploads. `cargo publish` (>= 1.66) waits for index propagation between
# crates automatically. The version comes from `[workspace.package] version`
# in the root Cargo.toml (every crate inherits it via `version.workspace`).
#
# Publishing is IRREVERSIBLE: a yanked version's number can never be reused.
# Run with --dry-run first. The script is resumable — a crate whose current
# version is already on crates.io is skipped, so a re-run after a mid-way
# failure continues where it stopped.
#
# Usage:
#   ./scripts/publish.sh --dry-run   # package + verify every crate, no upload
#   ./scripts/publish.sh             # publish for real, in order
#
# Prerequisites: a crates.io token (~/.cargo/credentials.toml or
# CARGO_REGISTRY_TOKEN), a clean tree at the release commit, and the tag pushed.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

DRY_RUN=""
case "${1:-}" in
    --dry-run) DRY_RUN="--dry-run" ;;
    -h|--help)
        sed -n '2,26p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    "") ;;
    *)
        echo "error: unknown argument '$1' (use --dry-run or --help)" >&2
        exit 1
        ;;
esac

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found. Run ./scripts/setup.sh first." >&2
    exit 1
fi

# Topological publish order. These are PACKAGE names, not directory names:
# crates/modbus-rs publishes as `epics-modbus-rs`. `cargo publish -p modbus-rs`
# would fail with "did not match any packages".
CRATES=(
    epics-macros-rs
    epics-base-rs
    epics-ca-rs
    epics-pva-rs
    epics-tools-rs
    asyn-rs
    epics-bridge-rs
    ad-core-rs
    scaler-rs
    motor-rs
    mqtt-rs
    optics-rs
    epics-modbus-rs
    std-rs
    ad-plugins-rs
    epics-rs
)

# Workspace version — the single source every crate inherits.
VERSION="$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)"
if [[ -z "$VERSION" ]]; then
    echo "error: could not read [workspace.package] version from Cargo.toml" >&2
    exit 1
fi

TOTAL="${#CRATES[@]}"
MODE="PUBLISH"
[[ -n "$DRY_RUN" ]] && MODE="DRY-RUN"
echo "==> $MODE epics-rs workspace @ $VERSION  ($TOTAL crates, in dependency order)"
if [[ -z "$DRY_RUN" ]]; then
    echo "    This is IRREVERSIBLE. Ctrl-C now to abort."
fi

published=0 skipped=0 idx=0
for pkg in "${CRATES[@]}"; do
    idx=$((idx + 1))
    printf '==> [%2d/%d] %s %s\n' "$idx" "$TOTAL" "$pkg" "$VERSION"

    # Command substitution (not a pipe): $? is cargo's real exit status, so a
    # failure is never masked. `--locked` publishes exactly what Cargo.lock pins.
    if out="$(cargo publish -p "$pkg" --locked $DRY_RUN 2>&1)"; then
        echo "$out" | tail -3 | sed 's/^/    /'
        echo "    OK: $pkg"
        published=$((published + 1))
    else
        rc=$?
        # Idempotent resume: an already-uploaded version is not a failure.
        if echo "$out" | grep -qiE 'already (exists|uploaded)|is already uploaded|already been published'; then
            echo "    SKIP (already on crates.io): $pkg"
            skipped=$((skipped + 1))
        else
            echo "$out" | tail -25 | sed 's/^/    /'
            echo "    FAILED: $pkg (exit $rc) — stopping. Fix, then re-run to resume." >&2
            exit 1
        fi
    fi
done

echo "==> Done: $published published, $skipped skipped, $TOTAL total @ $VERSION"
[[ -n "$DRY_RUN" ]] && echo "    (dry run — nothing was uploaded)"
