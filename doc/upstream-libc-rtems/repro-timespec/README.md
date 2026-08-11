# `rtems-timespec-repro`

Standalone reproduction for the `libc` RTEMS `time_t` / `timespec` defect.
Needs an RSB-built `arm-rtems6` toolchain and `qemu-system-arm`. No other
project, no EPICS, no network.

## What it shows

With stock `libc 0.2.188`, on `armv7-rtems-eabihf`:

```
tsprobe: size_of::<libc::timespec>()=8 align=4
tsprobe: size_of::<libc::time_t>()=4 size_of::<libc::timeval>()=8 size_of::<libc::c_long>()=4
tsprobe: size_of::<libc::off_t>()=8 dev_t=4 ino_t=4
tsprobe: std SystemTime[0] secs=567993600 subsec_nanos=0
tsprobe: std SystemTime[1] secs=567993600 subsec_nanos=0
tsprobe: std SystemTime[2] secs=567993600 subsec_nanos=0
tsprobe: std SystemTime[3] secs=567993600 subsec_nanos=0
tsprobe: std SystemTime[4] secs=567993600 subsec_nanos=0
tsprobe: std Instant elapsed secs=0 subsec_nanos=0 (nanos=0)
tsprobe: frame layout size=32 off(before)=0 off(slot)=8 off(after)=16 slot_size=8
tsprobe: clock_gettime rc=0 before=0xdeadbeefcafef00d INTACT after=0xdeadbeef03600fa5 CLOBBERED tail=0xdeadbeefcafef00d INTACT
tsprobe: slot read back as 8-byte struct: tv_sec=567993600 tv_nsec=0
```

Three separate facts in that output:

1. `libc::timespec` is 8 bytes; RTEMS's `struct timespec` is 16.
2. `std`'s `SystemTime` and `Instant` therefore read `tv_nsec` as **0** — the
   five samples are milliseconds apart in real time.
3. `clock_gettime` into an 8-byte slot **clobbers a canary 8 bytes past it**.
   The `before` and `tail` canaries either side are intact, so this is the
   kernel writing the real 16-byte struct, not a stray store.

The canary arm uses its own `#[repr(C)] struct StockTimespec { tv_sec: i32,
tv_nsec: i32 }`, so it demonstrates the overwrite whether or not `libc` is
patched.

## Build

```sh
export RTEMS_BSP_PREFIX=/path/to/rsb/prefix   # the dir containing arm-rtems6/
export RTEMS_BSP=xilinx_zynq_a9_qemu
export PATH="$RTEMS_BSP_PREFIX/bin:$PATH"     # for arm-rtems6-gcc, the linker

cargo +nightly build --release --target armv7-rtems-eabihf \
      -Zbuild-std=std,panic_abort
```

`armv7-rtems-eabihf` is tier 3, so `-Zbuild-std` is required.

## Run

```sh
qemu-system-arm -M xilinx-zynq-a9 -m 256M -no-reboot -nographic \
    -serial null -serial mon:stdio \
    -kernel target/armv7-rtems-eabihf/release/rtems-timespec-repro
```

The first `-serial null` is required: the Zynq BSP's console is UART1, so
without it the output goes to a UART you are not watching.

## Testing a patched `libc`

Point both the crate and the `std` that `-Zbuild-std` compiles at the same
checkout:

```toml
# ~/.cargo/config.toml — or the crate's own .cargo/config.toml
[patch.crates-io]
libc = { path = "/path/to/libc" }
```

and add the identical `[patch.crates-io] libc` line to
`$(rustc --print sysroot)/lib/rustlib/src/rust/library/Cargo.toml`.

> **`-Zbuild-std` does not invalidate `std` when the patched `libc` changes.**
> Run `cargo clean --target armv7-rtems-eabihf` between runs, or you will
> measure the previous build. Symptom: the probe reports an 8-byte `timespec`
> while `SystemTime` reports correct nanoseconds, or the reverse. A correct
> rebuild takes ~45 s; a stale one finishes in ~1 s.

With the fix applied:

```
tsprobe: size_of::<libc::timespec>()=16 align=8
tsprobe: size_of::<libc::off_t>()=8 dev_t=8 ino_t=8
tsprobe: std SystemTime[0] secs=567993600 subsec_nanos=15715519
tsprobe: std SystemTime[4] secs=567993600 subsec_nanos=26538929
tsprobe: std Instant elapsed secs=0 subsec_nanos=25003520 (nanos=25003520)
```

## Files

- `src/main.rs` — the probe.
- `csrc/rtems_config.c` — 30 lines: an RTEMS `POSIX_Init` that calls the Rust
  `main`, plus the minimum `confdefs.h` settings. `CONFIGURE_UNLIMITED_OBJECTS`
  is needed because `std`'s thread-local machinery allocates POSIX keys;
  without it the image dies with `fatal runtime error: out of TLS keys`.
- `build.rs` — compiles that file with `arm-rtems6-gcc` and forwards the ABI
  flags and `-qrtems` to the link line.
- `.cargo/config.toml` — sets the linker to `arm-rtems6-gcc`.

## Environment this was measured on

RTEMS 6.0.0 `2faafecb7f9df8400fd78a1e6d9b3cf3df0eeccc`, RSB
`5dbc1e0855820578661fa4bf8384abc8dda21357`, gcc 13.3.0 20240521, newlib
`1b3dcfd`, BSP `xilinx_zynq_a9_qemu`, rustc `1.99.0-nightly (87e5904f5
2026-07-20)`.
