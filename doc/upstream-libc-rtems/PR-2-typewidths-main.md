# newlib: correct RTEMS scalar type widths

Six scalar types are wrong for `target_os = "rtems"`. Measured with an
`arm-rtems6` toolchain and **run** on `armv7-rtems-eabihf` under
`qemu-system-arm`, including only `<sys/types.h>` and `<time.h>`:

| type | `libc` | measured | this PR |
|---|---|---|---|
| `time_t`    | `i32`     | 8, align 8, signed   | `c_longlong` |
| `dev_t`     | `u32`     | 8, align 8, unsigned | `u64` |
| `ino_t`     | `u32`     | 8, align 8, unsigned | `u64` |
| `rlim_t`    | `u32`     | 8, align 8, signed   | `i64` |
| `clock_t`   | `c_long`  | 8, align 8, unsigned | `c_ulonglong` |
| `clockid_t` | `c_ulong` | 4, **signed**        | `c_int` |

```
TC time_t     size=8 align=8 signed=1      TC clock_t    size=8 align=8 signed=0
TC dev_t      size=8 align=8 signed=0      TC clockid_t  size=4 align=4 signed=1
TC ino_t      size=8 align=8 signed=0      TC rlim_t     size=8 align=8 signed=1
```

The other 13 newlib scalars were measured too and are correct, `off_t`
(already `i64`) included.

## Impact

`libc::timespec` is `{ time_t, c_long }`, so with `time_t = i32` it is 8 bytes
where RTEMS's is 16. `std`'s `SystemTime::now`/`Instant::now` pass a
`MaybeUninit<libc::timespec>` to `clock_gettime`, so **every clock read writes
4 bytes past the end of that stack slot** (measured: 12 bytes written into an
8-byte slot; a canary 8 bytes past it is clobbered), and `tv_nsec` lands where
the 8-byte view never reads it:

```
std SystemTime[0] secs=567993600 subsec_nanos=0
std SystemTime[4] secs=567993600 subsec_nanos=0     <- several ms later
std Instant elapsed secs=0 subsec_nanos=0
```

So on RTEMS today **every sub-second duration silently measures zero** — every
`Instant`-based timeout, rate limit and watchdog reads 0 with no panic and no
diagnostic. That is the half that makes this urgent; the out-of-bounds write is
the half that makes it a soundness bug.

After the patch: `timespec` 16/align 8, `subsec_nanos=15715519`,
`Instant::elapsed()` nonzero. Derived layouts then match the target exactly —
`timeval` 16, `itimerspec` 32, `struct stat` 104 with `st_size` @40.

## Changes

`time_t`, `dev_t`, `ino_t`, `rlim_t`, `clockid_t` gain a `target_os = "rtems"`
arm in `src/unix/newlib/mod.rs`, which already selects by `target_os` before
falling through to `target_arch`. `clock_t` is declared per-arch, so the RTEMS
value goes in `src/unix/newlib/rtems/` (already glob-imported for RTEMS) and
`newlib/arm`'s copy is gated `#[cfg(not(target_os = "rtems"))]`.
**No non-RTEMS target changes.** `timespec`/`timeval`/`itimerspec` need no
separate fix — they follow from `time_t`.

## Verification

- A `const _: () = assert!(...)` file over all seven types: **0 errors** on this
  branch, **16 errors** on `main`.
- `riscv32imc-esp-espidf` builds, unchanged. `armv7-sony-vita-newlibeabihf` and
  `armv6k-nintendo-3ds` fail *identically* before and after (pre-existing
  `E0573` at `src/unix/mod.rs:936`).
- `cargo +nightly fmt --all -- --check` clean.
- Toolchain: RTEMS 6.0.0 `2faafecb`, gcc 13.3.0, newlib `1b3dcfd`, BSP
  `xilinx_zynq_a9_qemu`. Standalone reproduction available (one crate, an RSB
  toolchain and `qemu-system-arm`, nothing else) — happy to attach.

## Relation to #5132

#5132 proposes `time_t = i64` for the non-espidf/vita branch, which covers the
`time_t` row here and is correct. It leaves `dev_t`/`ino_t` at `u32` — measured
8 on RTEMS — and does not touch `rlim_t`, `clock_t` or `clockid_t`. **Happy to
rebase this down to the four types it does not cover if it lands first**;
flagging so the two are not merged blind to each other.

Must reach 0.2 to change anything for `std`: `library/std/Cargo.toml` depends on
`libc 0.2.x`.

@rustbot label stable-nominated
