#!/usr/bin/env bash
#
# libc-std-patch.sh - prepare the libc checkout that lets `-Zbuild-std` see
# the workspace's pinned libc fixes, and print the `--config` TOML lines
# (one per line) that apply it. Everything else goes to stderr.
#
# THE PROBLEM, measured (isolated experiments on the bring-up box, pristine
# rust-src + fresh CARGO_HOME, 2026-07-26):
#
#   * a MANIFEST `[patch.crates-io]` never reaches the std unit graph:
#     build-std resolves std against rust-src's own `library/Cargo.lock`,
#     which no manifest of ours is part of. Only a CONFIG-level patch does.
#   * a config patch reaches std ONLY at version equality: its package
#     `version` must EQUAL the libc version rust-src's lock pins. A frozen
#     lock can absorb a same-version source swap and nothing else — any
#     other version is silently dropped from the std graph (`warning: patch
#     ... was not used in the crate graph`) and std fails on `killpg`. Git
#     or path source, with or without `--locked`: all dropped alike.
#   * `--locked` cannot ride along at all: ANY config-added patch entry
#     needs a bookkeeping write to the workspace lock, which `--locked`
#     refuses (`error: cannot update the lock file`). Callers run unlocked
#     and snapshot/restore `Cargo.lock` around the invocation instead.
#   * a bare same-key config patch at the relabelled version poisons the
#     WORKSPACE graph: it supersedes the manifest pin, and when the
#     relabelled version no longer satisfies some workspace dependency the
#     resolver falls through to a FLOATING stock crates-io libc (observed:
#     lock rewritten to registry 0.2.189 with `[patch.unused] 0.2.185`).
#
# THE SHAPE THIS PRINTS therefore depends on whether the toolchain's pinned
# version and the fork's version coincide:
#
#   * versions differ (the usual case): an ALIAS patch entry —
#         patch.crates-io.libc-std.package="libc"
#         patch.crates-io.libc-std.path="<checkout relabelled to the pin>"
#     The manifest's own `libc` entry stays in force, so the WORKSPACE graph
#     keeps the committed fork resolution (verified in-lock during
#     measurement), while the std graph — whose requirement only the
#     relabelled version satisfies — takes the alias. Both graphs get the
#     same CONTENT: the manifest pin's exact rev.
#   * versions equal: the alias would duplicate the manifest entry's version
#     (two `[patch]` entries for one version is a cargo error), so print a
#     same-key path patch instead — one source swap serving both graphs at
#     the version both demand.
#
# Either way the workspace manifest stays the single source of truth for
# WHAT libc (URL + rev); the toolchain stays the single source of truth for
# the version label its std graph demands. When a future nightly's std
# starts calling libc API the pinned content does not carry, the target rows
# fail loudly at compile — that is the trip-wire to rebase the fork branch
# and bump the manifest rev.
#
# Usage: scripts/libc-std-patch.sh <toolchain>
#   Checkouts are cached under
#   $CARGO_TARGET_DIR/libc-std-patch/<rev>-<version>, one per (rev, version)
#   pair, so a toolchain bump prepares a fresh one instead of relabelling in
#   place.

set -euo pipefail

TOOLCHAIN="${1:?usage: scripts/libc-std-patch.sh <toolchain>}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# The manifest pin: the single place the URL and rev are written.
LIBC_PIN_LINE=$(grep -E '^libc *= *\{.*git *=' Cargo.toml || true)
LIBC_GIT=$(sed -n 's/.*git *= *"\([^"]*\)".*/\1/p' <<<"$LIBC_PIN_LINE")
LIBC_REV=$(sed -n 's/.*rev *= *"\([^"]*\)".*/\1/p' <<<"$LIBC_PIN_LINE")
if [[ -z "$LIBC_GIT" || -z "$LIBC_REV" ]]; then
    echo "error: no '[patch.crates-io] libc = { git = ..., rev = ... }' pin in Cargo.toml;" >&2
    echo "       nothing to derive the std libc patch from." >&2
    exit 1
fi

# The version the toolchain's own std graph demands.
SYSROOT=$(rustc "+$TOOLCHAIN" --print sysroot)
STD_LOCK="$SYSROOT/lib/rustlib/src/rust/library/Cargo.lock"
if [[ ! -f "$STD_LOCK" ]]; then
    echo "error: $STD_LOCK not found - is the rust-src component installed for '$TOOLCHAIN'?" >&2
    exit 1
fi
STD_LIBC_VER=$(awk '/^name = "libc"$/ { getline; sub(/^version = "/, ""); sub(/"$/, ""); print; exit }' "$STD_LOCK")
if [[ -z "$STD_LIBC_VER" ]]; then
    echo "error: no libc entry found in $STD_LOCK" >&2
    exit 1
fi

DEST="${CARGO_TARGET_DIR:-target}/libc-std-patch/$LIBC_REV-$STD_LIBC_VER"
case "$DEST" in
    /*) : ;;
    *) DEST="$REPO_ROOT/$DEST" ;;
esac

# `.fork-version` is checked alongside `.ready`: both are written by this
# script, and a directory carrying one without the other is a checkout an
# older revision of this script prepared — rebuild it rather than failing
# on the missing marker.
if [[ ! -f "$DEST/.ready" || ! -f "$DEST/.fork-version" ]]; then
    rm -rf "$DEST"
    mkdir -p "$DEST"
    git -C "$DEST" init -q
    git -C "$DEST" fetch -q --depth 1 "$LIBC_GIT" "$LIBC_REV"
    git -C "$DEST" checkout -q FETCH_HEAD
    rm -rf "$DEST/.git"
    sed -n 's/^version = "\(.*\)"/\1/p' "$DEST/Cargo.toml" | head -1 > "$DEST/.fork-version"
    sed -E -i "s/^version = \"[0-9.]+\"/version = \"$STD_LIBC_VER\"/" "$DEST/Cargo.toml"
    touch "$DEST/.ready"
    echo "libc-std-patch: prepared $LIBC_REV as libc $STD_LIBC_VER at $DEST" >&2
fi
FORK_VER=$(cat "$DEST/.fork-version")

if [[ "$STD_LIBC_VER" == "$FORK_VER" ]]; then
    echo "patch.crates-io.libc.path=\"$DEST\""
else
    echo "patch.crates-io.libc-std.package=\"libc\""
    echo "patch.crates-io.libc-std.path=\"$DEST\""
fi
