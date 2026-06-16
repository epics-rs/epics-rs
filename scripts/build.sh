#!/usr/bin/env bash
#
# build.sh - Build the entire epics-rs workspace in release mode.
#
# The whole stack (Channel Access, pvAccess, IOC runtime and 35 base records,
# asyn, motor, areaDetector, QSRV bridge, and the example IOCs) builds with a
# single cargo command. Release mode is mandatory: debug builds run the IOC
# and protocol hot paths roughly 10-30x slower, which causes CA timeouts and
# dropped monitor updates.
#
# Usage:
#   ./scripts/build.sh            # build the default members in release mode
#   ./scripts/build.sh --all      # build every workspace member, including examples
#
# After a successful build, the bundled command-line tools are placed in
# target/release/.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found. Run ./scripts/setup.sh first." >&2
    exit 1
fi

SCOPE="--workspace"
if [[ "${1:-}" == "--all" ]]; then
    # --workspace already covers every member; kept explicit for readability.
    SCOPE="--workspace --all-targets"
fi

echo "==> Building epics-rs (release): cargo build --release ${SCOPE}"
# shellcheck disable=SC2086
cargo build --release ${SCOPE}

TOOLS_DIR="$REPO_ROOT/target/release"
echo "==> Build complete."
echo "==> Bundled CA/PVA command-line tools are in: $TOOLS_DIR"
echo "    softioc-rs, caget-rs, caput-rs, camonitor-rs, cainfo-rs"
echo "    Add them to PATH for this shell with:"
echo "      export PATH=\"$TOOLS_DIR:\$PATH\""
echo "==> Next: ./scripts/run-ioc.sh   (defaults to the mini-beamline example IOC)"
