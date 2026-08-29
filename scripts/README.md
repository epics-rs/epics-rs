# Build and run scripts

Helper scripts that take a fresh clone of `epics-rs` from zero to a running
IOC. Because `epics-rs` is pure Rust with no C dependencies, the entire flow
is three steps and the only prerequisite is a Rust toolchain (pinned in
[`rust-toolchain.toml`](../rust-toolchain.toml)).

| Script | Purpose |
|--------|---------|
| [`setup.sh`](./setup.sh)   | Install `rustup` if missing and resolve the pinned toolchain (channel + `clippy`/`rustfmt`). |
| [`build.sh`](./build.sh)   | Build the whole workspace in release mode (`cargo build --release --workspace`). |
| [`run-ioc.sh`](./run-ioc.sh) | Build and run one of the bundled example IOCs (defaults to `mini-beamline`). |

## Quick start

```bash
git clone https://github.com/epics-rs/epics-rs.git
cd epics-rs

./scripts/setup.sh                 # one-time: toolchain
./scripts/build.sh                 # build the workspace (release)
./scripts/run-ioc.sh               # run the mini-beamline example IOC

# or pick another example:
./scripts/run-ioc.sh --list
./scripts/run-ioc.sh sim-detector
```

After `build.sh`, the bundled command-line tools (`softioc-rs`, `caget-rs`,
`caput-rs`, `camonitor-rs`, `cainfo-rs`) are in `target/release/`. Every PV is
served over both Channel Access and pvAccess, so standard C EPICS clients
(`caget`, `camonitor`, `pvget`) interoperate with the Rust IOCs.

> **Always build with `--release`.** Debug builds run the IOC, CA protocol,
> and areaDetector hot paths ~10-30x slower, which causes CA timeouts and
> dropped monitor updates.

## RTEMS

The RTEMS scripts are documented in
[`crates/epics-rtems-boot/README.md`](../crates/epics-rtems-boot/README.md);
one of them is not optional:

| Script | Purpose |
|--------|---------|
| [`rtems-bsp.sh`](./rtems-bsp.sh) | **Required.** Build the BSP prefix an RTEMS image links against — RSB cross tools, kernel and libbsd from pinned upstream commits, RTEMS 7 by default (`--series 6` for the 6 branch). No release carries the libbsd/kernel fixes the image relies on, so the prefix is a source build and this is its one recorded recipe; source the `epics-rs-env.sh` it writes before any RTEMS build. |
| [`rtems-check.sh`](./rtems-check.sh) | Type-check the RTEMS closure on a machine with no toolchain. |
| [`embedded-image.sh`](./embedded-image.sh) | Build the deployable (`release-embedded`) RTEMS or VxWorks image. |

## Real-hardware IOCs

The drivers for real devices (Intel RealSense D435i, Measurement Computing
USB-CTR08 / USB-2408-2AO) live in the separate `epics-rs-iocs` workspace and
additionally require Linux vendor libraries (`librealsense2-dev`,
`libuldaq`). Those are out of scope for these scripts.
