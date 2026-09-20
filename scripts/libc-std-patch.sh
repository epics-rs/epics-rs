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
# A VERSION LABEL IS A CLAIM ABOUT API, so the relabel is only half the job:
# std is compiled against the version the checkout says it is, and names the
# libc fields of that version directly. Where the fork carries a rename that
# no 0.2 release has published yet, the checkout must present the published
# spelling or std fails on the field it cannot find. `STD_STAT_SPELLING`
# below reads which spelling std itself uses and reconciles the checkout to
# it — by NAME only, never by layout, since the pinned content is the one
# that matches the BSP header — so the reconciliation retires on its own
# once the rename ships.
#
# Usage: scripts/libc-std-patch.sh <toolchain>
#   Checkouts are cached under
#   $CARGO_TARGET_DIR/libc-std-patch/<rev>-<version>, keyed on (rev, version,
#   spelling), so a toolchain bump prepares a fresh one instead of relabelling
#   in place — including when it moves the spelling without moving the
#   version, which the directory name cannot show.

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

# The spelling that toolchain's std reads for the newlib `stat` timestamps.
# `os/rtems/fs.rs` names the libc fields in the body of its accessors, so
# std's own source answers it without guessing from a version number.
STD_RTEMS_FS="$SYSROOT/lib/rustlib/src/rust/library/std/src/os/rtems/fs.rs"
if [[ ! -f "$STD_RTEMS_FS" ]]; then
    echo "error: $STD_RTEMS_FS not found - is the rust-src component installed for '$TOOLCHAIN'?" >&2
    exit 1
fi
if grep -q 'as_inner()\.st_atime as' "$STD_RTEMS_FS"; then
    STD_STAT_SPELLING=st_atime
elif grep -q 'as_inner()\.st_atim\.tv_sec' "$STD_RTEMS_FS"; then
    STD_STAT_SPELLING=st_atim
else
    echo "error: $STD_RTEMS_FS reads the stat timestamps in neither spelling this" >&2
    echo "       script knows (st_atime, st_atim.tv_sec); teach it the new one." >&2
    exit 1
fi

DEST="${CARGO_TARGET_DIR:-target}/libc-std-patch/$LIBC_REV-$STD_LIBC_VER"
case "$DEST" in
    /*) : ;;
    *) DEST="$REPO_ROOT/$DEST" ;;
esac

# Give the newlib `stat` timestamps the names std reads WITHOUT moving a
# byte. The BSP header is the authority
# (`$RTEMS_BSP_PREFIX/arm-rtems6/include/sys/stat.h:41-49`): on `__rtems__`
# the three timestamps ARE `struct timespec`, `st_blksize`/`st_blocks`
# follow them, there is no `st_spare4`, and C code reaches the old names
# through `#define st_atime st_atim.tv_sec`. A macro is what Rust cannot
# have, which is the whole of this mismatch: the pinned fork spells the
# fields `st_atim`/`st_mtim`/`st_ctim` (27fe099a, a backport of PR #5132,
# merged and unreleased) and std, compiled against the version this checkout
# is relabelled to, names `st_atime`.
#
# So the rewrite splits each `timespec` into its two members and pads the
# group back to `size_of::<timespec>()`. The padding is what the flat
# published spelling gets wrong and must not be dropped: on a 32-bit target
# `timespec` is `i64 + i32` in 16 bytes, and three flat `time_t + c_long`
# pairs end four bytes early, which walks `st_blksize` and `st_blocks` off
# their offsets. `NEWLIB_TIMESPEC_PAD` derives the width from the types
# rather than the triple, so it is 4 where `c_long` is 32-bit and 0 where it
# is 64-bit. std reads the seconds only — `st_*_nsec()` is hardcoded to 0 —
# so the nsec member keeps its value and just answers to a Rust name.
relabel_newlib_stat() {
    local file="$1"
    cat >> "$file" <<'RS'

// Injected by scripts/libc-std-patch.sh: the tail padding `struct timespec`
// carries wherever `c_long` is narrower than `time_t`. See that script.
const NEWLIB_TIMESPEC_PAD: usize = core::mem::size_of::<crate::timespec>()
    - core::mem::size_of::<crate::time_t>()
    - core::mem::size_of::<c_long>();
RS
    sed -E -i \
        -e 's/^( *)pub st_atim: crate::timespec,$/\1pub st_atime: crate::time_t,\n\1pub st_atime_nsec: c_long,\n\1__pad_st_atim: [u8; NEWLIB_TIMESPEC_PAD],/' \
        -e 's/^( *)pub st_mtim: crate::timespec,$/\1pub st_mtime: crate::time_t,\n\1pub st_mtime_nsec: c_long,\n\1__pad_st_mtim: [u8; NEWLIB_TIMESPEC_PAD],/' \
        -e 's/^( *)pub st_ctim: crate::timespec,$/\1pub st_ctime: crate::time_t,\n\1pub st_ctime_nsec: c_long,\n\1__pad_st_ctim: [u8; NEWLIB_TIMESPEC_PAD],/' \
        "$file"
    if grep -qE '^ *pub st_[amc]tim: crate::timespec,$' "$file"; then
        echo "error: $file still declares the unreleased st_atim/st_mtim/st_ctim" >&2
        echo "       spelling after the rewrite - the struct has moved." >&2
        exit 1
    fi
}

# `.fork-version` and `.stat-spelling` are checked alongside `.ready`: all
# three are written by this script, and a directory carrying one without the
# others is a checkout an older revision of this script prepared — rebuild it
# rather than failing on the missing marker. A spelling that no longer
# matches the toolchain's std rebuilds it too.
if [[ ! -f "$DEST/.ready" || ! -f "$DEST/.fork-version" ||
      "$(cat "$DEST/.stat-spelling" 2>/dev/null)" != "$STD_STAT_SPELLING" ]]; then
    rm -rf "$DEST"
    mkdir -p "$DEST"
    git -C "$DEST" init -q
    git -C "$DEST" fetch -q --depth 1 "$LIBC_GIT" "$LIBC_REV"
    git -C "$DEST" checkout -q FETCH_HEAD
    rm -rf "$DEST/.git"
    sed -n 's/^version = "\(.*\)"/\1/p' "$DEST/Cargo.toml" | head -1 > "$DEST/.fork-version"
    sed -E -i "s/^version = \"[0-9.]+\"/version = \"$STD_LIBC_VER\"/" "$DEST/Cargo.toml"
    if [[ "$STD_STAT_SPELLING" == st_atime ]]; then
        relabel_newlib_stat "$DEST/src/unix/newlib/mod.rs"
    fi
    echo "$STD_STAT_SPELLING" > "$DEST/.stat-spelling"
    touch "$DEST/.ready"
    echo "libc-std-patch: prepared $LIBC_REV as libc $STD_LIBC_VER ($STD_STAT_SPELLING) at $DEST" >&2
fi
FORK_VER=$(cat "$DEST/.fork-version")

if [[ "$STD_LIBC_VER" == "$FORK_VER" ]]; then
    echo "patch.crates-io.libc.path=\"$DEST\""
else
    echo "patch.crates-io.libc-std.package=\"libc\""
    echo "patch.crates-io.libc-std.path=\"$DEST\""
fi
