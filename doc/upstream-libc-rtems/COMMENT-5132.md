# Comment to post on rust-lang/libc#5132

Measured RTEMS data below, but first a problem with the module restructure that
I think blocks this as written.

## The `cfg_if` merge makes `mod rtems` unreachable

On `main` today there are **two** `cfg_if` blocks at the end of
`src/unix/newlib/mod.rs`: one selecting the arch module (`mod arm`), and a
separate one that glob-imports `mod rtems` **on top** for
`target_os = "rtems"`. This PR merges them into one chain and places the RTEMS
arm *after* the arch arm:

```rust
} else if #[cfg(target_arch = "arm")] {
    mod arm;
    pub use self::arm::*;
} else if #[cfg(target_os = "rtems")] {   // unreachable for armv7-rtems-eabihf
    mod rtems;
    pub use self::rtems::*;
} else {
    core::compile_error!("unsupported target");
}
```

`armv7-rtems-eabihf` is **both** `target_arch = "arm"` and
`target_os = "rtems"` (`rustc --print cfg --target armv7-rtems-eabihf`), so it
takes the `arm` arm and `mod rtems` is never imported. Everything in
`src/unix/newlib/rtems/mod.rs` — `sockaddr_un`, `AF_UNIX`, `RTLD_DEFAULT`, the
whole signal set, `pthread_create`, `pthread_condattr_setclock`, `getentropy`,
`arc4random_buf`, `setgroups`, the `W*` helpers — silently disappears.

Reproduced against this PR applied to current `main`, with a one-file crate
that only names two of those items:

```
error[E0425]: cannot find value `AF_UNIX` in crate `libc`
error[E0425]: cannot find type `sockaddr_un` in crate `libc`
```

The same crate builds on `main` unmodified. Putting the `target_os = "rtems"`
arm *before* the `target_arch` arms fixes it, but note that RTEMS genuinely
needs *both* modules — that is why the second `cfg_if` glob-imports rather than
selecting — so an `else if` chain may not be the right shape here at all.

## Measured type data

Compiled and **run** on `armv7-rtems-eabihf` (RTEMS 6.0.0, gcc 13.3.0, newlib
`1b3dcfd`, `xilinx_zynq_a9_qemu` under `qemu-system-arm`), from
`<sys/types.h>` alone:

```
TC time_t     size=8 align=8 signed=1      TC clock_t    size=8 align=8 signed=0
TC dev_t      size=8 align=8 signed=0      TC clockid_t  size=4 align=4 signed=1
TC ino_t      size=8 align=8 signed=0      TC rlim_t     size=8 align=8 signed=1
```

- **`time_t = i64` is right for RTEMS** — this PR's change is correct there,
  and `off_t = i64` for arm is already correct (measured 8).
- **`dev_t` and `ino_t` are 8 bytes on RTEMS, not 4.** This PR moves them into
  an `any(target_arch = "arm", target_arch = "powerpc")` arm that keeps `u32`,
  so RTEMS still gets the wrong width.
- `rlim_t` (8, signed), `clock_t` (8, unsigned — `libc` has `c_long` via
  `newlib/arm`) and `clockid_t` (signed — `libc` has `c_ulong`) are also wrong
  for RTEMS and untouched here.

Why `time_t` is worth landing sooner: at `i32`, `libc::timespec` is 8 bytes
where RTEMS's is 16, so `std`'s `SystemTime::now`/`Instant::now` hand
`clock_gettime` an 8-byte `MaybeUninit` slot and the kernel writes 12 bytes into
it — 4 past the end on every clock read, with a canary 8 bytes out clobbered.
`tv_nsec` then lands where the 8-byte view never reads it:

```
std SystemTime[0] secs=567993600 subsec_nanos=0
std SystemTime[4] secs=567993600 subsec_nanos=0     <- several ms later
std Instant elapsed secs=0 subsec_nanos=0
```

Every sub-second duration on RTEMS silently measures zero — timeouts, rate
limits and watchdogs all read 0, with no panic and no diagnostic.

I have opened #5308 for the six scalar types and #5307 for a separate RTEMS
socket-address defect. #5308 overlaps this PR on `time_t`; happy to rebase it
down to the five types this one does not cover if this lands first — flagging
so the two are not merged blind to each other. Whatever lands needs to reach
`libc-0.2` to change anything for `std`, which depends on `0.2.x`.
