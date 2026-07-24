#!/usr/bin/env bash
#
# rtems-rustc-wrapper.sh - cargo `build.rustc-wrapper` (wired in
# `.cargo/config.toml`) that reroutes any invocation selecting the *builtin*
# `armv7-rtems-eabihf` triple through the generated has-thread-local spec, so
# a plain `cargo +nightly build --target armv7-rtems-eabihf` picks up the
# one-key deviation (`doc/rtems-tls-spec-deviation.md`) without going through
# `rtems-check.sh` or the box build scripts.
#
# Every other invocation — host builds, and the explicit `-tls.json` spec
# paths the gate and box scripts pass — execs straight through untouched, so
# the wrapper is inert everywhere the flip does not apply.
#
# The spec itself still has exactly one owner: `scripts/rtems-tls-spec.sh`
# (injection + the upstream-flip trip-wire live there, and a trip-wire refusal
# fails the build loudly here too). It is generated from the *wrapped* rustc
# (`RTEMS_TLS_SPEC_RUSTC`), cached per rustc under `target/rtems-tls-spec/`,
# and named `armv7-rtems-eabihf.json` so the rustc-side target name stays the
# builtin one cargo already used for `target/<name>/` and the linker table.
#
# RTEMS_USE_STOCK_SPEC=1 disables the rewrite — the same escape hatch as
# `rtems-check.sh` and the box build scripts.

set -euo pipefail

STOCK_TARGET="armv7-rtems-eabihf"

if [[ "${RTEMS_USE_STOCK_SPEC:-0}" == "1" ]]; then
    exec "$@"
fi

# Find `--target armv7-rtems-eabihf` (split or `=` form). `hit` indexes the
# argv element to rewrite: the value for the split form, the whole `--target=`
# element for the `=` form.
args=("$@")
hit=-1
split=0
for i in "${!args[@]}"; do
    if [[ "${args[$i]}" == "--target" ]]; then
        j=$((i + 1))
        if [[ $j -lt ${#args[@]} && "${args[$j]}" == "$STOCK_TARGET" ]]; then
            hit=$j
            split=1
        fi
    elif [[ "${args[$i]}" == "--target=$STOCK_TARGET" ]]; then
        hit=$i
        split=0
    fi
done
if [[ $hit -lt 0 ]]; then
    exec "$@"
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# The real rustc in the argv: cargo may chain a workspace wrapper (e.g.
# clippy-driver) ahead of it, so take the first element whose basename is
# rustc, falling back to the first argument.
rustc_bin="${args[0]}"
for a in "${args[@]}"; do
    case "${a##*/}" in
    rustc | rustc.exe)
        rustc_bin="$a"
        break
        ;;
    esac
done

# Cache one generated spec per exact rustc (keyed by `-vV`, which pins both
# version and host), regenerating only when the toolchain changes. Concurrent
# rustc processes may race the generation; they produce identical content and
# the final `mv` is atomic, so the race is harmless.
key="$("$rustc_bin" -vV | sha256sum | cut -c1-16)"
spec_dir="$root/target/rtems-tls-spec/$key"
spec="$spec_dir/$STOCK_TARGET.json"
if [[ ! -f "$spec" ]]; then
    mkdir -p "$spec_dir"
    tmp="$(mktemp "$spec_dir/.gen.XXXXXX")"
    trap '[[ -f "$tmp" ]] && rm -f "$tmp"' EXIT
    RTEMS_TLS_SPEC_RUSTC="$rustc_bin" "$root/scripts/rtems-tls-spec.sh" "$tmp" >/dev/null
    mv -f "$tmp" "$spec"
    trap - EXIT
fi

if [[ $split -eq 1 ]]; then
    args[$hit]="$spec"
else
    args[$hit]="--target=$spec"
fi
# Loading a JSON target spec is itself gated behind `-Zunstable-options` on
# current nightlies; appending it twice is harmless when cargo already passed
# it, and a stable rustc could not build this tier-3 target anyway.
exec "${args[@]}" -Zunstable-options
