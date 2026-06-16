#!/usr/bin/env bash
#
# setup.sh - Install and verify the toolchain required to build epics-rs.
#
# epics-rs is pure Rust with no C dependencies (no libca, no libCom), so the
# only prerequisite for the core workspace is a Rust toolchain. The required
# channel and components are pinned in rust-toolchain.toml at the repository
# root, so rustup installs the correct version automatically on the first
# cargo invocation; this script makes that step explicit and verifies it.
#
# Usage:
#   ./scripts/setup.sh
#
# Note: the real-hardware drivers in the companion epics-rs-iocs workspace
# (Intel RealSense D435i, Measurement Computing USB-CTR08 / USB-2408-2AO)
# additionally require Linux vendor libraries (librealsense2-dev, libuldaq).
# Those are NOT needed for this workspace and are not installed here.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

echo "==> epics-rs setup (repository root: $REPO_ROOT)"

# 1) Ensure rustup is present (it manages the pinned toolchain).
if ! command -v rustup >/dev/null 2>&1; then
    echo "==> rustup not found; installing the Rust toolchain manager."
    echo "    (downloads from https://sh.rustup.rs and installs non-interactively)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    # Make cargo/rustup available in the current shell for the rest of this run.
    # shellcheck source=/dev/null
    . "${CARGO_HOME:-$HOME/.cargo}/env"
else
    echo "==> rustup found: $(command -v rustup)"
fi

# 2) Install the toolchain pinned in rust-toolchain.toml (channel + components).
#    'rustup show' resolves and installs the pinned toolchain if it is missing.
echo "==> Resolving the pinned toolchain from rust-toolchain.toml"
rustup show

# 3) Report the active versions so the build environment is reproducible.
echo "==> Toolchain versions:"
echo "    rustup : $(rustup --version 2>/dev/null)"
echo "    rustc  : $(rustc  --version 2>/dev/null)"
echo "    cargo  : $(cargo  --version 2>/dev/null)"

echo "==> Setup complete. Next: ./scripts/build.sh"
