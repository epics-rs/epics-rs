# RTEMS 6 / QEMU bring-up — measured artefacts

Everything here was **measured on a real toolchain**, not taken from
documentation. `doc/rtems-runtime-acceptance-plan.md` marked exactly two items
`[VERIFY-ON-BOX]` because neither the RTEMS tree nor `qemu-system-arm` existed
on the development machine; both are recorded below, plus the boot evidence
that makes them trustworthy.

**Where.** Remote box `192.168.2.128` (`gv100`, Ubuntu 24.04, 12 cores), all
artefacts under `$HOME/rtems-bringup/` — see the `rtems-qemu-box` memory for
the access and sudo limits that apply there.

## Toolchain

| Component | Version |
|---|---|
| `arm-rtems6-gcc` | GCC 13.3.0 20240521 (RTEMS 6, RSB `5dbc1e08`, Newlib `1b3dcfd`) |
| `arm-rtems6-ld` | GNU Binutils 2.43 |
| `arm-rtems6-gdb` | GDB 17.2 |
| RTEMS kernel + BSP | 6.0.2 `2faafecb7f9df8400fd78a1e6d9b3cf3df0eeccc`, BSP `arm/xilinx_zynq_a9_qemu` |
| rtems-libbsd | branch `6-freebsd-12`, `libbsd.a` 89.9 MB, `rtems_bsd_initialize` present |

## Artefact (a) — the QEMU command line

```
qemu-system-arm -no-reboot -nographic \
  -serial null -serial mon:stdio \
  -M xilinx-zynq-a9 -m 256M \
  -nic user,hostfwd=tcp::5064-:5064,hostfwd=udp::5064-:5064,hostfwd=tcp::5075-:5075,hostfwd=udp::5076-:5076 \
  -kernel <app>.exe
```

Three details that are not guessable and each cost a debugging cycle:

- **`-serial null -serial mon:stdio`, in that order.** The BSP's
  `ZYNQ_UART_KERNEL_IO_BASE_ADDR` defaults to `ZYNQ_UART_1_BASE_ADDR` — the
  *second* UART. A single `-serial mon:stdio` prints nothing at all, which
  looks exactly like a boot failure.
- **The NIC must be `-nic user,...`.** The GEMs are SoC-onboard, so
  `-nic netdev=net0` fails with `Invalid parameter 'netdev'` and
  `-device cadence_gem` would add a *third* NIC. The
  `warning: nic cadence_gem.1 has no peer` line is the harmless second GEM.
- **No `restrict=yes`.** EPICS base's i386 QEMU line carries it
  (`rtems_init.c:1027-1030`); it blocks guest-initiated outbound traffic and is
  wrong for an IOC.

## Artefact (b) — the link command

Driver flags are `-B<BSP>/lib -qrtems -Wl,--gc-sections` plus the ABI flags.
`-qrtems` expands via `collect2` to (`$RB` = `~/rtems-bringup`):

```
-u POSIX_Init
crti.o crtbegin.o
-L$RB/tools/lib/gcc/arm-rtems6/13.3.0/thumb/armv7-a+simd/hard
-L$RB/tools/lib/gcc/arm-rtems6/13.3.0/../../../../arm-rtems6/lib/thumb/armv7-a+simd/hard
-L$RB/tools/arm-rtems6/xilinx_zynq_a9_qemu/lib
-L$RB/tools/lib/gcc/arm-rtems6/13.3.0
-L$RB/tools/lib/gcc/arm-rtems6/13.3.0/../../../../arm-rtems6/lib
--gc-sections <objs> -lbsd -lm -lz
--start-group -lgcc
  --start-group -lrtemsbsp -lrtemscpu -latomic -lc -lgcc --end-group
--end-group
crtend.o crtn.o
-T $RB/tools/arm-rtems6/xilinx_zynq_a9_qemu/lib/linkcmds
```

Three consequences for `.cargo/config.toml` / the shim crate's `build.rs`:

1. `-lbsd -lm -lz` sit **before** the `-qrtems` group, not inside it.
2. `-B<BSP>/lib` is what supplies both the BSP `-L` and the `-T linkcmds`, so
   the linker script is never named explicitly.
3. The multilib is **`thumb/armv7-a+simd/hard`**, selected by
   `-march=armv7-a -mthumb -mfpu=neon -mfloat-abi=hard -mtune=cortex-a9`.
   Rust must emit a matching ABI or its objects land in the wrong multilib.

## Boot evidence

Stock samples, console verbatim:

```
*** BEGIN OF TEST HELLO WORLD ***
*** TEST VERSION: 6.0.0.2faafecb7f9df8400fd78a1e6d9b3cf3df0eeccc
*** TEST BUILD: RTEMS_POSIX_API
*** TEST TOOLS: 13.3.0 20240521 (RTEMS 6, RSB 5dbc1e08…, Newlib 1b3dcfd)
Hello World
*** END OF TEST HELLO WORLD ***
[ RTEMS shutdown ]
```

A hand-written `POSIX_Init` + libbsd shim — the C-side contract our
`rtems-ca-ioc` needs — boots and reaches `main`:

```
SHIM: POSIX_Init entered
nexus0: <RTEMS Nexus device>
SHIM: rtems_bsd_initialize -> 0 (RTEMS_SUCCESSFUL)
SHIM: main() reached
[ RTEMS shutdown ]
```

**It did not boot the first time**, and the failure is worth keeping:
`emerg: rtems_bsd_threads_init_early: cannot create extension`. Cause:
`CONFIGURE_UNLIMITED_OBJECTS` does **not** cover user extensions. Fixed by
adding `CONFIGURE_MAXIMUM_USER_EXTENSIONS 1` per libbsd's own
`testsuite/include/rtems/bsd/test/default-init.h`.

## Networking, proven rather than assumed

DHCP over SLIRP works:

```
cgem0: <Cadence CGEM Gigabit Ethernet Interface> on nexus0
info: cgem0: offered 10.0.2.15 from 10.0.2.2 … acknowledged 10.0.2.15
```

`hostfwd` was proven **byte-level, with a negative control** — not by a
successful connect, because SLIRP completes the host-side accept before the
guest answers, so "connection succeeded" is not evidence:

```
=== bytes from guest port 23 via hostfwd 2323 ===
00000000  ff fb 01 0d 0a 52 54 45  4d 53 20 53 68 65 6c 6c  |.....RTEMS Shell|
00000010  20 6f 6e 20 2f 64 65 76                           | on /dev|
=== NEGATIVE CONTROL 2399 -> guest:99 (nothing listening) ===
(no output)
```

This closes `doc/rtems-runtime-acceptance-plan.md` §3's open question: SLIRP
plus port-preserving `hostfwd` is sufficient, and **no tap device and no sudo
widening are needed**.

## Known rough edges

- **libbsd's `waf install` is not parallel-safe.** `-j12` dies in
  `fix_perms → os.chmod` with `FileNotFoundError` on
  `lib/include/machine/cpufunc.h` — a copy-then-chmod race, not a build error
  (the compile phase logged zero errors). `./waf install -j1` succeeds.
  Upstream waf was not patched; the install is serialised.
- **libbsd's `NET_CFG_SELF_IP` stays at its default `192.168.0.10`.** Not a
  defect for us — our IOC will use DHCP (`10.0.2.15`), where the plain
  `hostfwd=tcp::5064-:5064` form applies. It matters only to anyone reusing
  libbsd's *test* binaries, which must be targeted at `192.168.0.10` or
  reconfigured with `--net-test-config`.

## What is still not done

No Rust has been built or linked on the box — `cargo`/`rustc` are still absent
there. The shim proves the **C** side of the contract; wiring
`.cargo/config.toml` and linking `rtems-ca-ioc` is the next step.
