#!/usr/bin/env bash
#
# rtems-tls-spec.sh - emit the custom RTEMS target spec that carries
# `has-thread-local: true`, the one-key deviation documented in
# `doc/rtems-tls-spec-deviation.md`.
#
# WHY GENERATE RATHER THAN COMMIT A FROZEN JSON
#
# The builtin `armv7-rtems-eabihf` spec is whatever the *active* nightly emits,
# and it changes across nightlies (data-layout, features, metadata). A frozen
# JSON checked into the tree would silently drift from the toolchain a
# developer actually runs, and the first symptom would be a mismatched
# data-layout at codegen — exactly the class of failure the deviation exists to
# avoid. So the spec is DERIVED from the active toolchain every time: dump the
# builtin spec and add the single key. The output tracks the nightly by
# construction; the deviation is one key, never a whole frozen file.
#
# The added key is EXACTLY one — `"has-thread-local": true` — verified against
# the stock print before emitting, so this script cannot silently widen into a
# second deviation.
#
# Usage:
#   ./scripts/rtems-tls-spec.sh            # write the spec to a temp file, print its path
#   ./scripts/rtems-tls-spec.sh -          # write the spec to stdout
#   ./scripts/rtems-tls-spec.sh /path.json # write the spec to /path.json
#
# Requires the nightly toolchain (`-Zunstable-options --print target-spec-json`)
# and `jq`. `RTEMS_TLS_SPEC_RUSTC=/path/to/rustc` derives the spec from that
# exact rustc instead of the rustup `+nightly` shim — the rustc-wrapper path
# (`scripts/rtems-rustc-wrapper.sh`) uses it so the spec always matches the
# toolchain actually compiling.

set -euo pipefail

STOCK_TARGET="armv7-rtems-eabihf"

if [[ -z "${RTEMS_TLS_SPEC_RUSTC:-}" ]] && ! cargo +nightly --version >/dev/null 2>&1; then
    echo "error: the nightly toolchain is required to print the target spec." >&2
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "error: jq is required to inject the single spec key." >&2
    exit 1
fi

if [[ -n "${RTEMS_TLS_SPEC_RUSTC:-}" ]]; then
    stock="$("$RTEMS_TLS_SPEC_RUSTC" -Zunstable-options --print target-spec-json \
        --target "$STOCK_TARGET" 2>/dev/null)" || {
        echo "error: $RTEMS_TLS_SPEC_RUSTC cannot print the target spec" >&2
        echo "(a nightly rustc is required for -Zunstable-options)." >&2
        exit 1
    }
else
    stock="$(rustc +nightly -Zunstable-options --print target-spec-json \
        --target "$STOCK_TARGET" 2>/dev/null)"
fi

# Refuse if the stock spec already sets the key — then the deviation is over and
# this script (and the whole wiring) should be retired, not silently no-op.
if [[ "$(jq -r '."has-thread-local" // "absent"' <<<"$stock")" != "absent" ]]; then
    echo "error: the builtin $STOCK_TARGET spec already sets has-thread-local." >&2
    echo "The deviation is retired upstream — delete this script, revert the" >&2
    echo "rtems-check.sh spec wiring, and remove doc/rtems-tls-spec-deviation.md." >&2
    exit 2
fi

flipped="$(jq '. + {"has-thread-local": true}' <<<"$stock")"

# Prove the injection did exactly one thing: ADD the `has-thread-local` key.
# `has-thread-local` is absent from the stock spec (checked above), so it must
# show up as an added key and NOT as a changed one, and no *existing* key may
# have its value altered.
added="$(jq -cn --argjson a "$stock" --argjson b "$flipped" \
    '(($b|keys) - ($a|keys)) | sort')"
changed="$(jq -cn --argjson a "$stock" --argjson b "$flipped" \
    '[($a|keys[]) as $k | select(($a[$k]) != ($b[$k])) | $k] | sort')"
if [[ "$added" != '["has-thread-local"]' ]] || [[ "$changed" != '[]' ]]; then
    echo "error: the injection did not add exactly the one key." >&2
    echo "  added keys:   $added   (expected [\"has-thread-local\"])" >&2
    echo "  changed keys: $changed   (expected [])" >&2
    exit 3
fi

dest="${1:-}"
if [[ "$dest" == "-" ]]; then
    printf '%s\n' "$flipped"
    exit 0
fi
if [[ -z "$dest" ]]; then
    # cargo derives the target-triple NAME from the spec file's basename, and
    # that name becomes the `target/<name>/` output dir and the
    # `CARGO_TARGET_<NAME>_LINKER` env a real image build needs. So the
    # basename must be a fixed, valid triple stem — never a random `mktemp`
    # name (which would make the linker env unnameable and drop the build onto
    # the host `cc`, failing at `-qrtems`). A unique *directory* keeps
    # concurrent panels from clashing while the basename stays fixed.
    dest="$(mktemp -d "${TMPDIR:-/tmp}/rtems-tls-spec.XXXXXX")/armv7-rtems-eabihf-tls.json"
fi
printf '%s\n' "$flipped" >"$dest"
printf '%s\n' "$dest"
