# newlib: give RTEMS its own socket address types

On `armv7-rtems-eabihf` every `libc` socket address struct is missing the BSD
length byte, and 27 socket/file constants have the wrong value. The result:
**`TcpListener::bind` succeeds but `local_addr()` fails with `InvalidInput`**,
and every `accept`/`peer_addr`/`recv_from` fails the same way — `std` cannot
parse an address RTEMS returns. Networking is unusable on the target.

Two commits: the address structs (`sockaddr`, `sockaddr_in`, `sockaddr_in6`,
`sockaddr_un`, `sockaddr_storage`), then the constants (`AF_INET6`, `PF_INET6`,
`SOL_SOCKET`, `FIONBIO`, `POLL*`, `MSG_*`, `O_CLOEXEC`, `SOCK_CLOEXEC`, `NCCS`,
`FD_SETSIZE`, `TCP_NODELAY`, `TCP_MAXSEG`, `IP_TTL`, `IP_{ADD,DROP}_MEMBERSHIP`,
`h_errno`, `AI_*`, `NI_*`, `EAI_*`).

## Why `newlib/rtems/`, not `newlib/arm/`

RTEMS's network stack is `rtems-libbsd`, imported from FreeBSD, so its socket
addresses carry a leading one-byte length. **The length byte comes from the OS,
not the architecture** — plain newlib on arm has no such stack and no length
byte. `src/unix/newlib/mod.rs` already selects by `target_os` first (espidf,
horizon, vita) before falling through to `target_arch`, and already glob-imports
`mod rtems` for `target_os = "rtems"`, so this follows the layout that is there.
`newlib/arm`'s copies become `#[cfg(not(target_os = "rtems"))]`, so **no
non-RTEMS target changes**.

`newlib/aarch64/mod.rs:22` already has `sin_len` — correct *shape*, wrong
*place*: it silently changes plain `aarch64-none-newlib` too. This PR does not
repeat that. `sa_family_t = u8` is **correct** for RTEMS and is left alone; one
length byte plus one family byte is exactly the BSD layout.

## Evidence

From the toolchain headers a shim compile actually resolves (`arm-rtems6-gcc -M`):

```c
struct sockaddr    { unsigned char sa_len;  sa_family_t sa_family; char sa_data[14]; };
struct sockaddr_in { uint8_t sin_len; sa_family_t sin_family; in_port_t sin_port;
                     struct in_addr sin_addr; char sin_zero[8]; };
struct sockaddr_storage { unsigned char ss_len; sa_family_t ss_family; /* ... */ };
```

`sys/socket.h:246` has `AF_INET6 28`; `libc` says 23. `_SS_MAXSIZE` is 128,
where `newlib/arm/mod.rs:27` declared `sockaddr_storage` as **28 bytes**.

In the guest, `getsockname` on `0.0.0.0:5064` returns `10 02 00 00 …` —
`ss_len = 16`, `ss_family = 2`. `std`'s `socket_addr_from_c` reads offset 0,
gets **16**, matches neither `AF_INET` nor `AF_INET6`, and returns
`InvalidInput` with `raw_os_error() == None` — no syscall failed.

Same binary and image, `libc` the only variable:

```
before:  cannot bind CA TCP port 5064: invalid argument
after:   serving 3 records on CA port 5064 (TCP + UDP search)
         tcp local_addr = Ok(0.0.0.0:5064)
         TCP accept peer=Ok(192.168.2.127:48684) local=Ok(10.0.2.15:5064)
```

Scope measured by compiling, for `armv7-rtems-eabihf`, a generated file of
`const _: () = assert!(libc::NAME == <value from arm-rtems6-gcc>);`, so `cfg`
resolution is the compiler's and not a script's:

| | before | after |
|---|---|---|
| constants wrong (of 305 measurable) | 27 | 0 |
| `sockaddr*` layout facts wrong | 10 of 10 | 0 |

`arm` is the only reachable RTEMS outlier: `aarch64` has the right shape but no
`sockaddr_storage`; `powerpc` (devkitPPC) documents having no sockaddr;
`espidf`/`vita` already carry length bytes; `horizon` is not BSD-derived.

## Verification

- Builds for `armv7-rtems-eabihf`; `cargo +nightly fmt --all -- --check` clean.
- `riscv32imc-esp-espidf` builds, unchanged. `armv7-sony-vita-newlibeabihf` and
  `armv6k-nintendo-3ds` fail *identically* before and after (pre-existing
  `E0573` at `src/unix/mod.rs:936`).
- RTEMS 6.0.0 `2faafecb`, gcc 13.3.0, newlib `1b3dcfd`, BSP
  `xilinx_zynq_a9_qemu` under `qemu-system-arm`.

Must reach 0.2 to unblock anyone: `library/std/Cargo.toml` depends on
`libc 0.2.x`, so a `main`-only fix never reaches the `std` that `-Zbuild-std`
compiles.

@rustbot label stable-nominated

## Not in this PR

- **Scalar type widths** — `time_t`, `dev_t`, `ino_t`, `rlim_t`, `clock_t` are 8
  bytes on RTEMS and declared 4; `clockid_t` is signed and declared unsigned.
  Separate PR (#\<TYPEWIDTHS_PR\>), because it overlaps the open #5132. Both PRs
  add to `src/unix/newlib/rtems/mod.rs`; whichever lands second needs a one-hunk
  context rebase there.
- `fcntl(F_DUPFD)` fails on an `rtems-libbsd` socket, so `TcpStream::try_clone`
  cannot work there. **Not a `libc` defect** — `rtems-libbsd` installs
  `rtems_bsd_sysgen_open_error` as `.open_h` on every socket, which
  `duplicate_iop` (`cpukit/libcsupport/src/fcntl.c`) calls. Reported separately.
