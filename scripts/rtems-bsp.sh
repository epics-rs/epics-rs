#!/usr/bin/env bash
#
# rtems-bsp.sh - build the RTEMS toolchain, kernel and libbsd that an epics-rs
# image links against, from PINNED upstream revisions, into one prefix.
#
#   scripts/rtems-bsp.sh                       # series 7 into ~/rtems-bsp/7
#   scripts/rtems-bsp.sh --series 6            # series 6 into ~/rtems-bsp/6
#   scripts/rtems-bsp.sh --prefix /opt/rtems/7 --jobs 8
#   source ~/rtems-bsp/7/epics-rs-env.sh && scripts/embedded-image.sh rtems pva
#
# WHY THIS SCRIPT EXISTS
#
# No RTEMS release carries the fixes that make a kqueue-registered socket
# closable on libbsd (rtems-libbsd !153/!154/!156/!159, kernel !1381/!1383/
# !1439, merged 2026-07..2026-08). They are on every maintained branch tip and
# in no tarball: RTEMS 7 is unreleased, and the first 6-line release to carry
# them is 6.3, which is not tagged. Until then the BSP an image links against
# is a source build, and a source build assembled by hand is not evidence of
# anything - the bring-up box's original prefix was a May-2026 kernel with a
# libbsd.a copied in from a merge-request branch, matching no upstream tip.
# This script is the one recorded way to produce the prefix, so that an image
# is linked against revisions that can be named, and so that the two series
# are built the same way.
#
# WHAT IT PRODUCES
#
# One prefix, laid out as the link contract expects
# (crates/epics-rtems-boot/src/contract.rs):
#
#   <prefix>/bin/arm-rtems<N>-gcc                     RSB cross tools
#   <prefix>/arm-rtems<N>/<bsp>/lib/{librtemsbsp.a,linkcmds,...}   kernel
#   <prefix>/arm-rtems<N>/<bsp>/lib/libbsd.a          libbsd
#   <prefix>/arm-rtems<N>/<bsp>/lib/include/...       BSP + libbsd headers
#   <prefix>/epics-rs-env.sh                          the operator's environment
#
# `epics-rs-env.sh` exports RTEMS_BSP_PREFIX, RTEMS_BSP, PATH and the cargo
# linker override, and records the revisions the prefix was built from.
#
# VERSION AND THE KQUEUE GATE
#
# A branch build reports the series' development version in cpuopts.h:
# 7.0.0 on main, 6.0.0 on the 6 branch (the release tooling writes the real
# minor into a tarball's VERSION file; git trees carry none). The PVA driver
# selection in epics-rtems-boot reads that version with pvxs's rule
# "kqueue from 6.3" - true for 7.0.0, false for 6.0.0. So a series-6 prefix,
# although this script verified the fixes are in its tree, would be gated to
# the blocking driver by its version alone; epics-rs-env.sh therefore exports
# EPICS_RTEMS_KQUEUE=1 for series 6, which is the override that gate takes.
# Series 7 needs no override.
#
# PINS
#
# Every source is checked out at a pinned commit (`--tip` moves each to its
# branch tip instead), and the required fix commits are asserted to be
# ancestors of whatever is checked out, so a moved pin cannot silently drop
# them. The kernel pin is what lets the libbsd tip LINK: since !156/!159
# libbsd registers pipe handlers through cpukit hooks that only a kernel
# carrying !1383 (main) / !1439 (6) declares.
#
# MEASURED PITFALLS THIS SCRIPT ENCODES
#
#   * libbsd's `waf install` is not parallel-safe (-j12 dies mid-copy);
#     install runs -j1. doc/rtems-qemu-bringup-artefacts.md.
#   * RTEMS_POSIX_API defaults to false on both branches; without it libbsd's
#     openssl apps fail to link (signal/alarm live in cpukit/posix) and an
#     epics-rs image has no POSIX_Init. It is set in the generated config.ini.
#   * rtems_waf does not track installed kernel headers: an incremental libbsd
#     build over a changed kernel option links stale confdefs objects and
#     faults in _POSIX_Threads_Create_extension. Both waf trees are built
#     clean (build/ removed) every run.
#
# PREREQUISITES: git, python3, and the RSB host packages (`sb-check` lists
# what is missing). 12 cores build the series-7 tools in ~1h20m, the kernel in
# under a minute and libbsd in ~4 min (measured on the bring-up box).

set -euo pipefail

usage() {
    sed -n '2,10p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
    echo >&2
    echo "options:" >&2
    echo "  --series 7|6      RTEMS series (default 7: kernel main, libbsd 7-freebsd-14)" >&2
    echo "  --prefix DIR      install prefix (default \$HOME/rtems-bsp/<series>)" >&2
    echo "  --src DIR         source checkouts (default <prefix>-src)" >&2
    echo "  --bsp ARCH/BSP    BSP to build (default arm/xilinx_zynq_a9_qemu)" >&2
    echo "  --jobs N          parallel jobs (default nproc)" >&2
    echo "  --tip             build the branch tips instead of the pinned commits" >&2
    echo "  --rebuild-tools   run the RSB even if <prefix>/bin/arm-rtems<N>-gcc exists" >&2
    exit 2
}

SERIES=7
PREFIX=""
SRC=""
BSP="arm/xilinx_zynq_a9_qemu"
JOBS="$(nproc)"
TIP=0
REBUILD_TOOLS=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --series) SERIES="${2:-}"; shift 2 ;;
        --prefix) PREFIX="${2:-}"; shift 2 ;;
        --src) SRC="${2:-}"; shift 2 ;;
        --bsp) BSP="${2:-}"; shift 2 ;;
        --jobs) JOBS="${2:-}"; shift 2 ;;
        --tip) TIP=1; shift ;;
        --rebuild-tools) REBUILD_TOOLS=1; shift ;;
        -h|--help) usage ;;
        *) echo "rtems-bsp: unknown argument: $1" >&2; usage ;;
    esac
done

GITLAB=https://gitlab.rtems.org/rtems

# The pins. Branch tips as of 2026-08-25 (`git ls-remote`), chosen because
# every fix commit listed under REQUIRED_* had merged by then; a `--tip` run
# still asserts the REQUIRED_* ancestry.
case "$SERIES" in
    7)
        TOOL_TARGET=arm-rtems7
        RSB_BRANCH=main;          RSB_PIN=f34884368d43f61fc3a478cad67f32e1d1371434
        KERNEL_BRANCH=main;       KERNEL_PIN=181e86a199551f30cb0b62929b786ae0b24cae01
        LIBBSD_BRANCH=7-freebsd-14; LIBBSD_PIN=fdd9fa734c4b32b4dd86e053878328dcfdfec735
        # kernel !1383: the pipe/pipe2 hooks libbsd registers through.
        REQUIRED_KERNEL=(ec42cadd3b4deab0fe784148aae42cdd89634912)
        # libbsd !153 (descriptor identity + kqueue hold release, 4 commits)
        # and !156 (pipe handler registration).
        REQUIRED_LIBBSD=(
            c86cbc579e3587faac717b540be2fe3d82a9b48b
            5a7a48f8be452cd5d95c6824e7853b73ef0caadd
            6e65c770fb9612d99959b6ed1a0220db4999ab9b
            9b5b3e50c7a231bfad1f3bdce9aadfd81d8b2320
            ef5b94a4420c961807a243df0c84bd57120c85a3
        )
        KQUEUE_OVERRIDE=""
        ;;
    6)
        TOOL_TARGET=arm-rtems6
        RSB_BRANCH=6;             RSB_PIN=06460f631ff4b71dfd7e1f83c64b4628d5f6140d
        KERNEL_BRANCH=6;          KERNEL_PIN=51f962fb3fd8e1eae4249d04f3821e7630eda774
        LIBBSD_BRANCH=6-freebsd-14; LIBBSD_PIN=6d027f0bb482a36706639aa74549f1afd2c39ecb
        # kernel !1439: the !1383 hooks backported to 6.
        REQUIRED_KERNEL=(51f962fb3fd8e1eae4249d04f3821e7630eda774)
        # libbsd !154 (the !153 series on 6-freebsd-14) and !159 (the !156
        # registration on 6-freebsd-14).
        REQUIRED_LIBBSD=(
            08d8e275ebe246257ad2060a43560a6b918f3277
            36880e4d8fa220255fa3df125d6b0f4669ff222f
            1d06fe5a62fa560f80a9fa7498f1398686b20847
            d8a187bc2e54deb31e319e63d6e12ec531fed209
            6d027f0bb482a36706639aa74549f1afd2c39ecb
        )
        KQUEUE_OVERRIDE=1
        ;;
    *) echo "rtems-bsp: --series takes 7 or 6, not '$SERIES'" >&2; exit 2 ;;
esac

[[ "$BSP" == */* ]] || { echo "rtems-bsp: --bsp takes ARCH/BSP, e.g. arm/xilinx_zynq_a9_qemu" >&2; exit 2; }
BSP_ARCH="${BSP%%/*}"
BSP_NAME="${BSP#*/}"
[[ "$BSP_ARCH" == arm ]] || { echo "rtems-bsp: only arm BSPs: the epics-rs target is armv7-rtems-eabihf" >&2; exit 2; }

PREFIX="${PREFIX:-$HOME/rtems-bsp/$SERIES}"
SRC="${SRC:-${PREFIX}-src}"
# Absolute: the RSB and both waf trees are driven from other directories.
mkdir -p "$PREFIX" "$SRC"
PREFIX="$(cd "$PREFIX" && pwd)"
SRC="$(cd "$SRC" && pwd)"
LOGS="$SRC/logs"
mkdir -p "$LOGS"
export PATH="$PREFIX/bin:$PATH"

for tool in git python3; do
    command -v "$tool" >/dev/null 2>&1 || { echo "rtems-bsp: $tool is required" >&2; exit 1; }
done

STEP_T0=0
step() {
    STEP_T0=$SECONDS
    echo "== rtems-bsp: $*" >&2
}
done_step() {
    echo "   ($(( SECONDS - STEP_T0 )) s)" >&2
}

# checkout <dir> <repo-path> <branch> <pin>: clone or fetch, then detach at the
# pin (or the fetched branch tip under --tip). Prints the commit checked out.
checkout() {
    local dir="$1" repo="$2" branch="$3" pin="$4" want
    if [[ ! -d "$dir/.git" ]]; then
        git clone --quiet --branch "$branch" "$GITLAB/$repo.git" "$dir"
    fi
    git -C "$dir" fetch --quiet origin "$branch"
    if [[ "$TIP" == 1 ]]; then
        want=FETCH_HEAD
    else
        want="$pin"
        # A pin that is not on the fetched branch is a typo or a rewritten
        # branch; either way it is not "the branch at this commit".
        git -C "$dir" merge-base --is-ancestor "$pin" FETCH_HEAD 2>/dev/null \
            || { echo "rtems-bsp: $repo pin $pin is not on origin/$branch" >&2; exit 1; }
    fi
    git -C "$dir" checkout --quiet --detach "$want"
    git -C "$dir" rev-parse HEAD
}

# require <dir> <what> <sha>...: every listed commit must be an ancestor of HEAD.
require() {
    local dir="$1" what="$2" sha; shift 2
    for sha in "$@"; do
        git -C "$dir" merge-base --is-ancestor "$sha" HEAD 2>/dev/null || {
            echo "rtems-bsp: $what at $(git -C "$dir" rev-parse --short HEAD) does not contain required fix $sha" >&2
            echo "  $(git -C "$dir" log -1 --format='%h %s' "$sha" 2>/dev/null || echo "(commit not in this clone)")" >&2
            exit 1
        }
    done
}

# ---- 1. sources -------------------------------------------------------------
step "sources (series $SERIES, $( [[ $TIP == 1 ]] && echo branch tips || echo pinned)) -> $SRC"
RSB_REV=$(checkout "$SRC/rsb" tools/rtems-source-builder "$RSB_BRANCH" "$RSB_PIN")
KERNEL_REV=$(checkout "$SRC/kernel" rtos/rtems "$KERNEL_BRANCH" "$KERNEL_PIN")
LIBBSD_REV=$(checkout "$SRC/libbsd" pkg/rtems-libbsd "$LIBBSD_BRANCH" "$LIBBSD_PIN")
git -C "$SRC/libbsd" submodule --quiet update --init rtems_waf
require "$SRC/kernel" "kernel $KERNEL_BRANCH" "${REQUIRED_KERNEL[@]}"
require "$SRC/libbsd" "libbsd $LIBBSD_BRANCH" "${REQUIRED_LIBBSD[@]}"
echo "   rsb    $RSB_BRANCH @ $RSB_REV" >&2
echo "   kernel $KERNEL_BRANCH @ $KERNEL_REV" >&2
echo "   libbsd $LIBBSD_BRANCH @ $LIBBSD_REV" >&2
done_step

# ---- 2. cross tools (RSB) ---------------------------------------------------
if [[ "$REBUILD_TOOLS" == 1 || ! -x "$PREFIX/bin/$TOOL_TARGET-gcc" ]]; then
    step "tools: $SERIES/rtems-arm via RSB -> $PREFIX (log $LOGS/rsb.log)"
    ( cd "$SRC/rsb/rtems" && ../source-builder/sb-check ) >"$LOGS/sb-check.log" 2>&1 || {
        cat "$LOGS/sb-check.log" >&2
        echo "rtems-bsp: sb-check failed; install the host packages it names" >&2
        exit 1
    }
    ( cd "$SRC/rsb/rtems" && ../source-builder/sb-set-builder \
        --prefix="$PREFIX" --log="$LOGS/rsb.log" --jobs="$JOBS" "$SERIES/rtems-arm" ) >"$LOGS/rsb.out" 2>&1 || {
        tail -40 "$LOGS/rsb.out" >&2
        echo "rtems-bsp: RSB failed; full log $LOGS/rsb.log" >&2
        exit 1
    }
    [[ -x "$PREFIX/bin/$TOOL_TARGET-gcc" ]] || { echo "rtems-bsp: RSB finished but $PREFIX/bin/$TOOL_TARGET-gcc is missing" >&2; exit 1; }
    done_step
else
    echo "== rtems-bsp: tools: $PREFIX/bin/$TOOL_TARGET-gcc present, RSB skipped (--rebuild-tools forces it)" >&2
fi
"$PREFIX/bin/$TOOL_TARGET-gcc" --version | head -1 | sed 's/^/   /' >&2

# ---- 3. kernel --------------------------------------------------------------
step "kernel: $BSP -> $PREFIX (log $LOGS/kernel.log)"
KERNEL_CFG="$SRC/kernel-config.ini"
cat >"$KERNEL_CFG" <<INI
[$BSP]
RTEMS_POSIX_API = True
BUILD_SAMPLES = True
BUILD_TESTS = False
INI
(
    cd "$SRC/kernel"
    rm -rf build
    python3 ./waf configure --prefix="$PREFIX" --rtems-tools="$PREFIX" --rtems-config="$KERNEL_CFG"
    python3 ./waf -j"$JOBS"
    python3 ./waf install
) >"$LOGS/kernel.log" 2>&1 || { tail -40 "$LOGS/kernel.log" >&2; echo "rtems-bsp: kernel build failed; log $LOGS/kernel.log" >&2; exit 1; }
BSP_LIB="$PREFIX/$TOOL_TARGET/$BSP_NAME/lib"
[[ -f "$BSP_LIB/librtemsbsp.a" && -f "$BSP_LIB/linkcmds" ]] || { echo "rtems-bsp: kernel installed nothing under $BSP_LIB" >&2; exit 1; }
CPUOPTS="$BSP_LIB/include/rtems/score/cpuopts.h"
RTEMS_VERSION="$(awk '/#define __RTEMS_(MAJOR|MINOR|REVISION)__/ {printf "%s.", $3}' "$CPUOPTS" | sed 's/\.$//')"
echo "   cpuopts.h reports $RTEMS_VERSION" >&2
done_step

# ---- 4. libbsd --------------------------------------------------------------
step "libbsd: buildset/default.ini for $BSP (log $LOGS/libbsd.log)"
(
    cd "$SRC/libbsd"
    rm -rf build
    python3 ./waf configure --prefix="$PREFIX" --rtems="$PREFIX" --rtems-tools="$PREFIX" \
        --rtems-bsps="$BSP" --buildset=buildset/default.ini
    python3 ./waf -j"$JOBS"
    python3 ./waf install -j1
) >"$LOGS/libbsd.log" 2>&1 || { tail -40 "$LOGS/libbsd.log" >&2; echo "rtems-bsp: libbsd build failed; log $LOGS/libbsd.log" >&2; exit 1; }
[[ -f "$BSP_LIB/libbsd.a" && -f "$BSP_LIB/include/machine/rtems-bsd-config.h" ]] \
    || { echo "rtems-bsp: libbsd installed nothing under $BSP_LIB" >&2; exit 1; }
done_step

# ---- 5. the operator's environment ------------------------------------------
ENV_FILE="$PREFIX/epics-rs-env.sh"
{
    echo "# Generated by epics-rs scripts/rtems-bsp.sh; source it before building an RTEMS image."
    echo "#   series $SERIES, BSP $BSP, cpuopts.h $RTEMS_VERSION"
    echo "#   rsb    $RSB_BRANCH @ $RSB_REV"
    echo "#   kernel $KERNEL_BRANCH @ $KERNEL_REV"
    echo "#   libbsd $LIBBSD_BRANCH @ $LIBBSD_REV"
    echo "export RTEMS_BSP_PREFIX=\"$PREFIX\""
    echo "export RTEMS_BSP=\"$BSP_NAME\""
    echo "export PATH=\"$PREFIX/bin:\$PATH\""
    echo "# .cargo/config.toml names arm-rtems6-gcc; the environment wins."
    echo "export CARGO_TARGET_ARMV7_RTEMS_EABIHF_LINKER=$TOOL_TARGET-gcc"
    if [[ -n "$KQUEUE_OVERRIDE" ]]; then
        echo "# A 6-branch tree reports $RTEMS_VERSION, below the 6.3 the kqueue gate"
        echo "# needs; this prefix carries the fixes (asserted at build), so say so."
        echo "export EPICS_RTEMS_KQUEUE=$KQUEUE_OVERRIDE"
    fi
} >"$ENV_FILE"

echo "== rtems-bsp: done. $PREFIX ($TOOL_TARGET, RTEMS $RTEMS_VERSION)" >&2
echo "   source $ENV_FILE" >&2
