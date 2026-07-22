# RTEMS deviation — we run base's score-arm ceiling of 150 on a target where base runs the POSIX arm's 64

**Status:** recorded deviation, measured on the bring-up box
**Date:** 2026-07-22
**Configuration site:** `crates/epics-rtems-boot/csrc/rtems_config.c` §F
**Upstream reference:** `epics-base` `modules/libcom/RTEMS/posix/rtems_config.c:83`

This file exists so that nobody re-runs a 300-connection ramp on the target to
re-learn that raising the cap buys nine connections. Every number below was
either read out of a source file named here, or measured on the bring-up box
during the scope-B session. Nothing is estimated.

---

## 1. The deviation — and what shape it actually is

**150 is not a number we invented.** Upstream base carries *two* values, one per
OS-API arm:

| | value | source |
|---|---|---|
| EPICS base, **POSIX** arm | **64**, spelled `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS` | `modules/libcom/RTEMS/posix/rtems_config.c:83` |
| EPICS base, **score** arm | **150**, spelled `CONFIGURE_LIBIO_MAXIMUM_FILE_DESCRIPTORS` | `modules/libcom/RTEMS/score/rtems_config.c:36` |
| **our guest** | **150** | `crates/epics-rtems-boot/csrc/rtems_config.c` §F |

Our shim's §F already records that we took base's score-arm value deliberately.
The deviation is therefore *not* "we picked an arbitrary number" — it is which
**arm** we take the number from, and that matters because base does not compile
both:

- `configure/toolchain.c:32-35` — `#if __RTEMS_MAJOR__>=5` ⟹ `OS_API = posix`,
  `#else` ⟹ `OS_API = score`.
- `modules/libcom/RTEMS/Makefile:15` — `SRC_DIRS += ../$(OS_API)`, and `:27`
  pulls `rtems_config.c` out of whichever arm directory that selected.

So **an RTEMS 6 build of base compiles the POSIX arm, and the value base
actually runs with on this target is 64.** The score arm's 150 is a value base
still ships but no longer compiles here. Stated exactly: *we run the score arm's
150 on a target where base runs the POSIX arm's 64.* That is the deviation.

(The same `OS_API = posix` selection is why our shim is derived from the POSIX
arm in the first place, and it is cited for the same reason in the stack-class
argument — handoff §5.2.)

### The cap is not hard-coded — this is the bisect route

`§F` wraps the value in an `#ifndef` / `#define` / `#endif`, deliberately, so a
build can carry a different ceiling **without a source edit**:

```c
#ifndef CONFIGURE_MAXIMUM_FILE_DESCRIPTORS
#define CONFIGURE_MAXIMUM_FILE_DESCRIPTORS 150
#endif
```

Any `-D CONFIGURE_MAXIMUM_FILE_DESCRIPTORS=N` reaching the shim's compile line
wins. That compile line is built by `crates/epics-rtems-boot/build.rs`, which
hands the three `csrc/*.c` files to `cc::Build` (`cc 1.2.60`). The fd=400 image
in §3 — the one that produced the memory-wall measurement — is an image built
with a different ceiling, and this `#ifndef` is what makes such an image
possible without touching this file.

*Inferred, not exercised in this tree:* `build.rs` adds no dedicated knob for
this — its only `.define()` is `BSP_<bsp>` — so the env-var route would be
`cc`'s own target `CFLAGS` handling
(`CFLAGS_armv7_rtems_eabihf=-DCONFIGURE_MAXIMUM_FILE_DESCRIPTORS=400`). That
follows from `cc`'s documented behaviour; it has not been run from this
worktree, and nothing here records how the fd=400 image was actually built.

### Why base caps at 64 — SETTLED, and it does not bind on us

Do not re-open this: both halves below are recorded next to the configuration
itself (`csrc/rtems_config.c` §F) and one of them is a measurement, not an
argument.

Base's comment sits directly above its `#define`
(`modules/libcom/RTEMS/posix/rtems_config.c:70-81`) and says, verbatim, that
`select()` can only be used with the first `FD_SETSIZE` descriptors (newlib
default 64), that since RTEMS 5.1 descriptors are allocated sequentially, and
that a cap at or above `FD_SETSIZE` "will likely cause applications making
`select()` calls to fault at some point". It then states outright:

> IOC core components (libca and RSRV) do not make `select()` calls.

Two facts close it:

- **`FD_SETSIZE` on this BSP is 256, not the newlib default of 64.** newlib's
  `sys/select.h:33-34` takes the `__rtems__` arm — **confirmed by preprocessing
  with the real BSP include path**, so this is measured rather than reasoned.
  150 is under that ceiling *even for a library that does call `select()`*, so
  base's caveat cannot fire at our cap regardless of what any code does.
- **Nothing of ours calls `select()`/`poll()` anyway.**
  `rg 'libc::select|libc::poll|FD_SET'` across `epics-base-rs`, `epics-ca-rs`
  and `epics-pva-rs` returns zero hits — this port is blocking
  thread-per-connection with no reactor anywhere.

The first fact is the load-bearing one: it holds even if the second stops
holding. The second would only become relevant again above 256.

The macro spelling is settled too: `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS` is the
RTEMS 6 name (`confdefs/libio.h:89` reads it; `confdefs/obsolete.h:109-111`
turns the older `CONFIGURE_LIBIO_…` spelling into a rename `#warning`).

---

## 2. What the deviation costs and buys — measured

Identical driver on both stacks: raw CA TCP, version handshake, one
`CA_PROTO_CREATE_CHAN`, counted "served" only on reply `18`.

| build | cap | last served | first refused |
|---|---|---|---|
| stock EPICS base | 64 | 53 | #54 |
| base, rebuilt to match our guest | 150 | 139 | #140 |
| **`epics-rs`** | **150** | **142** | **#143** |

Both stacks print the same console line and fail with the same errno:

```
[zone: socket] kern.ipc.maxsockets limit reached
CAS: Client accept ERROR: Too many open files in system      # ENFILE
```

**C is 3 lower than us at the same cap, and the reason is not efficiency — it
holds 3 more descriptors itself at idle.** (Arithmetic from the table: the
served count is the cap minus the IOC's own idle hold — 150 − 142 = 8 for us,
150 − 139 = 11 and 64 − 53 = 11 for C, self-consistent across both C rows.)

---

## 3. There are two walls, and 142 is not the memory one

This is the distinction that makes the cap worth so little.

| | set by | our guest | if RAM doubles |
|---|---|---|---|
| **fd wall** | `MAXIMUM_FILE_DESCRIPTORS − 8` | **142** | unchanged |
| **memory wall** | free heap ÷ 1,589,000 B | **151** | roughly doubles |

142 is arithmetic, not a memory result: the cap is 150 and the IOC itself holds
8 descriptors at idle. The status PVs confirm it directly (§5 below —
`FD_CNT + FD_FREE = FD_MAX = 150` on every row), and so does the errno:
`ENFILE`, not `ENOMEM`.

**The effective ceiling is the lower of the two, so raising either one alone
buys almost nothing.** Raising the cap buys 142 → 151. Adding RAM buys nothing
at all while the cap is 150. The lever that would move both is per-connection
memory, 97.4 % of which is the two thread stacks — and C over-provisions those
exactly as we do, so cutting the stack classes would be a deviation from
measured C behaviour rather than a correction of one.

### The fd=400 experiment — this is the one not to re-run

An image was built with the cap at 400 so the fd wall no longer binds. Result:

- **151 served**, by two independent derivations: 300 connections attempted
  with 149 refused, and `(259,803,736 − 19,880,696) / 1,589,000 = 150.99`.
- Refusals are **`EAGAIN`** (thread creation), not `ENFILE`, and are announced
  on powers of two.
- Per-connection cost measured across that ramp is flat:
  1,588,393 / 1,589,254 / 1,588,876 / 1,589,431 B at 25 / 50 / 100 / 140
  connections — spread 1,038 B, 0.065 %.

**So the cap buys 9 connections, 142 → 151, and then the 256 MB guest is out of
heap.** That is the whole return on raising it.

**Scope limit, stated:** these are VERSION-handshake holds with no channels and
no subscriptions, so this measures the *connection object*, not per-channel
state.

---

## 4. What an operator sees — the ceiling is published, with two traps

`rtems-pva-ioc` publishes devIocStats-named PVs through a one-second pusher
thread (a `ReadHook` would not work: it is GET-only, so `camonitor` on a
hook-backed PV never updates). Verified with both `caget` and `camonitor`.

| held | `FD_CNT` | `FD_FREE` | `CA_CONN_CNT` | `MEM_FREE` |
|---:|---:|---:|---:|---:|
| 0 | 8 | **142** | 0 | 241,199,000 |
| 100 | 108 | 42 | 100 | 82,313,800 |
| 141 | 149 | 1 | 141 | 17,148,200 |
| 142 | *unreadable* | — | — | — |

`FD_CNT + FD_FREE = FD_MAX = 150` at every row, one descriptor per connection,
and **`FD_FREE` at idle is numerically the ceiling.** A console-less operator
can watch it count down.

Two traps, both measured, both of which an operator must be told:

- **The instrument dies at the wall.** At 142 held, `caget` returns nothing — a
  CA client needs a descriptor of its own and there are none left. You can
  watch the wall approach; you cannot read anything once you are against it.
- **`CA_REFUSED_CNT` is the wrong alarm for this wall.** It stayed **0**
  through the entire ramp. The fd wall is an `accept` failure (`ENFILE`) that
  happens *before* a client object exists, so it never reaches the refusal
  counter. `FD_FREE` is the only published number that sees this wall.

Timestamps on the target read `2014-04-14` — there is no RTC, so those are the
RTEMS epoch base and not wall clock.

---

## 5. If you are about to change the cap

- **You do not need to edit the source.** §1's `#ifndef` means a `-D` on the
  shim's compile line carries a different ceiling; that is the bisect route,
  and it is how an image with a cap other than 150 exists at all.
- Raising it above 256 crosses this BSP's `FD_SETSIZE`, which re-opens base's
  `select()` caveat for any code that multiplexes. That is the one place the
  unaudited question still matters: **libbsd's internals were never audited for
  `select()` use** (`src/lib.rs` open item 3). Below 256 it cannot bite.
- Raising it within the guest's current memory buys at most 9 connections
  (§3). Do not spend a target session re-deriving that.
- Lowering it back to base's 64 — the value base itself compiles on RTEMS 6 —
  costs most of the ceiling for no measured benefit on this BSP. *(Inferred,
  not measured: our stack has never been run at cap 64. The same `cap − 8`
  arithmetic would put the fd wall at 56 — 53 is
  **C's** number at that cap, not ours, because C holds 11 descriptors at idle
  where we hold 8.)*
- The number that would actually move the ceiling is per-connection memory,
  and the dominant term there is the two thread stacks.
