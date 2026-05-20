#!/usr/bin/env bash
# Build the pvxs golden-capture harness.
#
# Requires:
#   - pvxs built at $PVXS_TOP (default: ~/codes/pvxs)
#   - EPICS base built at $EPICS_BASE (default: ~/epics/epics-base)
#
# The harness includes pvxs **private** headers (dataimpl.h,
# pvaproto.h) — must point at pvxs's `src/` not just `include/`.

set -euo pipefail

PVXS_TOP="${PVXS_TOP:-$HOME/codes/pvxs}"
EPICS_BASE="${EPICS_BASE:-$HOME/epics/epics-base}"
EPICS_HOST_ARCH="${EPICS_HOST_ARCH:-darwin-aarch64}"

cd "$(dirname "$0")"

clang++ -std=c++17 -O2 -Wall \
    -I "$PVXS_TOP/include" \
    -I "$PVXS_TOP/src" \
    -I "$EPICS_BASE/include" \
    -I "$EPICS_BASE/include/os/Darwin" \
    -I "$EPICS_BASE/include/compiler/clang" \
    -I "$(brew --prefix libevent 2>/dev/null || echo /opt/homebrew/opt/libevent)/include" \
    -L "$PVXS_TOP/lib/$EPICS_HOST_ARCH" -lpvxs \
    -L "$EPICS_BASE/lib/$EPICS_HOST_ARCH" -lCom \
    -Wl,-rpath,"$PVXS_TOP/lib/$EPICS_HOST_ARCH" \
    -Wl,-rpath,"$EPICS_BASE/lib/$EPICS_HOST_ARCH" \
    capture.cpp -o capture

echo "built: ./capture"
