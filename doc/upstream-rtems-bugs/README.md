# Upstream bugs found during RTEMS 6 bring-up — evidence and reproduction

Four defects in other people's code, found while bringing EPICS up on RTEMS 6
under qemu. This is the single document for all four: what each one is, the
measured evidence, what a correct fix looks like where the evidence states one,
and how to reproduce it.

Two things to know before reading anything below.

**Nothing here was re-measured.** Every number — poll-call counts, CPU-idle
percentages, descriptor timings, fault registers, `addr2line` output — is quoted
from artefacts recovered off the bring-up box on **2026-07-22**, or from
`doc/rtems-scope-b-session-handoff.md` §5.3/§6. Each bug section ends with a
**Sourcing** table mapping every claim to where it came from. That discipline is
the point of the document, not decoration: no observation here was produced by
its author.

**The recovered artefacts are byte-identical and must stay that way.**
`evidence/FINDING-1-libevent-poll-spin.md`,
`evidence/FINDING-2-base-rtems-fsimage-null.md`,
`evidence/FINDING-3-per-connection-heap.md` and `evidence/DEVIATIONS.md` are
verbatim copies whose SHA-256 sums are recorded in
[Appendix B](#appendix-b--checksums-verified-on-both-ends). They must not be
edited, reflowed, moved or renamed. This document cites them; it does not
replace them, and where they contain someone's prose it is preserved rather
than rewritten.

Provenance, layout, what was deliberately left behind, and the checksum table
are in the appendices at the bottom.

---

## How the four relate

This is the most useful thing the set knows, and it is invisible if the bugs are
read one at a time.

```
bug 1  rtems-libbsd: poll() returns POLLERR on a valid IMFS FIFO   [ROOT DEFECT]
   │        breaks every libevent-based program on RTEMS 6, not only pvxs
   │
   └── bug 2  pvxs steers itself into that defect: an RTEMS-5-era
              event_config_avoid_method(conf,"kqueue") pushes libevent
              off working kqueue and onto the broken poll backend

       ⇒ fixing EITHER ONE INDEPENDENTLY makes pvxs work on RTEMS 6:
         a conforming poll() (bug 1), or not avoiding kqueue (bug 2).

bug 3  pvxs bundle/cmake/Platform/RTEMS.cmake:28 passes -specs bsp_specs
       UNRELATED to 1 and 2 — a build-system blocker. Nothing compiles at
       all, so it is hit first and hides the other two until removed.

bug 4  EPICS base epicsRtemsFSImage=NULL faults before main()
       INDEPENDENT of all of the above. Different project, different
       mechanism, no interaction with libevent or the reactor.
```

| # | owner | kind | relationship |
|---|---|---|---|
| 1 | rtems-libbsd | broken syscall primitive | root defect of the pvxs wedge |
| 2 | pvxs | RTEMS-5-era workaround, one line | victim of bug 1; either fix suffices |
| 3 | pvxs | build system, one line | independent; blocks compilation entirely |
| 4 | EPICS base | boot crash on a documented configuration | independent |

---

## Bug 1 — rtems-libbsd: `poll()` returns `POLLERR` on a valid IMFS FIFO

Handoff §6 bug 1. Root cause of the whole class.

Primary artefact:
[`evidence/FINDING-1-libevent-poll-spin.md`](evidence/FINDING-1-libevent-poll-spin.md)
(verbatim recovery), plus handoff §5.3.

### Versions the measurements were taken on

| component | version |
|---|---|
| RTEMS | 6.0.0 (`2faafecb7f9df8400fd78a1e6d9b3cf3df0eeccc`) |
| BSP | `xilinx_zynq_a9_qemu`, under qemu-system-arm, 256 MB guest |
| compiler | arm-rtems6-gcc 13.3.0 |
| EPICS base | 7.0.10, zero source patches |
| pvxs | `cc7bc72`, bundled libevent `1fe626c4` |

### The defect

`poll()` on a valid, open, non-socket descriptor returns immediately with
`POLLERR` instead of reporting the descriptor's real readiness. Measured on a
`pipe()` read end, which on this target is an RTEMS IMFS FIFO rather than a
libbsd socket.

**Which rtems-libbsd file and line owns it: not identified.** The finding names
the owner as libbsd's `poll()` and describes the mechanism — RTEMS routes
`poll()` for IMFS objects through libio, and a descriptor that libbsd's `poll()`
cannot map to a socket is flagged as an error rather than delegated — but no
libbsd source file or line was located, and none is claimed here. The defect is
stated behaviourally, from the target measurements below. The gap is left
visible rather than guessed at.

The file and line that *do* appear in this class belong to the victim, not the
owner: pvxs `src/evhelper.cpp:183`, which is [bug 2](#bug-2--pvxs-srcevhelpercpp183-the-rtems-kqueue-avoidance-is-an-rtems-51-leftover).

### Symptom as first seen

A pvxs-based IOC never reaches `iocInit`; it wedges inside `pvxsBaseRegistrar()`
at 100 % guest CPU. pvxs's own `test/testev.cpp` `test_call()` hangs identically.

What it actually is: `event_base_loop()` never blocks when libevent selects its
**poll** backend. It returns from `poll()` immediately and re-enters, at ~27 µs
per iteration.

### Measured: `--wrap=poll` counts and CPU-idle attribution

A `-Wl,--wrap=poll` interposer, applied to the probe binary only — libevent and
pvxs sources unmodified — around one 4.000 s `event_base_loop`, the base armed
with a single 1000 s timer plus a 4 s `event_base_loopexit`. IDLE is from
`rtems_cpu_usage_report()`.

| backend | `poll()` calls in 4 s | timeouts requested | guest IDLE |
|---|---:|---:|---:|
| raw `poll()` | 1 | 4000 ms | 97.9 % |
| libevent kqueue | 0 | — | 97.7 % |
| libevent **poll** | **148,081** | min 1 ms, max 4000 | **33.6 %** |

In the poll case the loop thread accounts for the missing 3.95 s of the 4.00 s
window: it burned the core for the entire duration of what the caller sees as a
correct 4 s block.

### The three-descriptor discrimination test

Measured directly on target, no libevent involved. This is what separates "a
libevent problem" from "a descriptor problem":

```
fd 55: getsockopt(SO_TYPE)=0  type=2   mode=0140666   [udp socket]
    poll(fd=55, POLLIN, 1000 ms) rv=0 revents=0x0  elapsed=1.0106 s  BLOCKS

pipe() -> read fd 56, write fd 57
fd 56: getsockopt(SO_TYPE)=-1 errno=9  mode=010600    [IMFS FIFO]
    poll(fd=56, POLLIN, 1000 ms) rv=1 revents=0x8  elapsed=0.0002 s  IMMEDIATE
                                        ^^^^^^^^^^ POLLERR

socketpair(AF_UNIX) -> 58, 59
fd 58: getsockopt(SO_TYPE)=0  type=1   mode=0140666   [unix socket]
    poll(fd=58, POLLIN, 1000 ms) rv=0 revents=0x0  elapsed=1.0058 s  BLOCKS
```

`pipe()` fails where `socketpair()` and a UDP socket both block correctly.

**Why libevent lands on the failing descriptor.**
`evutil_make_internal_pipe_()` prefers `pipe()`/`pipe2()` over `socketpair()` for
its internal notification channel. On this target `pipe()` is an IMFS FIFO, so
libevent's notify descriptor is exactly the kind `poll()` mishandles; libevent
then treats it as always ready. Inside the spinning loop the interposer records
that descriptor:

```
poll() calls=69076  last: fd=62 msec=1 rv=1 revents=0x8 errno=11
fd 62: getsockopt(SO_TYPE)=-1 errno=9  fstat=-1        [libevent notify fd]
```

`revents=0x8` is `POLLERR`. This snapshot is a *different run* from the 4 s
window above and the finding states no window for it — the 69,076 figure
identifies the descriptor, it is not a second rate measurement, and it must not
be compared against the 148,081.

### Why it presents as "a secondary thread hangs"

The spin is present on any thread; it is only *observable* when some other
thread needs the core. RTEMS POSIX threads on this BSP default to `SCHED_FIFO`
priority 1 and `pthread_create` inherits the creator's priority, so the loop
thread and its creator are equal-priority on a single core. `SCHED_FIFO` does
not time-slice between equals, so the spinner never yields:

| case | 20 × `nanosleep(100 ms)` on the other thread |
|---|---:|
| worker `nanosleep(4 s)` | 2.200 s normal |
| worker `poll(NULL,0,4000)` | 2.192 s normal |
| worker `poll(1 udp fd,4000)` | 2.194 s normal |
| worker `select(1 udp fd,4000)` | 2.195 s normal |
| worker libevent **kqueue** loop, 4 s | 2.187 s normal |
| worker libevent **poll** loop, 4 s | **6.105 s — starved for the full 4 s** |
| … + a real fd registered | 6.089 s starved |
| … + `evthread_make_base_notifiable` | 6.091 s starved |

### The priority hypothesis was tested and REJECTED as the cause

This is the part a reader will otherwise re-derive, so it is carried here rather
than left in the finding alone.

Raising the other thread above the loop thread (`main` at `SCHED_FIFO` 3, worker
explicitly at 1) makes the starvation disappear — `main` returns to 2.198 s. But
guest IDLE in that same window is **0.000074 s of 4.017 s**. The core is still
100 % consumed by the spin.

**Priority separation hides the symptom; it does not fix the defect.** This is a
broken primitive, not priority-inversion starvation. Equal-priority single-core
`SCHED_FIFO` is only why it *presents* as a hang.

Handoff §5.3 keeps two earlier explanations that were retracted under
measurement, for the same reason: *"pvxs does not run on RTEMS 6"* (it does) and
*"`event_base_loop` on a secondary pthread busy-spins"* (on a secondary pthread
the loop is fine — the thread was never the variable).

### Ownership split, and what a correct fix looks like

* **rtems-libbsd owns the defect.** `poll()` on a valid, open, non-socket
  descriptor must report the descriptor's real readiness, not `POLLERR`. A
  conforming `poll()` here would fix libevent, pvxs and any other portable
  reactor at once.
* **libevent deserves a defensive change.** `evutil_make_internal_pipe_()` could
  prefer `socketpair()` on RTEMS; `socketpair(AF_UNIX)` is measured above to poll
  correctly on this exact target. That would make the poll backend usable even on
  an unfixed libbsd.
* **pvxs is only the victim, but it holds the one-line workaround** that steers
  it into the broken backend — [bug 2](#bug-2--pvxs-srcevhelpercpp183-the-rtems-kqueue-avoidance-is-an-rtems-51-leftover),
  where the line, the end-to-end demonstration with it removed and the
  non-reproduction of the behaviour it was written for all live.

Scope limit that belongs attached to the ownership claim: one BSP
(`xilinx_zynq_a9_qemu`) at one RTEMS revision. No other BSP was tested.

### Reproduction

~90 lines of C, no pvxs: create an `event_base` with
`event_config_avoid_method(conf,"kqueue")`, add one `EV_PERSIST` 1000 s timer,
`event_base_loopexit(+4 s)`, and run `event_base_loop(base,0)` on one thread
while another does 20 × `nanosleep(100 ms)`. The second thread takes ~6 s instead
of ~2 s.

Sources are recovered at [`repro/probe/`](repro/probe/) —
`probeApp/src/probeMain.c` for the program (part A is the three-descriptor
discrimination test, part B the libevent starvation run) and
`probeApp/src/Makefile` for the `USR_LDFLAGS += -Wl,--wrap=poll` that counts the
calls. **Caveat:** the recovered `probeMain.c` is the last state left on the box,
which runs a 2 s loop with 10 × `nanosleep(100 ms)`; the tables above were taken
at 4 s and 20 × 100 ms. Those two constants must be changed back to reproduce the
published numbers verbatim. It was recovered as found, not adjusted.

Nothing was rebuilt or re-run during the recovery: what is verified about
`repro/probe/` is that it is byte-identical to the box, not that it still builds.

### Sourcing — bug 1

| claim | source |
|---|---|
| versions table | `evidence/FINDING-1-libevent-poll-spin.md` header ("Measured on: …") |
| symptom: IOC wedges in `pvxsBaseRegistrar()` at 100 % CPU; `testev.cpp` `test_call()` hangs identically | FINDING-1 "Symptom as originally seen" |
| `event_base_loop` never blocks under the poll backend; ~27 µs per iteration | FINDING-1 "What it actually is" |
| backend table — 148,081 / 1 / 0 calls, 33.6 / 97.9 / 97.7 % idle, timeouts requested | FINDING-1 "What it actually is"; same table in handoff §5.3 |
| loop thread accounts for the missing 3.95 s of 4.00 s; IDLE from `rtems_cpu_usage_report()` | FINDING-1, same section |
| three-descriptor discrimination test, with fd numbers, modes, `SO_TYPE` results, elapsed times | FINDING-1 "Root cause"; condensed in handoff §5.3 |
| `evutil_make_internal_pipe_()` prefers `pipe()`/`pipe2()`; `pipe()` here is an IMFS FIFO | FINDING-1 "Root cause" |
| interposer snapshot `calls=69076 … fd=62 revents=0x8 errno=11`; fd 62 is the libevent notify fd | FINDING-1 "Root cause" |
| that the 69,076 snapshot is a different run with no stated window | read from FINDING-1 while writing this: the figure sits outside the 4 s measurement block and FINDING-1 gives it no window |
| starvation table (2.187–2.200 s normal vs 6.105 / 6.089 / 6.091 s starved) | FINDING-1 "Why it presents as 'a secondary thread hangs'" |
| RTEMS POSIX threads default to `SCHED_FIFO` 1; `pthread_create` inherits; no time-slicing between equals | FINDING-1, same section |
| priority hypothesis tested and rejected; `SCHED_FIFO` 3 vs 1; main back to 2.198 s; guest IDLE **0.000074 s of 4.017 s** | FINDING-1 "Priority hypothesis: tested and rejected"; handoff §5.3 gives the same figures |
| the two retracted explanations (Wrong v1 / Wrong v2) | handoff §5.3 opening block |
| ownership split — libbsd owns, libevent defensive change, pvxs victim | FINDING-1 "Who owns it"; handoff §5.3 "Owner: rtems-libbsd" |
| no libbsd file/line identified; mechanism described as libio routing | FINDING-1 "Who owns it" is the only statement of mechanism and names no file or line — the absence is stated, not filled in |
| reproduction recipe (~90 lines, 4 s, 20 × 100 ms) | FINDING-1 "Reproduction" |
| recovered probe source is the last-left variant at 2 s / 10 × 100 ms | read from `repro/probe/probeApp/src/probeMain.c` during the recovery |
| nothing rebuilt or re-run; box read-only | this recovery's own constraints, [Appendix A](#appendix-a--provenance-layout-and-what-was-not-recovered) |

---

## Bug 2 — pvxs `src/evhelper.cpp:183`: the RTEMS kqueue avoidance is an RTEMS-5.1 leftover

Handoff §6 bug 2. pvxs is the victim of [bug 1](#bug-1--rtems-libbsd-poll-returns-pollerr-on-a-valid-imfs-fifo)
but holds a one-line workaround that selects the broken path.

Sources: handoff §5.3, `evidence/FINDING-1-libevent-poll-spin.md`,
`evidence/DEVIATIONS.md`, and the recovered diff.

### Versions the measurements were taken on

| component | version |
|---|---|
| RTEMS | 6.0.0 (`2faafecb7f9df8400fd78a1e6d9b3cf3df0eeccc`) |
| BSP | `xilinx_zynq_a9_qemu`, under qemu-system-arm, 256 MB guest |
| compiler | arm-rtems6-gcc 13.3.0 |
| EPICS base | 7.0.10 (`bf11a0c`), zero source patches |
| pvxs | `cc7bc72`, bundled libevent `1fe626c4` |

### The line

`pvxs/src/evhelper.cpp`, line 183 as it stands in `cc7bc72`:

```c
#ifdef __rtems__
    /* with libbsd circa RTEMS 5.1
     * TCP peer close/reset notifications appear to be lost.
     * Maybe due to absence of NOTE_EOF?
     * poll() seems to work though.
     */
    event_config_avoid_method(conf.get(), "kqueue");
#endif
```

The comment names its own vintage: *libbsd circa RTEMS 5.1*. On RTEMS 6 with
rtems-libbsd, steering libevent away from kqueue steers it into the `poll`
backend, and that backend does not work on this target — the measurement is
[bug 1's](#measured---wrappoll-counts-and-cpu-idle-attribution) 148,081 poll
calls in 4 s at 33.6 % guest idle, against 1 call at 97.9 % for raw `poll()` and
0 calls at 97.7 % for kqueue, with the failing descriptor identified as
libevent's internal `pipe()`-based notify FIFO.

### Which backends were available, and which were tested

From the actual build's
`bundle/O.RTEMS-xilinx_zynq_a9_qemu/include/event2/event-config.h`, exactly
three backends are compiled in and no more:

| macro | state |
|---|---|
| `EVENT__HAVE_KQUEUE` | `#define … 1` (and `EVENT__HAVE_WORKING_KQUEUE 1`) |
| `EVENT__HAVE_POLL` | `#define … 1` |
| `EVENT__HAVE_SELECT` | `#define … 1` |
| `EVENT__HAVE_EPOLL` | `#undef` |
| `EVENT__HAVE_DEVPOLL` | `#undef` |
| `EVENT__HAVE_EVENT_PORTS` | `#undef` |

kqueue is measured working (bug 1's table, plus peer-close detection below).
poll is measured broken.

> **The select backend was never tested. Any statement about it is a hypothesis,
> not a result.** The raw `select(1 udp fd)` in bug 1's starvation table is a
> syscall probe, not libevent's select backend, and the descriptor that breaks is
> the internal notify FIFO, not a socket. *If* libbsd's `select()` folds
> `POLLERR` into "readable" the way FreeBSD's does, it would spin identically —
> that is the hypothesis. One `avoid_method("poll")` added to the existing probe
> would settle it, and has not been run.

### End-to-end demonstration with the line removed

With that single line commented out and **nothing else changed**, the same pvxs
IOC boots to completion on the same guest:

```
iocRun: All initialization complete
```

and answers from the host over `EPICS_PVA_NAME_SERVERS=127.0.0.1:5175`:

* `pvxinfo PIOC:AI1` → full `epics:nt/NTScalar:1.0` introspection
* `pvxget PIOC:AI1` → `value double = 1.25`
* `pvxput PIOC:AO 7.5` then `pvxget PIOC:AO` → `value double = 7.5`
* `pvxmonitor PIOC:HB100` → streaming updates from a 0.1 s scan record

### The behaviour the workaround was written for does not reproduce

The comment's stated reason is lost TCP peer close/reset notification under
kqueue. Re-tested on RTEMS 6: killing the `pvxmonitor` client with `SIGKILL` — so
no FIN is sent — is detected by the kqueue backend:

```
DEBUG pvxs.tcp.io connection to Client 10.0.2.2:34704 closed by peer
DEBUG pvxs.tcp.setup Client 10.0.2.2:34704 Cleanup TCP Connection
```

So on RTEMS 6 the RTEMS-5.1 reason for avoiding kqueue did not reproduce in this
test. Scope of that statement: one monitor client, killed with `SIGKILL`, on this
BSP, at these versions. It is not a general claim about every close/reset path.

### The exact change that was made

Full diff against the preserved original:
[`patches/pvxs-evhelper.cpp.diff`](patches/pvxs-evhelper.cpp.diff). One line
disabled:

```diff
-            event_config_avoid_method(conf.get(), "kqueue");
+            /* rtems-cside panel: disabled to test kqueue on RTEMS 6 */
+            /* event_config_avoid_method(conf.get(), "kqueue"); */
```

That commented-out form is what the measurements above were taken with. It is a
measurement patch, not a proposed shape for an upstream change. `DEVIATIONS.md`
records that any pvxs measurement taken after 2026-07-22 was taken with this line
disabled.

### Reproduction

Same probe as bug 1 — [`repro/probe/`](repro/probe/), with the same caveat about
the 2 s / 10 × 100 ms constants. For the pvxs-level demonstration: build pvxs
`cc7bc72` for the RTEMS 6 target (which first requires
[bug 3's](#bug-3--pvxs-bundlecmakeplatformrtemscmake28-passes--specs-bsp_specs-which-rtems-6-does-not-install)
build fix), boot the IOC once as shipped — it wedges before `iocInit` — then
comment out `evhelper.cpp:183` and boot again.

### The claim to make, and the claim not to make

Handoff §5.3 and §8.1 are explicit about this, and the boundary is load-bearing
for our own design rationale:

* **The claim:** the reference implementation ships an RTEMS-5-era workaround
  that makes it unusable on RTEMS 6 today, and finding that took a `--wrap=poll`
  interposer plus CPU-idle attribution.
* **Not the claim:** "a reactor cannot run on RTEMS." A libevent reactor
  demonstrably does, once steered to kqueue — the demonstration is two sections
  above. Our blocking thread-per-connection backend avoids the class by not
  depending on a reactor; that is a different and weaker statement than being the
  only thing that works, and the stronger version was written into a previous
  draft and retracted.

### Sourcing — bug 2

| claim | source |
|---|---|
| versions table | `evidence/FINDING-1-libevent-poll-spin.md` header |
| the line, its text and comment | `patches/pvxs-evhelper.cpp.diff` context lines; quoted identically in FINDING-1 |
| line number 183 | diff hunk `@@ -175,17` plus 8 context lines → the removed line is 183 |
| backend counts and idle percentages | FINDING-1 and handoff §5.3 (see bug 1's sourcing row for the same table) |
| compiled-in backends kqueue/poll/select; EPOLL, DEVPOLL, EVENT_PORTS undef | handoff §5.3; re-read directly from `pvxs/bundle/O.RTEMS-xilinx_zynq_a9_qemu/include/event2/event-config.h` on the box during the recovery (lines 82, 97, 115, 176, 233, 257, 459) |
| select backend untested → hypothesis | handoff §5.3, which states this as a requirement on how it is written |
| end-to-end `iocRun` / pvxinfo / 1.25 / 7.5 / pvxmonitor | handoff §5.3 and FINDING-1 "Consequence: pvAccess does work on RTEMS 6" |
| SIGKILL peer-close detected under kqueue, with the two DEBUG log lines | FINDING-1 "Consequence"; handoff §5.3 |
| the exact one-line change | `patches/pvxs-evhelper.cpp.diff`, generated by `diff -u -U8` on the box against `patches/evhelper.cpp.orig` |
| measurements after 2026-07-22 were taken with the line disabled | `evidence/DEVIATIONS.md` lines 48-56 |
| narrow-claim / not-the-stronger-claim discipline | handoff §5.3 closing section and §8.1 item 1 |

---

## Bug 3 — pvxs `bundle/cmake/Platform/RTEMS.cmake:28` passes `-specs bsp_specs`, which RTEMS 6 does not install

Handoff §6 bug 3. Unrelated to bugs 1 and 2: a build-system blocker, hit before
either of them, and nothing compiles until it is removed.

Sources: `evidence/DEVIATIONS.md` entry 5 (lines 35-40) and the preserved
original file.

### Versions

| component | version |
|---|---|
| pvxs | `cc7bc72`, bundled libevent `1fe626c4` |
| RTEMS | 6.0.0, BSP `xilinx_zynq_a9_qemu` |
| compiler | arm-rtems6-gcc 13.3.0 |
| EPICS base | 7.0.10 (`bf11a0c`) |

### The line

`bundle/cmake/Platform/RTEMS.cmake`, lines 27-29 as shipped — the full original
file is preserved verbatim at [`patches/RTEMS.cmake.orig`](patches/RTEMS.cmake.orig):

```cmake
set(CMAKE_C_FLAGS_INIT
 "-B${RTEMS_TARGET_PREFIX}/${RTEMS_BSP}/lib/ -specs bsp_specs -qrtems ${RTEMS_BSP_C_FLAGS}"
)
```

`-specs bsp_specs` is an RTEMS-5-era flag. RTEMS 6 no longer installs that spec
file, so with it in `CMAKE_C_FLAGS_INIT` the toolchain cannot compile a single
translation unit — CMake's own compiler-identification step fails first:

```
arm-rtems6-gcc: fatal error: cannot read spec file 'bsp_specs'
```

Nothing builds; this is not a runtime problem.

### Independent checks run during the recovery

Both are read-only observations of the trees on the box, and both are consistent
with the DEVIATIONS entry:

* `find` over the whole installed RTEMS 6 toolchain (`~/rtems-cside/tools`, the
  copy of the arm-rtems6 tools with BSP `xilinx_zynq_a9_qemu`) returns **no file
  named `bsp_specs`** anywhere.
* `grep -rn bsp_specs` over the whole EPICS base 7.0.10 tree returns **nothing**,
  and base's generated pkgconfig for this target
  (`lib/pkgconfig/epics-base-RTEMS-xilinx_zynq_a9_qemu.pc:41`) carries `-qrtems`
  with no `-specs` flag:

  ```
  Libs: -L${libdir} -B…/xilinx_zynq_a9_qemu/lib -qrtems -Wl,--gc-sections … -u POSIX_Init
  ```

  So a working EPICS RTEMS 6 build in the same environment uses `-qrtems` without
  `-specs bsp_specs`. Note this corroboration is of a link-flag line in a
  *generated* pkgconfig; `-qrtems` was not found in base's `configure/` sources.

### The exact change that was made, and what it does not establish

Full diff: [`patches/pvxs-RTEMS.cmake.diff`](patches/pvxs-RTEMS.cmake.diff).
`-specs bsp_specs ` removed from line 28, nothing else:

```diff
- "-B${RTEMS_TARGET_PREFIX}/${RTEMS_BSP}/lib/ -specs bsp_specs -qrtems ${RTEMS_BSP_C_FLAGS}"
+ "-B${RTEMS_TARGET_PREFIX}/${RTEMS_BSP}/lib/ -qrtems ${RTEMS_BSP_C_FLAGS}"
```

This is the whole deviation for bug 3. **No pvxs C++ source was patched for it**
— the `src/evhelper.cpp` change is bug 2, a separate and independent deviation.

What is demonstrated: removing the flag lets pvxs build for RTEMS 6 with this
toolchain and BSP, and the resulting image boots and serves (bug 2's end-to-end
section). **Whether RTEMS 5 still needs the flag was not tested** — no RTEMS 5
toolchain was present — so nothing here says what the conditional form of a fix
should be.

### Reproduction

Configure pvxs `cc7bc72` for an RTEMS 6 target
(`CROSS_COMPILER_TARGET_ARCHS = RTEMS-xilinx_zynq_a9_qemu`, `RTEMS_VERSION = 6`,
`RTEMS_BASE` pointing at an arm-rtems6 install, `RTEMS_BSD_NETWORKING = yes`) and
build the bundled libevent. CMake fails at compiler identification with the
`cannot read spec file 'bsp_specs'` error above. The site configuration used is
recorded in [`evidence/DEVIATIONS.md`](evidence/DEVIATIONS.md) entries 1, 2 and 6.

### Sourcing — bug 3

| claim | source |
|---|---|
| the file, line 28 and its full text | `patches/RTEMS.cmake.orig` (verbatim copy, checksum verified) |
| RTEMS 6 does not install `bsp_specs`; `-qrtems` alone is what base uses | `evidence/DEVIATIONS.md` lines 35-40 |
| exact compiler error string | `evidence/DEVIATIONS.md` line 39 |
| the one-line change | `patches/pvxs-RTEMS.cmake.diff`, generated by `diff -u` on the box against the preserved original |
| "No pvxs C++ source was patched" for this item | `evidence/DEVIATIONS.md` line 40 |
| `bsp_specs` absent from the toolchain and from base; base pkgconfig uses `-qrtems`; `-qrtems` not found in base's `configure/` | read-only `find`/`grep` over `~/rtems-cside/tools` and `~/rtems-cside/base` during the recovery |
| build/site configuration used | `evidence/DEVIATIONS.md` entries 1, 2, 6 |
| nothing compiles without the change | handoff §6 bug 3; `DEVIATIONS.md` line 38 |
| RTEMS 5 behaviour untested | no RTEMS 5 toolchain was present on the box; stated as a limit rather than assumed either way |

---

## Bug 4 — EPICS base: `epicsRtemsFSImage = NULL` faults through `set_directory(NULL)` before `main()`

Handoff §6 bug 4. A boot crash on base's own documented configuration,
independent of bugs 1-3.

Primary artefact:
[`evidence/FINDING-2-base-rtems-fsimage-null.md`](evidence/FINDING-2-base-rtems-fsimage-null.md)
(verbatim recovery).

### Two properties that make this report load-bearing

* **Base was UNPATCHED.** No `.c`, `.cpp` or `.h` in base was touched to produce
  the crash; `evidence/DEVIATIONS.md` records base as carrying zero source
  patches, site configuration only.
* **The whole application is 12 lines.** The reproducer is a `PROD_IOC` whose
  entire content is one documented declaration and a `printf`.

Together: the crash is reachable from base as shipped, by an application that
does nothing.

### Versions

| component | version |
|---|---|
| EPICS base | 7.0.10 (`bf11a0c`), `modules/libcom/RTEMS/posix/rtems_init.c` |
| RTEMS | 6.0.0, BSP `xilinx_zynq_a9_qemu` |
| compiler | arm-rtems6-gcc 13.3.0 |

### The configuration that crashes

`rtems_init.c` documents three states for `epicsRtemsFSImage`. The middle one is
the "I do not need a filesystem" declaration:

```c
205  const epicsMemFS *epicsRtemsFSImage __attribute__((weak));
206  const epicsMemFS *epicsRtemsFSImage = (void*)&epicsRtemsFSImage;
...
212  epicsRtemsMountLocalFilesystem(char **argv)
213  {
214      if(epicsRtemsFSImage==(void*)&epicsRtemsFSImage)
215          return -1; /* no FS image provided. */
216      else if(epicsRtemsFSImage==NULL)
217          return 0;  /* no FS image provided, but none is needed. */
218      else {
...
224              argv[1] = "/";
225              return 0;
```

Taking that documented path kills the guest before `main()` runs.

### The chain, by line number

All in `modules/libcom/RTEMS/posix/rtems_init.c`:

| line | what happens |
|---|---|
| **948** | `POSIX_Init` declares the startup argument vector, all NULL: `char *argv[3] = { NULL, NULL, NULL };` |
| 1127 | `initialize_remote_filesystem(argv, initialize_local_filesystem(argv));` |
| **238** | `initialize_local_filesystem` calls the weak hook and, on `0`, reports success: `if (epicsRtemsMountLocalFilesystem(argv)==0) return 1;` |
| **216-217** | the hook returns `0` for the `NULL` image **without assigning `argv[1]`**. Every other successful path assigns it — 224 (`argv[1]="/"`), 256, 315, 339, 366, 411 |
| 293 | `initialize_remote_filesystem(argv, hasLocalFilesystem=1)` guards all of its own `argv[1] = …` assignments with `if (!hasLocalFilesystem)`, so it correctly leaves it alone |
| **1164** | `set_directory (argv[1]);` — `argv[1]` is still `NULL` |
| **471** | inside `set_directory`: `cp = strrchr(commandline, '/');` → fault |

`set_directory` as it stands:

```c
465  set_directory (const char *commandline)
466  {
467      const char *cp;
...
471      cp = strrchr(commandline, '/');
472      if (cp == NULL) {          /* handles "no slash", not "no string" */
```

Line 1165 `epicsEnvSet ("IOC_STARTUP_SCRIPT", argv[1])` and line 1184
`result = main (…, argv)` are both downstream of the fault and would also need to
cope with a `NULL` `argv[1]`.

### Why this is a defect and not a misuse

1. **The guard at 472 handles a path with no slash, but not the absence of a
   path** — and the absence of a path is precisely what "no filesystem is needed"
   means. The author anticipated `"st.cmd"` without a directory; the documented
   NULL-image configuration produces no string at all.
2. **The `NULL` branch is the only success path that does not assign `argv[1]`.**
   Lines 224, 256, 315, 339, 366 and 411 all assign it; 216-217 returns success
   without honouring the invariant that 238 then reports upward.

The contract — a successful `initialize_local_filesystem` leaves a usable startup
path in `argv[1]` — is established in six places, broken in one, and the consumer
is not total against the difference.

### What was observed on target

```
***** Setting up file system *****
***** Initializing NFS *****
 check for time registered , C++ initialization ...
***** Preparing EPICS application *****

*** FATAL ***
fatal source: 9 (RTEMS_FATAL_SOURCE_EXCEPTION)

R0   = 0x00000000 R8  = 0x00000010
R1   = 0x0000002f R9  = 0x00000000
...
PC  = 0x002dc238
```

`R0 = 0` is the NULL string argument; `R1 = 0x2f` is `'/'`. Resolving the PC:

```
$ arm-rtems6-addr2line -f -e fsbug.exe 0x002dc238
strchr
newlib/libc/string/strchr.c:100
```

reached from `strrchr` (`0x002dca98` in the same image; newlib's `strrchr`
tail-calls `strchr`). `main()` is never entered.

### What a correct fix looks like

The finding gives two options and says to do both. Quoted, not re-derived:

**A. Honour the invariant at the branch that breaks it** (`rtems_init.c:216-217`)
— preferable, because it keeps the invariant with the code that establishes it:

```c
    else if(epicsRtemsFSImage==NULL) {
        argv[1] = "/";   /* no image needed; run from the root of the IMFS */
        return 0;        /* no FS image provided, but none is needed. */
    }
```

**B. Make the consumers total**, so no future path can reintroduce it:

```c
    /* 1164 */
    if (argv[1] == NULL)
        argv[1] = "/";
    set_directory (argv[1]);
```

and/or make `set_directory` accept NULL by treating it exactly as the existing
"no slash" case:

```c
    cp = commandline ? strrchr(commandline, '/') : NULL;
```

**Doing both A and B is cheap and leaves no configuration that can fault.**

Consequence the finding attaches to either fix: `IOC_STARTUP_SCRIPT` becomes
`"/"`, and `main()` is reached with `argv[1] == "/"`. An application that declared
it needs no filesystem is by definition not going to read a script from it, so
that is consistent with the documented intent.

### Minimal reproduction

The entire application:

```c
#include <stdio.h>
#include <epicsMemFs.h>

const epicsMemFS *epicsRtemsFSImage = NULL;   /* documented: none is needed */

int main(int argc, char **argv)
{
    printf("FSBUG: main() reached -- bug is NOT present\n");
    return 0;
}
```

Link as a normal `PROD_IOC` for an RTEMS target and boot. The success message
never prints; the fault above appears instead.

**The reproducer sources are already recovered in-tree** at
[`repro/fsbug/`](repro/fsbug/) — `fsbugApp/src/fsbugMain.c` plus the app and
`configure/` makefiles, byte-identical to the box and checksummed in
[Appendix B](#appendix-b--checksums-verified-on-both-ends).
`configure/RELEASE.local` carries the absolute paths from the box
(`/home/coding-agent/rtems-cside/…`) and must be re-pointed to build elsewhere;
it also names `PVXS`, which this application does not use. Build artefacts and
the 30 MB `fsbug.exe` were deliberately not recovered, so the `addr2line`
resolution above cannot be re-run against that exact image.

### Workaround used on the box meanwhile

Define the weak hook in the application and assign `argv[1]` there:

```c
int epicsRtemsMountLocalFilesystem(char **argv) { argv[1] = "/"; return 0; }
```

An override of a base hook, not a patch to base — which is how base stayed at
zero source patches while bring-up continued.

### Sourcing — bug 4

| claim | source |
|---|---|
| versions, and that base is unpatched | `evidence/FINDING-2-base-rtems-fsimage-null.md` header; base's zero-source-patch status also in `evidence/DEVIATIONS.md` §"EPICS base R7.0.10 … ZERO source patches" |
| "the whole application is 12 lines" | FINDING-2 header; the recovered `repro/fsbug/fsbugApp/src/fsbugMain.c` is that application |
| the three documented states and the quoted 205-225 block | FINDING-2 "The supported configuration that crashes" |
| chain lines 948, 1127, 238, 216-217, 293, 1164, 471 and their content | FINDING-2 "The exact NULL and how it reaches `strrchr`"; same table in handoff §6 bug 4 |
| every other success path assigns `argv[1]` — 224, 256, 315, 339, 366, 411 | FINDING-2, same section |
| the 472 guard handles "no slash", not "no string" | FINDING-2, same section; handoff §6 |
| lines 1165 and 1184 are downstream and would also need a NULL-safe `argv[1]` | FINDING-2, same section |
| boot output, `RTEMS_FATAL_SOURCE_EXCEPTION`, `R0 = 0x00000000`, `R1 = 0x0000002f`, `PC = 0x002dc238` | FINDING-2 "Minimal reproduction"; register values also in handoff §6 |
| `addr2line` → `strchr`, `newlib/libc/string/strchr.c:100`, reached from `strrchr` at `0x002dca98` | FINDING-2, same section |
| fix options A and B, and "doing both leaves no faulting configuration" | FINDING-2 "What a correct fix looks like"; handoff §6 states the same pair |
| `IOC_STARTUP_SCRIPT` becomes `"/"` under either fix | FINDING-2, same section |
| the weak-hook workaround | FINDING-2 "Workaround used by this panel meanwhile" |
| reproducer sources recovered at `repro/fsbug/` | [Appendix B](#appendix-b--checksums-verified-on-both-ends) and the files themselves |
| `fsbug.exe` and build artefacts not recovered | [Appendix A](#appendix-a--provenance-layout-and-what-was-not-recovered) |

---

## Measurements taken on the box, which are **not** upstream bugs

* [`measurement-c-thread-priority-on-rtems-6.md`](measurement-c-thread-priority-on-rtems-6.md)
  — **added 2026-07-22, after the recovery.** Does EPICS base's own thread
  ladder take effect on RTEMS 6? Measured: **yes**, on one boot of
  `cioc-fd64.exe` (stock 64-descriptor base configuration, zero base source
  patches). Closes handoff §8.0 gap 1. It also establishes, by measurement
  rather than by reading `configure/toolchain.c`, that an RTEMS 6 build of base
  uses the **POSIX** arm's linear map, and records where libbsd's own threads
  sit relative to `CAS-TCP`. Its evidence is the complete unedited console
  transcript in `evidence/`, and its drivers are in `repro/priority/`. Unlike
  everything else in this directory, this file and its evidence were produced
  by *running* the box rather than by copying from it.
* [`measurement-c-event-leak-bytes-on-rtems-6.md`](measurement-c-event-leak-bytes-on-rtems-6.md)
  — **added 2026-07-23.** How many bytes does the
  [`epicsEventDestroy` leak](c-base-rtems-posix-event-leak.md) actually cost?
  Measured on the same stock `cioc-fd64.exe`: **160 bytes and exactly 5 heap
  blocks per CA client connect/disconnect cycle**, reproduced across two boots
  and four batches, with **32 bytes per leaked block** measured independently.
  Closes that document's first "Not measured / open" bullet. The heap instrument
  is the RTEMS shell's own `malloc` command reached through base's `rt` bridge,
  so the image carrying every per-cycle number is unmodified; a second image
  carrying one added iocsh command contributes only the per-block size, and is
  declared as a deviation. Evidence is three unedited console logs in
  `evidence/`; drivers are in `repro/evleak/`.

## Also recovered, and NOT an upstream bug

* [`evidence/FINDING-3-per-connection-heap.md`](evidence/FINDING-3-per-connection-heap.md)
  — a measurement write-up of where the C IOC's per-connection heap goes. It is
  kept for the same reason as the rest: **it opens by retracting an earlier
  invalid comparison of its own** — a per-connection residual compared across two
  different builds and two different boots — and a document that hides its own
  corrections is how the next session re-derives the wrong version. The retraction
  is its section "Correction to my earlier report"; it must survive any future
  edit. (`FINDING-1` carries a retraction too: the priority hypothesis, above.)
* [`evidence/DEVIATIONS.md`](evidence/DEVIATIONS.md) — the declared-deviation log
  for everything done on the box: which upstream trees were patched (two lines,
  both pvxs), which were not (base: zero source patches), what was site
  configuration rather than source, and that no packages were installed and
  `sudo` was never used. It is also the only original home of bug 3.

---

## Appendix A — provenance, layout, and what was not recovered

**Provenance.** Recovered **2026-07-22** from
`coding-agent@192.168.2.128:~/rtems-cside/`, which is not under version control
and has no backup. Before the recovery the evidence behind handoff §6 existed as
a single copy on that desktop, and §8.2's premise ("each already has its
evidence; what is missing is the prose") depended entirely on it surviving.

Nothing on the box was written, moved or modified; no build was run and no qemu
was started. Every file was copied out read-only and verified byte-identical by
SHA-256 on both ends ([Appendix B](#appendix-b--checksums-verified-on-both-ends)).

**Why this directory rather than `doc/upstream-c-bugs.md`.** That file is the
catalogue of reference defects found by *parity reading*, one `CBUG-` section per
entry. These four were found by *running on a target*, and their evidence is boot
logs, syscall-interposer counts, CPU-idle attribution, two buildable reproducer
trees and two build patches — which does not fit as inline sections.
`doc/upstream-c-bugs.md` is where to cross-reference this from if the two are
ever unified; this work deliberately did not edit it. Numbering follows handoff
§6 (bugs 1-4); no new `CBUG-`-style IDs were invented.

**Layout.**

```
doc/upstream-rtems-bugs/
├── README.md     this file — all four bugs, plus these appendices
├── c-base-rtems-posix-event-leak.md              evidence package: the epicsEventDestroy leak
├── measurement-c-thread-priority-on-rtems-6.md   handoff §8.0 gap 1, measured on a boot 2026-07-22
├── measurement-c-event-leak-bytes-on-rtems-6.md  bytes/cycle for that leak, measured 2026-07-23
├── evidence/     four markdown artefacts recovered byte-for-byte, unedited, plus
│   │             four console logs produced by *running* the box rather than copying from it
│   ├── FINDING-1-libevent-poll-spin.md        bug 1 (and the pvxs consequence)
│   ├── FINDING-2-base-rtems-fsimage-null.md   bug 4
│   ├── FINDING-3-per-connection-heap.md       not a bug; carries its own retraction
│   ├── DEVIATIONS.md                          declared deviations; sole original home of bug 3
│   │                                          (re-copied 2026-07-23 with its session-4 section)
│   ├── c-thread-priority-boot-console-2026-07-22.log   the priority boot's whole console
│   ├── cioc-evleak-2026-07-23.log             event-leak run 1 (stock image)
│   ├── cioc-evleak-repeat-evmon-2026-07-23.log  run 2 + the monitor control (stock image)
│   ├── cioc-evloop-2026-07-23.log             per-block size (variant image)
│   └── .gitattributes                         `*.log -text`; the logs are CR LF and checksummed
├── patches/      the one-line change to each modified upstream file
│   ├── RTEMS.cmake.orig            pristine upstream file (1,781 B), verbatim
│   ├── pvxs-RTEMS.cmake.diff       bug 3, one line
│   └── pvxs-evhelper.cpp.diff      bug 2, one line, ±8 lines of context
└── repro/
    ├── fsbug/    bug 4 — the 12-line application that faults unpatched base at boot
    ├── probe/    bugs 1-2 — the poll-spin probe and its --wrap=poll link flag
    ├── priority/ the six host-side CA load drivers behind the priority measurement
    └── evleak/   the event-leak drivers, plus the one app source the variant image adds
```

**Reproducer notes.** `repro/fsbug/` is a `PROD_IOC` whose entire application
content is `const epicsMemFS *epicsRtemsFSImage = NULL;` plus a `printf`.
`repro/probe/probeMain.c` does two things: part A polls a UDP socket, a `pipe()`
read end and an `AF_UNIX` `socketpair()` for 1000 ms each and reports which block
and which return immediately; part B runs a libevent loop with
`event_config_avoid_method(conf,"kqueue")` on one thread while the main thread
sleeps, reporting the starvation, the `__wrap_poll` call count and the offending
descriptor. It needs `USR_LDFLAGS += -Wl,--wrap=poll`, which is in the recovered
`probeApp/src/Makefile`. Neither reproducer was rebuilt or re-run: what is
verified is that the sources are byte-identical to the box, not that they still
build.

**What was deliberately not recovered.**

* All `.exe` images (`cioc.exe` 37 MB, `pioc.exe` 70 MB, `fsbug.exe` 30 MB,
  `probe.exe`, `testev.exe`, …) and every other binary — `O.<arch>/` build
  directories, `bin/`, `.o`, `.d`.
* The EPICS base and pvxs source trees, and the copied RTEMS toolchain.
* `patches/evhelper.cpp.orig` — 31 KB of pristine upstream pvxs source. The diff
  carries the one-line change with 8 lines of context on each side, which is what
  the recovery was asked for; the full original is a slice of the pvxs tree and
  stays out. Its checksum is in Appendix B so the diff stays re-verifiable.
* Boot logs, the `ceiling*.py` drivers, `boot-*.sh`, and the `cioc`/`pioc`
  application trees. These back handoff §5.1/§5.6/§5.7 rather than these four
  bugs, and were out of scope — **they are still single-copy on the box.**
  (Partial exception since 2026-07-22: the *one* boot log and the *six* drivers
  behind the priority measurement are now in `evidence/` and `repro/priority/`.
  `ceiling*.py`, `boot-*.sh` and the application trees remain single-copy.)

**Beyond the originally requested set.** `repro/probe/` was recovered although
the recovery brief listed only the `fsbug` sources, because FINDING-1's
"Reproduction" cites `probeApp/src/probeMain.c` by path as the reproducer for the
root-cause bug and `DEVIATIONS.md` line 67 cites its `Makefile` for the
`-Wl,--wrap=poll` interposer behind the 148,081-call figure. Without them the
recovered FINDING-1 points at nothing.

## Appendix B — checksums (verified on both ends)

SHA-256, computed on `192.168.2.128` under `~/rtems-cside/` and again on the copy
in this repository. All 23 verbatim files matched; the two generated diffs were
checksummed against the same `diff -u` run piped through `sha256sum` on the box.

**These sums are the reason `evidence/` must not be edited** — reflowing or
renaming any of those four files breaks verification against the source of truth.

**One row has changed twice since the recovery, both times for the same reason.**
`evidence/DEVIATIONS.md` is re-copied from the box whenever a measurement session
appends its own section to the box's file. It went `8c3d8109…fbb440` (recovered)
→ `bcd3a3ed…b00b44` (2026-07-22, "Session 3 additions") →
`796e0973…ab96a5d` (2026-07-23, "Session 4 additions"), which is the row below.
Earlier content is unchanged in each step; each new section is appended after it.
The files added by those measurements carry their own checksum tables, in
[`measurement-c-thread-priority-on-rtems-6.md`](measurement-c-thread-priority-on-rtems-6.md)
and
[`measurement-c-event-leak-bytes-on-rtems-6.md`](measurement-c-event-leak-bytes-on-rtems-6.md).

| file in this directory | source on the box | sha256 |
|---|---|---|
| `evidence/FINDING-1-libevent-poll-spin.md` | `FINDING-1-libevent-poll-spin.md` | `ed161ccb162b361e381ea85cbbb094b1d3d20866791e9935e20f7804af2255d6` |
| `evidence/FINDING-2-base-rtems-fsimage-null.md` | `FINDING-2-base-rtems-fsimage-null.md` | `2ad0e20893c5a45a75ed4b0e4a8c3f1d3726683cc3388a233de9c13788d73222` |
| `evidence/FINDING-3-per-connection-heap.md` | `FINDING-3-per-connection-heap.md` | `18c9fd69a48efc7123a959770fea6e48744bde3a7553a6c8235d5a31f21f02ff` |
| `evidence/DEVIATIONS.md` | `DEVIATIONS.md` | `796e097350fe4a933dd996454677a5c600c5a9800a03abb06123133dbab96a5d` |
| `patches/RTEMS.cmake.orig` | `patches/RTEMS.cmake.orig` | `8c66cd3b8cd51d6153d37f00f95d4db7dd406a9d12bf908a87251620c365280b` |
| `patches/pvxs-RTEMS.cmake.diff` | generated: `diff -u patches/RTEMS.cmake.orig pvxs/bundle/cmake/Platform/RTEMS.cmake` | `3a9adff2382b1ca5a03a70e97a1d49e1e3f489a9414371712e2138b931836210` |
| `patches/pvxs-evhelper.cpp.diff` | generated: `diff -u -U8 patches/evhelper.cpp.orig pvxs/src/evhelper.cpp` | `90303375b1857a54c605edc6ab373f50834f25da6e050b9a73c10e0406053c0d` |
| `repro/fsbug/Makefile` | `fsbug/Makefile` | `453021b7fdbc413c8ea8b1fc5d87ba425140159f37fbdd0b78efa393c2e67405` |
| `repro/fsbug/fsbugApp/Makefile` | `fsbug/fsbugApp/Makefile` | `960535bc161314e71fb4007bbff708cb556647b9299e605ad8f6d66f41f1ed53` |
| `repro/fsbug/fsbugApp/src/Makefile` | `fsbug/fsbugApp/src/Makefile` | `eb8accc5f3358331ac6579216e6f358cf36c7cabc4d93e3eebedc2f089065287` |
| `repro/fsbug/fsbugApp/src/fsbugMain.c` | `fsbug/fsbugApp/src/fsbugMain.c` | `bd479a38b4d1f24a19910ff0b3f1f4072815378eca79787ef9df1c514d2b2bcf` |
| `repro/fsbug/configure/CONFIG` | `fsbug/configure/CONFIG` | `3635f21380223cd15382aeb0eb4d933c106154cbc31880545de6dce518da9798` |
| `repro/fsbug/configure/CONFIG_SITE` | `fsbug/configure/CONFIG_SITE` | `418af52fa673118c478f683d0c3be1f3d4fd6c931193856ccae796fcee45e818` |
| `repro/fsbug/configure/Makefile` | `fsbug/configure/Makefile` | `bc53a6ada3ae3b03f32937725c222302b6adb1b512a4143cd5df693648658a22` |
| `repro/fsbug/configure/RELEASE` | `fsbug/configure/RELEASE` | `695ce8a3e3b337835cc51afa05687845c689017bb281669f909b4ac3c5e21848` |
| `repro/fsbug/configure/RELEASE.local` | `fsbug/configure/RELEASE.local` | `53b45695c1a4d40efa3a4858308604a27b6dcfb52a49960797f767772858013f` |
| `repro/fsbug/configure/RULES` | `fsbug/configure/RULES` | `992ac6dd3999917753508325192fba69dc0ee6b90efbb944924643e50d8fef02` |
| `repro/fsbug/configure/RULES_DIRS` | `fsbug/configure/RULES_DIRS` | `17509b755fe4d3a24bb20116294211c8d1fafac5705e30adf8d93aac98759490` |
| `repro/fsbug/configure/RULES.ioc` | `fsbug/configure/RULES.ioc` | `83aa64637499c7a09508b8c4cfbcd4b9c0e6e280b151d47674603603ac65511a` |
| `repro/fsbug/configure/RULES_TOP` | `fsbug/configure/RULES_TOP` | `1152876c06c70ce615f8d769b13d80234b87acde2116c96c25a27d295a07e404` |
| `repro/probe/Makefile` | `probe/Makefile` | `c06b2429868f2e360f15edfc8cd7932b2a4b6b4ce4b06740cb905d478b08785f` |
| `repro/probe/configure/Makefile` | `probe/configure/Makefile` | `bc53a6ada3ae3b03f32937725c222302b6adb1b512a4143cd5df693648658a22` |
| `repro/probe/probeApp/Makefile` | `probe/probeApp/Makefile` | `960535bc161314e71fb4007bbff708cb556647b9299e605ad8f6d66f41f1ed53` |
| `repro/probe/probeApp/src/Makefile` | `probe/probeApp/src/Makefile` | `8c8bbc6238cc9994fb6e9de45f9c102e60fddc98b03660f2adcbf2d7bbcfb704` |
| `repro/probe/probeApp/src/probeMain.c` | `probe/probeApp/src/probeMain.c` | `5701b8cfb959544ebc12a3aca0fc7a1d3c94eafa05a3a913c0fa59a852ed5706` |

Recorded but **not** copied here, so the diffs can be re-verified later:

| file on the box | sha256 |
|---|---|
| `patches/evhelper.cpp.orig` (pristine pvxs `cc7bc72`) | `13dce3fbdedf279c3594323a4b5307e6df487ac8fa1daacc60a08b038f19b7fd` |
| `pvxs/src/evhelper.cpp` (as modified on the box) | `9b9d256ed86be2bba5264bc9e278302febe38e9c75489a71dcf6c6f4b7487328` |
| `pvxs/bundle/cmake/Platform/RTEMS.cmake` (as modified on the box) | `c49cb5d42ff3a274544e3c889db83f1f122c49f047034b6ff20f3f4e6750d5bd` |

## Appendix C — two constraints this material is written under

1. **Nothing here is prose for another project's issue tracker.** This is
   evidence and reproduction for this repository. No part of this document is, or
   should become, a drafted issue body, a PR description, or text addressed to a
   maintainer. Where a recovered artefact already contains someone's prose, it is
   preserved verbatim rather than rewritten.
2. **The claim about pvxs stays narrow.** Handoff §5.3/§8.1: *the reference
   implementation ships an RTEMS-5-era workaround that makes it unusable on
   RTEMS 6 today* — never *a reactor cannot run on RTEMS*, because a libevent
   reactor demonstrably does once steered to kqueue. Relatedly, the libevent
   **select** backend was never tested, and every statement about it is labelled a
   hypothesis.
