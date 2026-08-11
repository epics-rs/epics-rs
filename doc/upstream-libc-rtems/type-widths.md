# `arm-rtems6` measured type widths vs `libc` 0.2.188 `src/unix/newlib/`

Measured on the target, not read from headers: compiled with
`arm-rtems6-gcc -march=armv7-a -mthumb -mfpu=neon -mfloat-abi=hard -qrtems`
against the `xilinx_zynq_a9_qemu` BSP and **run under `qemu-system-arm`**
(`~/rtems-bringup/tsmeasure.c`, `tsmeasure2.c`).
RTEMS 6.0.0 `2faafecb7f9df8400fd78a1e6d9b3cf3df0eeccc`, newlib `1b3dcfd`, gcc 13.3.0.

| type | libc newlib decl | libc bytes | measured bytes | align | signed | verdict |
|---|---|---|---|---|---|---|
| `time_t`      | `i32` (mod.rs:63)         | 4 | **8** | 8 | yes | **WRONG** |
| `off_t`       | `i64` (mod.rs:20)         | 8 | 8 | 8 | yes | ok |
| `dev_t`       | `u32` (mod.rs:18)         | 4 | **8** | 8 | no  | **WRONG** |
| `ino_t`       | `u32` (mod.rs:19)         | 4 | **8** | 8 | no  | **WRONG** |
| `rlim_t`      | `u32` (mod.rs:34)         | 4 | **8** | 8 | yes | **WRONG** |
| `suseconds_t` | `i32` (mod.rs:46)         | 4 | 4 | 4 | yes | ok |
| `blkcnt_t`    | `i32` (mod.rs:3)          | 4 | 4 | 4 | yes | ok |
| `blksize_t`   | `i32` (mod.rs:4)          | 4 | 4 | 4 | yes | ok |
| `nlink_t`     | `c_ushort` (mod.rs:31)    | 2 | 2 | 2 | no  | ok |
| `mode_t`      | `c_uint` (mod.rs:30)      | 4 | 4 | 4 | no  | ok |
| `clockid_t`   | `c_ulong` (mod.rs:6)      | 4 | 4 | 4 | **yes** | size ok, signedness differs |
| `fsblkcnt_t`  | `u64` (mod.rs:24)         | 8 | 8 | 8 | no  | ok |
| `fsfilcnt_t`  | `u32` (mod.rs:25)         | 4 | 4 | 4 | no  | ok |
| `id_t`        | `u32` (mod.rs:26)         | 4 | 4 | 4 | no  | ok |
| `key_t`       | `c_int` (mod.rs:27)       | 4 | 4 | 4 | yes | ok |
| `useconds_t`  | `u32` (mod.rs:54)         | 4 | 4 | 4 | no  | ok |
| `pthread_t`   | `c_ulong` (mod.rs:32)     | 4 | 4 | 4 | -   | ok |
| `pthread_key_t` | `c_uint` (mod.rs:33)    | 4 | 4 | 4 | -   | ok |
| `clock_t`     | (from `unix/mod.rs`)      | - | 8 | 8 | no  | check |

Derived struct layouts (all follow from `time_t`; no separate struct fix needed
once `time_t = c_longlong`):

| struct | measured size | align | fields |
|---|---|---|---|
| `struct timespec`  | **16** | 8 | `tv_sec` @0 (8), `tv_nsec` @8 (4), 4 tail pad |
| `struct timeval`   | **16** | 8 | `tv_sec` @0 (8), `tv_usec` @8 (4), 4 tail pad |
| `struct itimerspec`| **32** | 8 | two `timespec` |
| `struct stat`      | 104 | 8 | `st_size` @40, `st_atim` @48, `st_mtim` @64 |
| `pthread_mutex_t`  | 64  | 8 | |
| `pthread_cond_t`   | 28  | 4 | |

`libc` before the fix: `timespec` = `{ time_t, c_long }` = `{ i32, i32 }` = **8 bytes, align 4**.

## Write extent — how far past the end the kernel writes

```
TSM clock_gettime wrote bytes [16..27] of a 48-byte buffer, slot at 16 => extent 12 bytes
TSM gettimeofday  wrote bytes [16..27]                                 => extent 12 bytes
```

12 bytes written into an 8-byte slot ⟹ **4 bytes past the end**, every call.

## Addendum (measured 2026-07-21, from `<sys/types.h>` + `<time.h>` only)

`~/rtems-bringup/typecheck.c`, run under QEMU:

```
TC rlim_t       size=8 align=8 signed=1
TC dev_t        size=8 align=8 signed=0
TC ino_t        size=8 align=8 signed=0
TC time_t       size=8 align=8 signed=1
TC clock_t      size=8 align=8 signed=0
TC clockid_t    size=4 align=4 signed=1
TC nlink_t      size=2 align=2 signed=0
TC blkcnt_t     size=4 align=4 signed=1
```

Two corrections to the table above:

- `clock_t` was listed as "check". It is **WRONG**: 8 bytes unsigned on RTEMS,
  declared `c_long` (4, signed) in `src/unix/newlib/arm/mod.rs:3`. This is a
  fifth wrong type, found after the table was first written.
- `clockid_t` is 4 bytes as libc says, but **signed**; libc has `c_ulong`.

`struct rlimit` is an **incomplete type** on RTEMS from
`<sys/resource.h>` — the `rlim_t` typedef is reachable from `<sys/types.h>`,
but there is no application-level `getrlimit`/`setrlimit`. `rlim_t` is still
wrong in libc; it is just less likely to be hit.
