# libc gap 3: no PI mutex API for the newlib/RTEMS target

Status: **documented, not filed, no patch prepared yet.** Found 2026-07-22
while designing C-parity priority inheritance for the epics-rs record lock
(handoff §8.0 gap 4). Unlike gaps 1–2 this is a missing-API gap, not a
wrong-definition gap — nothing miscompiles; the API simply cannot be called
from Rust through the `libc` crate.

## What is missing

`libc` rev `6f64e70` (fork of 0.2.188 = `physwkim/libc` branch
`epics-rs-rtems-0.2`, i.e. upstream 0.2.188 + PR-1 + PR-2) exposes for the
newlib/RTEMS target only:

- `pthread_mutex_init/lock/trylock/unlock/destroy`,
  `pthread_mutexattr_init/destroy/settype` (generic `src/unix/mod.rs:1335-1350`)
- `pthread_mutexattr_t` as a struct (`src/unix/newlib/mod.rs:329`)

It has **zero** of the POSIX mutex protocol surface:

| symbol | in libc crate | in target newlib headers |
|---|---|---|
| `PTHREAD_PRIO_NONE` | absent | `0` — `sys/_pthreadtypes.h:81` |
| `PTHREAD_PRIO_INHERIT` | absent | `1` — `sys/_pthreadtypes.h:82` |
| `PTHREAD_PRIO_PROTECT` | absent | `2` — `sys/_pthreadtypes.h:83` |
| `pthread_mutexattr_setprotocol` | absent | `pthread.h:189` |
| `pthread_mutexattr_getprotocol` | absent | `pthread.h:191` |
| `pthread_mutexattr_setprioceiling` | absent | `pthread.h:193` |
| `pthread_mutexattr_getprioceiling` | absent | `pthread.h:195` |
| `pthread_mutex_setprioceiling` | absent | `pthread.h:203` |
| `pthread_mutex_getprioceiling` | absent | `pthread.h:206` |

Header paths are relative to the toolchain sysroot include dir; read on the
build box from `~/rtems-bringup/tools/arm-rtems6/include/` on 2026-07-22
(same toolchain that produced every other measurement in
`doc/upstream-rtems-bugs/`).

The feature macros are unconditionally on for RTEMS:
`sys/features.h:394-395` defines `_POSIX_THREAD_PRIO_INHERIT 1` and
`_POSIX_THREAD_PRIO_PROTECT 1` inside the `__rtems__` block, so the
prototypes above are always visible to C code and the symbols are always
present in the RTEMS-provided libc at link time. RTEMS implements the
protocols in-kernel (POSIX mutexes map onto Score mutexes; inherit protocol
is the same CORE mutex machinery `epicsMutexOsdCreate` reaches via
`RTEMS_INHERIT_PRIORITY | RTEMS_PRIORITY`).

## Why it matters to us

C EPICS on RTEMS gets priority inheritance on every `epicsMutex`
(dbLock.c:86 `epicsMutexMustCreate`). Rust-side parity needs
`pthread_mutexattr_setprotocol(&attr, PTHREAD_PRIO_INHERIT)` before
`pthread_mutex_init` — the constants and the two protocol functions at
minimum. Without them in the `libc` crate, a PI mutex for
`target_os = "rtems"` cannot be written against the crate.

## Filing shape (when we file)

Same routing as gaps 1–2 (see README): ONE PR against `main`, `@rustbot
label stable-nominated` comment for the 0.2 backport. Content shape:

- constants `PTHREAD_PRIO_NONE/INHERIT/PROTECT` = 0/1/2 in
  `src/unix/newlib/mod.rs` (values are newlib-generic, defined outside any
  target `#if` in `sys/_pthreadtypes.h`, so newlib scope not rtems scope
  is defensible — check espidf/horizon/vita builders before choosing)
- extern fns `pthread_mutexattr_{set,get}protocol` (and optionally the
  three prioceiling fns) — newlib guards them behind
  `_POSIX_THREAD_PRIO_INHERIT || _POSIX_THREAD_PRIO_PROTECT`, which
  espidf/vita may not define; if their sysroots lack the symbols the
  externs must go in `src/unix/newlib/rtems/mod.rs` instead. VERIFY
  against those targets' sysroots before picking the file; RTEMS-only
  placement is the safe minimum.

Per the maintainer's standing instruction (tgross35: no AI-generated PR
descriptions/comments): this file is internal evidence only; the PR body
gets handwritten at filing time.

## Interim (until it lands in a published 0.2)

Two options, decided in `doc/rtems-priority-locks-design.md`:

1. add the constants + externs to our fork branch `epics-rs-rtems-0.2`
   (needs a user-authorized push to the fork), or
2. declare the `extern "C"` block + constants locally in epics-rs
   (no libc change at all; symbols exist in the RTEMS libc at link time;
   `pthread_mutexattr_t` still comes from the libc crate so no layout
   duplication).

Option 2 has no distribution dependency and keeps the fork identical to
the filed PRs; it is the same pattern the codebase already uses for other
RTEMS-only symbols. Either way this file is the removal marker: when the
upstream PR lands in a published 0.2 release, delete the local
externs/fork commit together with the `[patch.crates-io]` entry.
