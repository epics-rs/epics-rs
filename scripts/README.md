# Build and run scripts

Helper scripts that take a fresh clone of `epics-rs` from zero to a running
IOC in three steps. The prerequisites are a Rust toolchain (1.99.1, pinned in
[`rust-toolchain.toml`](../rust-toolchain.toml)) and a C compiler, which
`ring` (pvAccess TLS) and the vendored PCRE in `stream-pcre-sys` build with.

| Script | Purpose |
|--------|---------|
| [`setup.sh`](./setup.sh)   | Install `rustup` if missing and resolve the pinned toolchain (channel + `clippy`/`rustfmt`). |
| [`build.sh`](./build.sh)   | Build the whole workspace in release mode (`cargo build --release --workspace`). |
| [`run-ioc.sh`](./run-ioc.sh) | Build and run one of the bundled example IOCs (defaults to `mini-beamline`). |
| [`pgo-train.sh`](./pgo-train.sh) | Optional; release builds are plain. Train a PGO profile for the Linux x86_64 IOCs `realtime-ca-ioc`, `realtime-pva-ioc` and `softioc-rs`; writes `target/pgo/merged.profdata`. Needs the `llvm-tools` rustup component and a C compiler. |
| [`pgo-build.sh`](./pgo-build.sh) | Release-build those IOCs with that profile; refuses a profile older than the sources. |

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
| [`embedded-image.sh`](./embedded-image.sh) | Build the deployable (`release-embedded`) RTEMS image. |

## Real-hardware IOCs

The drivers for real devices (Intel RealSense D435i, Measurement Computing
USB-CTR08 / USB-2408-2AO) live in the separate `epics-rs-iocs` workspace and
additionally require Linux vendor libraries (`librealsense2-dev`,
`libuldaq`). Those are out of scope for these scripts.
