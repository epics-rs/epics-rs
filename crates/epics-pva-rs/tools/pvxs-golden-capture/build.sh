#!/usr/bin/env bash
# Build the pvxs golden-capture harness.
#
# The two trees come from the environment — PVXS_HOME and EPICS_BASE, the
# same names the parity gate exports (PVXS_TOP is still accepted, it was
# this script's original name for the first one). Host architecture and
# compiler are derived from `uname` unless EPICS_HOST_ARCH / CXX say
# otherwise, so a Linux box gets linux-<machine> + g++ and a mac gets
# darwin-<machine> + clang++.
#
# Every input is checked before the compiler runs and a missing one ends
# the script: an inconsistent tree used to link and then emit nothing,
# which silently rewrites fixtures.txt to an empty file.
#
# The harness includes pvxs **private** headers (bitmask.h, dataimpl.h,
# pvaproto.h) — must point at pvxs's `src/` not just `include/`.

set -euo pipefail

die() { printf 'build.sh: %s\n' "$*" >&2; exit 1; }
need() { [ -e "$2" ] || die "$1 not found: $2"; }

PVXS_HOME="${PVXS_HOME:-${PVXS_TOP:-}}"
[ -n "$PVXS_HOME" ] || die "neither PVXS_HOME nor PVXS_TOP is set — point one at a built pvxs checkout"
[ -n "${EPICS_BASE:-}" ] || die "EPICS_BASE is not set — point it at a built EPICS base"

case "$(uname -s)" in
Darwin)
    case "$(uname -m)" in
    arm64 | aarch64) default_arch=darwin-aarch64 ;;
    *) default_arch=darwin-x86 ;;
    esac
    default_cxx=clang++
    osd=Darwin
    ;;
Linux)
    default_arch="linux-$(uname -m)"
    default_cxx=g++
    osd=Linux
    ;;
*)
    die "unsupported host $(uname -s) — set EPICS_HOST_ARCH and CXX by hand"
    ;;
esac

EPICS_HOST_ARCH="${EPICS_HOST_ARCH:-$default_arch}"
CXX="${CXX:-$default_cxx}"
command -v "$CXX" >/dev/null 2>&1 || die "compiler not on PATH: $CXX"

# base ships one include dir per compiler family; pick it from $CXX.
case "$(basename "$CXX")" in
*clang*) compiler_dir=clang ;;
*) compiler_dir=gcc ;;
esac

cd "$(dirname "$0")"

# Before anything can fail: a binary built against some other tree must not
# survive this run. The workflow pipes ./capture straight into fixtures.txt,
# so a stale one left behind by a failed build silently republishes the old
# tree's bytes as the new goldens.
rm -f capture

need "pvxs public headers" "$PVXS_HOME/include/pvxs/data.h"
need "pvxs private headers" "$PVXS_HOME/src/dataimpl.h"
need "pvxs private headers" "$PVXS_HOME/src/pvaproto.h"
need "libpvxs ($EPICS_HOST_ARCH)" "$PVXS_HOME/lib/$EPICS_HOST_ARCH"
need "base OSD headers" "$EPICS_BASE/include/os/$osd"
need "base compiler headers" "$EPICS_BASE/include/compiler/$compiler_dir"
need "libCom ($EPICS_HOST_ARCH)" "$EPICS_BASE/lib/$EPICS_HOST_ARCH"


# libevent: pvxs bundles one per arch, which is the copy libpvxs was
# linked against. Fall back to Homebrew, then to the system headers.
libevent_flags=()
libevent_lib=""
for root in "$PVXS_HOME/bundle/usr/$EPICS_HOST_ARCH" \
    "$(brew --prefix libevent 2>/dev/null || true)" \
    /opt/homebrew/opt/libevent /usr/local/opt/libevent; do
    if [ -n "$root" ] && [ -f "$root/include/event2/event.h" ]; then
        libevent_lib="$root/lib"
        libevent_flags=("-I$root/include" "-L$root/lib")
        break
    fi
done
if [ -z "$libevent_lib" ] && [ ! -f /usr/include/event2/event.h ]; then
    die "no libevent headers — looked in \$PVXS_HOME/bundle/usr/$EPICS_HOST_ARCH, Homebrew and /usr/include"
fi

# every element is one quoted word: -Wl,-rpath,DIR is comma-delimited by
# the linker, not by the shell.
rpath=("-Wl,-rpath,$PVXS_HOME/lib/$EPICS_HOST_ARCH" "-Wl,-rpath,$EPICS_BASE/lib/$EPICS_HOST_ARCH")
[ -n "$libevent_lib" ] && rpath+=("-Wl,-rpath,$libevent_lib")

# GNU ld resolves left to right, so the -l flags must follow capture.cpp;
# Apple's ld does not care, which is why the original order only ever
# worked on a mac.
"$CXX" -std=c++17 -O2 -Wall \
    -I "$PVXS_HOME/include" \
    -I "$PVXS_HOME/src" \
    -I "$EPICS_BASE/include" \
    -I "$EPICS_BASE/include/os/$osd" \
    -I "$EPICS_BASE/include/compiler/$compiler_dir" \
    ${libevent_flags[@]+"${libevent_flags[@]}"} \
    capture.cpp -o capture \
    -L "$PVXS_HOME/lib/$EPICS_HOST_ARCH" -lpvxs \
    -L "$EPICS_BASE/lib/$EPICS_HOST_ARCH" -lCom \
    "${rpath[@]}"

# An empty capture is the failure this script exists to make loud: the
# workflow pipes ./capture straight into fixtures.txt.
out=$(./capture) || die "./capture built but exited non-zero"
lines=$(printf '%s\n' "$out" | grep -c '=' || true)
[ "$lines" -gt 0 ] || die "./capture built but emitted no fixtures — the pvxs tree at $PVXS_HOME is not the one the harness expects"

echo "built: ./capture ($CXX, $EPICS_HOST_ARCH, $lines fixtures)"
