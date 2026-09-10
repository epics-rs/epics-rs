#!/usr/bin/env bash
#
# publish.sh - Publish the epics-rs workspace crates to crates.io.
#
# `cargo publish --workspace` (cargo >= 1.90) selects every member that is
# not `publish = false`, uploads them in dependency order, waits for index
# propagation between crates, and verifies each package against the ones
# packaged beside it — so the dev-dependency cycle epics-macros-rs <->
# epics-base-rs and an intra-workspace dependency not yet on crates.io both
# verify. Nothing here names a crate: a new workspace member ships the day
# it is added. The version comes from `[workspace.package] version` in the
# root Cargo.toml (every crate inherits it via `version.workspace`).
#
# Publishing is IRREVERSIBLE: a yanked version's number can never be reused.
# Run with --dry-run first. Every argument goes to `cargo publish`, so a run
# that failed part-way resumes with `--exclude <crate>` for each crate that
# already landed.
#
# Usage:
#   ./scripts/publish.sh --dry-run             # package + verify, no upload
#   ./scripts/publish.sh                       # publish for real, in order
#   ./scripts/publish.sh --exclude epics-rs    # resume without a crate
#
# Prerequisites: a crates.io token (~/.cargo/credentials.toml or
# CARGO_REGISTRY_TOKEN), a clean tree at the release commit, and the tag pushed.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

case "${1:-}" in
    -h|--help)
        sed -n '2,25p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
esac

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found. Run ./scripts/setup.sh first." >&2
    exit 1
fi

# Workspace version — the single source every crate inherits.
VERSION="$(awk -F'"' '/^version = /{print $2; exit}' Cargo.toml)"
if [[ -z "$VERSION" ]]; then
    echo "error: could not read [workspace.package] version from Cargo.toml" >&2
    exit 1
fi

MODE="PUBLISH"
for arg in "$@"; do
    if [[ "$arg" == "--dry-run" || "$arg" == "-n" ]]; then
        MODE="DRY-RUN"
    fi
done
echo "==> $MODE epics-rs workspace @ $VERSION"
if [[ "$MODE" == "PUBLISH" ]]; then
    echo "    This is IRREVERSIBLE. Ctrl-C now to abort."
fi

# `--locked` publishes exactly what Cargo.lock pins.
exec cargo publish --workspace --locked "$@"
