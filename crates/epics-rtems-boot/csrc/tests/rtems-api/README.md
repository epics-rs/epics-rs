# Recorded RTEMS API

The declarations `../../rtems_init.c` and `../../rtems_shell_cmds.c` name,
copied verbatim out of an installed RTEMS 6 BSP, so that a CI runner with no
cross toolchain can compile those files with `-Werror`.

## Why this exists

`rtems_init.c` is `POSIX_Init` — the entry task of every target image. Until
this directory existed it was compiled by nothing except a real image build on
the bring-up box: `crates/epics-rtems-boot/build.rs` runs `cc` only when
`RTEMS_BSP_PREFIX` resolves, `scripts/rtems-check.sh` is a
`cargo check --target armv7-rtems-eabihf` that type-checks Rust and never runs
`cc`, and `scripts/csrc-check.sh` reached only `boot_args.c`. A typo in the
boot path could not turn a job red, and its first reader was the serial console
of a board that would not boot.

Compiling it *for the target* in CI is not reachable, and the reason is not a
missing configuration:

- No distribution packages an `arm-rtems6` toolchain.
- RTEMS publishes source only. `ftp.rtems.org/pub/rtems/releases/6/6.1/`
  and `.../6.2/` carry `sources/` (`rtems-6.1.tar.xz`,
  `rtems-source-builder-6.1.tar.xz`, `rtems-libbsd-6.1.tar.xz`, …) and a
  `contrib/` holding nothing but `README.md` and `rtems-release/`. There is no
  prebuilt compiler and no prebuilt BSP.
- Building one with the RTEMS Source Builder takes hours; the resulting prefix
  on the bring-up box is 1.9 GB.

Linking is further out of reach again: it needs `libbsd.a`, `librtemsbsp.a` and
`librtemscpu.a` from a BSP build, which is the same hours.

What a runner *can* do is compile `rtems_init.c` for the host — the code is
ordinary C, and what it needs from RTEMS is 48 declarations. Those are recorded
here.

## What this proves, and what it does not

Proved on every push, by `scripts/csrc-check.sh`:

- `rtems_init.c` compiles under `-Wall -Wextra -Werror`, in all four
  configurations its `#if`s select (DHCP; static address; static address with a
  gateway; `EPICS_RTEMS_BSD_LOG_DEBUG=0`).
- `rtems_shell_cmds.c` — the RTEMS half of the operator commands base registers
  from `iocshRegisterRTEMS` — compiles under the same flags. It has no `#if` of
  its own, so one configuration is all of it.
- Every RTEMS and libbsd name it uses exists, with the right arity, argument
  types and return type — including the `printf` format checking that
  `RTEMS_PRINTFLIKE` puts on `rtems_panic`.
- Nothing was hand-written into the record to make an error go away
  (`scripts/rtems-api-check.sh` pass 1).

Proved wherever `RTEMS_BSP_PREFIX` is set — the bring-up box, an image build —
by `scripts/rtems-api-check.sh` pass 2: every recorded block still appears
verbatim in the header it names.

Not proved anywhere but on the target: that the image links, that the sizes and
values agree with the real ABI, and that it boots. The on-target run stays the
acceptance.

## Not covered by the record

`rtems_config.c` and `rtems_stats.c` are still compiled only by an image build,
and recording their API would not work:

- `rtems_config.c` ends in `#include <rtems/confdefs.h>`, the header that
  *generates* the application configuration table from the `CONFIGURE_*`
  macros above it. Recording declarations cannot stand in for it, and a stub
  would prove nothing about the table it produces.
- `rtems_stats.c` reaches into `<rtems/score/threadimpl.h>`,
  `<rtems/score/protectedheap.h>` and `<rtems/libio_.h>` — RTEMS internals
  whose surface is large and unstable. A record of those would cost more to
  keep true than the check is worth.

## Adding or re-recording a declaration

The record is written by hand, from a real BSP. There is no generator, because
generating it would need the very toolchain CI does not have.

1. On a machine with an installed BSP, find the declaration:

       B=$RTEMS_BSP_PREFIX/arm-rtems6/$RTEMS_BSP/lib/include
       grep -n 'rtems_task_set_priority' $B/rtems/rtems/tasks.h

   Newlib headers such as `<sys/socket.h>` are in the toolchain sysroot,
   `$RTEMS_BSP_PREFIX/arm-rtems6/include`, not the BSP tree.

2. Copy the lines out byte for byte — tabs included — into the matching file
   here, wrapped in markers:

       /* @rtems-api rtems/rtems/tasks.h */
       rtems_status_code rtems_task_set_priority(
         rtems_id             id,
         rtems_task_priority  new_priority,
         rtems_task_priority *old_priority
       );
       /* @rtems-api-end */

   The path in the marker is relative to whichever include root holds it. The
   block must be contiguous in that header; split it into two markers if the
   part you need is not.

3. Anything that is not a copy — an `#include`, an elision, a header guard —
   goes under `/* @rtems-api-local: <why> */` instead. Pass 1 rejects a
   declaration under neither.

4. Run `scripts/rtems-api-check.sh` there, with `RTEMS_BSP_PREFIX` set, so
   pass 2 confirms the copy; then `scripts/csrc-check.sh` anywhere.
