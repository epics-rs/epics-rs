# It links, it boots, and it stops at an upstream `libc` defect

The first cross-toolchain link and boot of `rtems-ca-ioc`. Measured on the
bring-up box against `684e5508`; the fixes it produced landed as `8a8c3071` and
`51cef3f2`.

| Rung (`doc/rtems-runtime-acceptance-plan.md`) | Result |
|---|---|
| 0 — it links | **PASS** |
| 1 — it boots and prints | **PASS** |
| 2 — it holds port 5064 | **FAIL** — upstream `libc`, §3 |
| 3–6 — search, caget, camonitor, endurance | **BLOCKED** by rung 2 |

## 1. Rung 0 — the link, and the fifth cause

Four things were flagged as likely to break the link. None of them did. The
cause was a fifth, and it is in `8a8c3071`: `CONFIGURE_APPLICATION_NEEDS_LIBBLOCK`
is not optional on a BSP whose nexus device set contains a block device, because
omitting the directive drops libblock's *configuration* rather than libblock.

With it: exit 0, a 7,285,116-byte image.

```
Class: ELF32   Machine: ARM   Type: EXEC   Flags: 0x5000400, Version5 EABI, hard-float ABI
0016c390 T POSIX_Init      001696f0 T main      0048f300 T rtems_bsd_initialize
```

### The four flagged items, all settled by measurement

1. **`bsp_include_dir` — the guess was right.** The method was wrong, though: a
   BSP *sample's* compile line is not evidence about the installed layout,
   because the kernel's waf build compiles out of its own source tree. libbsd is
   the right analogue.
2. **`-Bdynamic` is harmless** — no `-static` and no reordering needed. rustc
   emits `-Wl,-Bdynamic "-lbsd" "-lm" "-lz"`, but RTEMS ships only `.a`, so ld
   resolves from archives. Proof: `rtems_bsd_initialize` is in the image at
   `0x0048f300`, and `readelf -d` reports no dynamic section at all.
3. **`CONFIGURE_MAXIMUM_FILE_DESCRIPTORS` is the RTEMS 6 spelling, and 150
   stands.** `FD_SETSIZE` is **256** on RTEMS (newlib `sys/select.h:33-34`,
   `__rtems__` arm), not the 64 that base's own caveat assumes.
4. **`CONFIGURE_MAXIMUM_USER_EXTENSIONS 1` is enough** — the stack checker is an
   *initial* extension from the static table and never draws on the
   runtime-created pool. The hypothesis that it would was wrong.

## 2. Rung 1 — it boots

Console verbatim. Every `rtems-boot:` marker appeared, in order:

```
rtems-boot: POSIX_Init entered (RTEMS rtems-6.0.0 (ARM/ARMv4/xilinx_zynq_a9_qemu))
rtems-boot: initializing libbsd
rtems-boot: dhcp BOUND          (cgem0: offered 10.0.2.15 from 10.0.2.2)
rtems-boot: -------- ifconfig --------
rtems-boot: -------- netstat -rn --------
rtems-boot: main() reached
rtems-ca-ioc: cannot bind CA TCP port 5064: invalid argument
rtems-boot: IOC terminated with 1
```

## 3. Rung 2 — the blocker is upstream, and `bind` is not what failed

The IOC's message names the wrong operation. From a standalone RTEMS Rust probe
built outside the workspace:

```
rustprobe: TcpListener 0.0.0.0:5064   -> ok
rustprobe: UdpSocket   0.0.0.0:5064   -> ok
rustprobe: bound 5065
rustprobe: local_addr() on 5065       -> ERR kind=InvalidInput os=None msg=invalid argument
```

`os=None` means **no syscall failed**. A C probe replaying std's exact sequence
confirms the kernel is fine: `socket`, `fcntl(F_GETFD)`, `fcntl(F_SETFD)`,
`setsockopt(SO_REUSEADDR)`, `bind(0.0.0.0:5064)` and `listen(128)` all return 0.

It is a struct-layout mismatch, measured in the guest:

```
probe2: sizeof(sockaddr_in)=16 offsetof(sin_len)=0 offsetof(sin_family)=1
probe2: sizeof(sockaddr_storage)=128 offsetof(ss_family)=1 AF_INET=2
probe2: getsockname len=16 bytes: 10 02 00 00 00 00 00 00
```

RTEMS is FreeBSD-derived, so byte 0 is `ss_len` = 16 and byte 1 is `ss_family`
= 2. But `libc-0.2.185/src/unix/newlib/arm/mod.rs:20,27` declares `sockaddr_in`
and `sockaddr_storage` **without the length byte**, and `newlib/mod.rs:36-42`
sets `sa_family_t = u8`. So std's `socket_addr_from_c`
(`sys/net/connection/socket/mod.rs:194`) reads offset 0, gets **16**, matches
neither `AF_INET` nor `AF_INET6`, and returns the `InvalidInput` at `:208`.

**The `aarch64` sibling is correct** — `newlib/aarch64/mod.rs:22-28` has
`sin_len`. `arm` is the outlier. A second defect in the same declaration:
`sockaddr_storage` should be 128 bytes, not the ~28 the arm definition implies.

### Why there is no worthwhile workaround in our code

The blast radius is every caller of `socket_addr_from_c`: `local_addr`,
`peer_addr`, **`accept` (`mod.rs:609`)**, `UdpSocket::recv_from`
(`unix.rs:350`), and `lookup_host` (`mod.rs:324`). Even if `BlockingCaServer::bind`
skipped `local_addr()`, every accepted CA connection would still fail. The fix
belongs upstream in `libc`, mirroring what aarch64 already does.

## 4. `getpwnam` with no `/etc/passwd` synthesizes a root entry

This is the answer `doc/rtems-cfg-unix-trap-audit.md` §7 left open, and it is
the dangerous one of the three outcomes it listed.

`cpukit/libcsupport/src/pwdgrp.c`: `getpw_r` calls `_libcsupport_pwdgrp_init()`
at **line 201**, *before* `fopen("/etc/passwd")` at line 203. That runs
`pwdgrp_init()` once, which `mkdir("/etc")` and then `init_file` with these
literals:

- line 77 — `init_file("/etc/passwd", "root::0:0::::\n");`
- line 82 — `init_file("/etc/group",  "root::0:\n");`

Confirmed at runtime in the guest:

```
probe3: stat(/etc/passwd) before any call -> -1 (errno=2 No such file or directory)
probe3: getpwnam(root)   HIT  name=root uid=0 gid=0 dir=[] shell=[]
probe3: getgrgid(0)      HIT  gr_name=root gr_gid=0 gr_mem[0]=(none)
probe3: getpwnam(nobody) MISS errno=22 (Invalid argument)
probe3: stat(/etc/passwd) after -> 0
```

Two things a PVA authorization decision must not miss. **Only `root` is
synthesized** — every other name misses. And the miss is a **POSIX deviation**:
`getpw_r` returns -1 with `errno=EINVAL` (line 222) where POSIX specifies return
0 with `*result=NULL`, so on RTEMS an unknown user looks like `EINVAL`, not
not-found.

So an unconfigured RTEMS image would have resolved the account `root` to uid 0 /
gid 0 and handed the ACF gate the role `root`, silently. That is finding S1 of
the `cfg(unix)` audit, and `2b7f7154` closed it before this measurement existed
— the measurement confirms the fix was necessary rather than precautionary.

## 5. Open

- **The `libc` `sin_len`/`ss_len` omission on `armv7-rtems-eabihf`** blocks rungs
  2–6. Not patched locally: patching a registry crate in place would have hidden
  the defect rather than closed it.
- **`rtems-ca-ioc` attributes a `local_addr()` failure to "cannot bind".** A
  reporting defect, not the blocker.
