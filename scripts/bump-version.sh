#!/usr/bin/env bash
#
# bump-version.sh - Move the workspace to its next version, in lockstep.
#
# `cargo workspaces version` rewrites `[workspace.package] version` and
# `Cargo.lock`, and re-pins a workspace sibling only when its requirement
# stops matching: a minor bump moves every `[workspace.dependencies]` pin
# and the hand-written `epics-pva-rs` pin in `crates/epics-bridge-rs`, a
# patch bump moves none of them (`^0.30.0` already admits 0.30.1). Every
# release here keeps the pins in lockstep with the version, so this script
# finishes the job: after the tool runs, every `path = ` dependency that
# carries a `version = ` is set to the new version, whatever it held before
# (0.29.3 shipped with the bridge pin still at 0.29.2 after a bump by hand).
# `--force '*'` makes the tool bump the whole workspace rather than the
# crates changed since the last tag. The commit is left to the caller so
# the release branch keeps its own message and history.
#
# Usage:
#   ./scripts/bump-version.sh patch            # 0.30.0 -> 0.30.1
#   ./scripts/bump-version.sh minor            # 0.30.0 -> 0.31.0
#   ./scripts/bump-version.sh custom 0.31.0-rc.1
#
# Prerequisites: cargo-workspaces (`cargo install cargo-workspaces`) and a
# clean tree. Publishing is a separate step: see ./scripts/publish.sh.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

usage() {
    sed -n '2,24p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

case "${1:-}" in
    -h|--help)
        usage
        exit 0
        ;;
    "")
        usage >&2
        exit 1
        ;;
esac

if ! cargo workspaces --version >/dev/null 2>&1; then
    echo "error: cargo-workspaces not found. Run: cargo install cargo-workspaces" >&2
    exit 1
fi

if [[ -n "$(git status --porcelain)" ]]; then
    echo "error: the working tree is not clean; commit or discard first." >&2
    exit 1
fi

read_version() {
    awk -F'"' '/^version = /{print $2; exit}' Cargo.toml
}

BEFORE="$(read_version)"
if [[ -z "$BEFORE" ]]; then
    echo "error: could not read [workspace.package] version from Cargo.toml" >&2
    exit 1
fi

cargo workspaces version "$@" --force '*' --no-git-commit --yes

AFTER="$(read_version)"
if [[ "$AFTER" == "$BEFORE" ]]; then
    echo "error: version is still $BEFORE — nothing was bumped (is HEAD already tagged?)" >&2
    exit 1
fi

# Lockstep pins: a workspace sibling is a dependency spelled with `path = `;
# every such entry that also carries a `version = ` now names $AFTER.
for manifest in Cargo.toml crates/*/Cargo.toml; do
    if grep -Eq '^[^#]*path = "[^"]*"[^#]*version = "' "$manifest"; then
        sed -i -E "/^[^#]*path = \"[^\"]*\"/ s/version = \"[^\"]*\"/version = \"$AFTER\"/" "$manifest"
    fi
done
cargo update --workspace --offline --quiet

echo "==> workspace $BEFORE -> $AFTER"
git --no-pager diff --stat
echo
echo "Next: git commit -am 'chore(release): bump workspace to $AFTER'"
