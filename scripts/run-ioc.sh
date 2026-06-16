#!/usr/bin/env bash
#
# run-ioc.sh - Build (if needed) and run one of the bundled example IOCs.
#
# Each example IOC loads its devices from a startup script (st.cmd) exactly
# like a C EPICS IOC, then drops into an interactive iocsh shell. Every PV is
# served over both Channel Access and pvAccess, so standard clients
# (caget/camonitor, pvget) and the bundled Rust clients both work.
#
# Usage:
#   ./scripts/run-ioc.sh                 # default: mini-beamline
#   ./scripts/run-ioc.sh <example>       # one of the names listed below
#   ./scripts/run-ioc.sh --list          # list available example IOCs
#
# Available examples:
#   mini-beamline    8 motors, 3 point detectors, MovingDot 2D detector, DCM, slit, quad BPM
#   scope-ioc        digital oscilloscope simulator (asyn testAsynPortDriver port)
#   sim-detector     areaDetector simulation detector with the full plugin chain
#   xrt-beamline     real-time X-ray ray-tracing beamline simulation
#   mqtt-ioc         MQTT environment-monitoring IOC
#   modbus-ioc       Modbus device IOC
#   ophyd-test-ioc   IOC used for Bluesky/ophyd integration testing

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# Map example package name -> binary target. The startup script is always
# examples/<package>/ioc/st.cmd, and every example exposes the "ioc" feature.
ioc_bin() {
    case "$1" in
        mini-beamline)  echo "mini_ioc" ;;
        scope-ioc)      echo "scope_ioc" ;;
        sim-detector)   echo "sim_ioc" ;;
        xrt-beamline)   echo "xrt_ioc" ;;
        mqtt-ioc)       echo "mqtt_ioc" ;;
        modbus-ioc)     echo "modbus_ioc" ;;
        ophyd-test-ioc) echo "ophyd_test_ioc" ;;
        *)              echo "" ;;
    esac
}

EXAMPLES="mini-beamline scope-ioc sim-detector xrt-beamline mqtt-ioc modbus-ioc ophyd-test-ioc"

if [[ "${1:-}" == "--list" ]]; then
    echo "Available example IOCs:"
    for e in $EXAMPLES; do echo "  $e"; done
    exit 0
fi

EXAMPLE="${1:-mini-beamline}"
BIN="$(ioc_bin "$EXAMPLE")"
if [[ -z "$BIN" ]]; then
    echo "error: unknown example '$EXAMPLE'. Use --list to see available IOCs." >&2
    exit 1
fi

ST_CMD="examples/${EXAMPLE}/ioc/st.cmd"
if [[ ! -f "$ST_CMD" ]]; then
    echo "error: startup script not found: $ST_CMD" >&2
    exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found. Run ./scripts/setup.sh first." >&2
    exit 1
fi

echo "==> Running $EXAMPLE (release): bin=$BIN, startup=$ST_CMD"
exec cargo run --release -p "$EXAMPLE" --features ioc --bin "$BIN" -- "$ST_CMD"
