# epics-rtems-boot

The RTEMS boot shim and link contract for epics-rs IOCs. An RTEMS image is a
Rust binary plus a C entry task (`POSIX_Init`) that configures the kernel,
brings up libbsd, and only then calls `main`; this crate owns that C code and
the link flags, once, for every IOC binary in the workspace. On every
non-RTEMS target it is empty and costs a host build nothing.

This README is the build manual for the RTEMS target. The crate-level rustdoc
(`src/lib.rs`) documents the boot/link contract itself.

## Why an RTEMS-named crate has a VxWorks file in it

`src/stats/` is the IOC statistics funnel — descriptor and heap usage for the
status PVs, plus the console census (`rt top` / `rt stackuse` / a descriptor
listing) that a target with no shell has no other way to produce. It is *not*
RTEMS-specific: it has one backend per OS (`rtems.rs`, `vxworks.rs`,
`unsupported.rs`) behind a single set of entry points, so `status_pv` and both
IOC binaries have one uncfg'd call site each.

It lives here rather than in `epics-libcom-rs`, the workspace's os-portability
crate, for one reason that is not a preference: the RTEMS backend's symbols
exist only when *this* package's build script compiled `csrc/rtems_stats.c`,
and it says so by emitting `rtems_boot_linked` — a cfg only the crate it is
emitted for can see. Any other home would have to re-derive "was the C
compiled" from a second copy of the BSP-prefix resolution, which is how the
link contract stops having one source of truth. The boot glue itself stays
RTEMS-only; nothing in `csrc/` is compiled for any other target.

## Prerequisites

| what | needed for | notes |
|---|---|---|
| nightly toolchain + `rust-src` | everything below | `armv7-rtems-eabihf` is tier 3: no prebuilt `std`, so `-Zbuild-std` |
| `jq` | the target-spec generation | see "The target spec" below |
| BSP prefix from `scripts/rtems-bsp.sh` (tools + kernel + libbsd) | linking a bootable image | source its `epics-rs-env.sh`; this crate's `build.rs` derives the compiler and every link flag from `RTEMS_BSP_PREFIX` |

Type-checking needs only the first two rows — no cross-toolchain, no BSP.

## Type-check the RTEMS closure (any dev machine)

```bash
./scripts/rtems-check.sh
```

This is the portability gate: it compiles every RTEMS crate, binary, and both
build configurations (`portability` and `image`) for the target, with no
toolchain present.

## Build the BSP prefix (required)

```bash
./scripts/rtems-bsp.sh                 # RTEMS 7: kernel main + libbsd 7-freebsd-14 -> ~/rtems-bsp/7
./scripts/rtems-bsp.sh --series 6      # RTEMS 6: kernel 6 + libbsd 6-freebsd-14 -> ~/rtems-bsp/6
source ~/rtems-bsp/7/epics-rs-env.sh
```

An image must link against a prefix this script produced. The reason is not
convenience: the libbsd and kernel fixes that make a `kqueue`-registered
socket closable (rtems-libbsd !153/!154/!156/!159, kernel !1383/!1439, merged
2026-07..08) are on every maintained branch tip and in no release — RTEMS 7 is
unreleased and 6.3, the first 6-line release to carry them, is not tagged. The
script builds the RSB cross tools, the kernel and libbsd from pinned commits
into one prefix, asserts those fixes are ancestors of what it built, and
records the revisions in the prefix's `epics-rs-env.sh`. A prefix assembled
by hand matches no upstream tip and cannot be named in a bug report.

Series 7 is the default: a `main` tree reports 7.0.0 in `cpuopts.h`, so the
version-based driver gate the `kqueue` reactor will use reads it as usable
with no override. A 6-branch tree reports 6.0.0 until the 6.3 tag exists, so
the series-6 `epics-rs-env.sh` also exports `EPICS_RTEMS_KQUEUE=1`. The
toolchain target (`arm-rtems6` / `arm-rtems7`) is read off the prefix's
directory tree, by `contract::tool_target_in` in this crate and by
`scripts/rtems-tool-target.sh` in the build scripts; it is never configured.

## Build a bootable image

```bash
source ~/rtems-bsp/7/epics-rs-env.sh

# CA IOC
cargo +nightly build --release --locked \
    -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf \
    -p epics-ca-rs --bin realtime-ca-ioc --no-default-features --features client-core

# PVA (QSRV) IOC
cargo +nightly build --release --locked \
    -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf \
    -p epics-bridge-rs --bin realtime-pva-ioc --no-default-features --features qsrv-core,pvalink
```

The feature selections are deliberate, not defaults — `scripts/rtems-check.sh`
documents why each one is the target's configuration (in short:
`--no-default-features` keeps `ring`/`getrandom` off a target that cannot
compile them, and the named features put the record-link resolvers on the
image).

## The target spec (applied automatically)

Every build of the builtin `armv7-rtems-eabihf` triple in this workspace goes
through the one-key spec deviation `has-thread-local: true`, which takes std's
per-thread TLS leak on RTEMS from 136 B to 0. You do not pass anything: `.cargo/config.toml` wires
`build.rustc-wrapper = scripts/rtems-rustc-wrapper.sh`, which rewrites the
triple to a spec generated from the exact rustc in use and leaves every other
invocation (host builds included) untouched.

Two operational notes:

- `RTEMS_USE_STOCK_SPEC=1` builds against the stock builtin spec instead.
  Toggling it — or first building after the wrapper was introduced, over a
  `target/armv7-rtems-eabihf` dir holding stock-built artifacts — needs
  `rm -rf target/armv7-rtems-eabihf` once; mixed spec artifacts fail loudly
  with E0461, never silently.
- An environment `RUSTC_WRAPPER` (e.g. sccache) overrides the config wiring
  and silently drops the flip; unset it for RTEMS builds, or build through
  `scripts/rtems-check.sh`, whose explicit spec path does not depend on the
  wrapper.
- The wrapper is a bash script, so on Windows cargo cannot execute it and
  every build fails (os error 193). Building the workspace's *host* targets
  from Windows needs `RUSTC_WRAPPER` set to the **empty string** (cmd:
  `set RUSTC_WRAPPER=`), which disables a config-wired wrapper outright;
  RTEMS cross-builds are not possible from Windows in the first place.
