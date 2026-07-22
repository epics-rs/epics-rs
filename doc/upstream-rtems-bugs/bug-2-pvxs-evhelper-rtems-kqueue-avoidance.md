# Bug 2 — pvxs `src/evhelper.cpp:183`: the RTEMS kqueue avoidance is an RTEMS-5.1 leftover

Handoff numbering: `doc/rtems-scope-b-session-handoff.md` §6 bug 2. This bug had
no document of its own; on the box it existed only as one paragraph inside
`FINDING-1-libevent-poll-spin.md` ("pvxs is only the victim, but it holds the
one-line fix") and two lines of `DEVIATIONS.md`. This file is assembled from
those two plus handoff §5.3, and every statement is attributed in
[Sourcing](#sourcing) below. Nothing here is inferred beyond its source.

## Versions the measurements were taken on

| component | version |
|---|---|
| RTEMS | 6.0.0 (`2faafecb7f9df8400fd78a1e6d9b3cf3df0eeccc`) |
| BSP | `xilinx_zynq_a9_qemu`, under qemu-system-arm, 256 MB guest |
| compiler | arm-rtems6-gcc 13.3.0 |
| EPICS base | 7.0.10 (`bf11a0c`), zero source patches |
| pvxs | `cc7bc72`, bundled libevent `1fe626c4` |

## The line

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
rtems-libbsd, steering libevent away from kqueue steers it into its `poll`
backend, and that backend does not work on this target.

## What the poll backend does on RTEMS 6

Measured with a `-Wl,--wrap=poll` interposer applied to the probe binary only —
libevent and pvxs sources unmodified for this measurement — around one 4.000 s
`event_base_loop`, the base armed with a single 1000 s `EV_PERSIST` timer plus a
4 s `event_base_loopexit`. Guest IDLE is from `rtems_cpu_usage_report()`.

| backend | `poll()` calls in 4 s | guest IDLE |
|---|---:|---:|
| raw `poll()` | 1 | 97.9 % |
| libevent kqueue | 0 | 97.7 % |
| libevent **poll** | **148,081** | **33.6 %** |

The caller sees a correct 4 s block; the loop thread burns the core for all of
it, at ~27 µs per iteration.

### The descriptor it spins on

Isolated on target with no libevent involved:

```
pipe() -> fd 56   [IMFS FIFO]   poll(POLLIN,1000ms) rv=1 revents=0x8 in 0.0002s  <- POLLERR
socketpair()-> 58 [unix socket] poll(POLLIN,1000ms) rv=0 revents=0x0 in 1.0058s  <- blocks
udp socket -> 55                poll(POLLIN,1000ms) rv=0 revents=0x0 in 1.0106s  <- blocks
```

`revents=0x8` is `POLLERR`. libevent's `evutil_make_internal_pipe_()` prefers
`pipe()` over `socketpair()`; `pipe()` on this target is an RTEMS IMFS FIFO, and
rtems-libbsd's `poll()` flags it as an error rather than waiting on it. libevent
therefore treats its own notify descriptor as permanently ready.

**The defect itself is owned by rtems-libbsd — that is
[bug 1](evidence/FINDING-1-libevent-poll-spin.md), a separate report.** pvxs is
the victim; what pvxs holds is the one line that selects the broken path.

## Which backends were available, and which were tested

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

kqueue is measured working (table above, plus peer-close detection below). poll
is measured broken.

> **The select backend was never tested. Any statement about it is a hypothesis,
> not a result.** The raw `select(1 udp fd)` that appears in FINDING-1's
> starvation table is a syscall probe, not libevent's select backend, and the
> descriptor that breaks is the internal notify FIFO, not a socket. *If*
> libbsd's `select()` folds `POLLERR` into "readable" the way FreeBSD's does, it
> would spin identically — that is the hypothesis. One `avoid_method("poll")`
> added to the existing probe would settle it, and has not been run.

## End-to-end demonstration with the line removed

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

## The behaviour the workaround was written for does not reproduce

The comment's stated reason is lost TCP peer close/reset notification under
kqueue. Re-tested on RTEMS 6: killing the `pvxmonitor` client with `SIGKILL` —
so no FIN is sent — is detected by the kqueue backend:

```
DEBUG pvxs.tcp.io connection to Client 10.0.2.2:34704 closed by peer
DEBUG pvxs.tcp.setup Client 10.0.2.2:34704 Cleanup TCP Connection
```

So on RTEMS 6 the RTEMS-5.1 reason for avoiding kqueue did not reproduce in this
test. Scope of that statement: one monitor client, killed with `SIGKILL`, on this
BSP, at these versions. It is not a general claim about every close/reset path.

## The exact change that was made

Full diff against the preserved original:
[`patches/pvxs-evhelper.cpp.diff`](patches/pvxs-evhelper.cpp.diff). One line
disabled:

```diff
-            event_config_avoid_method(conf.get(), "kqueue");
+            /* rtems-cside panel: disabled to test kqueue on RTEMS 6 */
+            /* event_config_avoid_method(conf.get(), "kqueue"); */
```

The commented-out form is what the measurements above were taken with. It is a
measurement patch, not a proposed shape for an upstream change.

## Reproduction

Without pvxs, ~90 lines of C: create an `event_base` with
`event_config_avoid_method(conf,"kqueue")`, add one `EV_PERSIST` 1000 s timer,
`event_base_loopexit(+4 s)`, and run `event_base_loop(base,0)` on one thread
while another thread does 20 × `nanosleep(100 ms)`. The second thread takes ~6 s
instead of ~2 s.

The recovered source is [`repro/probe/`](repro/probe/) — `probeApp/src/probeMain.c`
for the program, `probeApp/src/Makefile` for the `USR_LDFLAGS += -Wl,--wrap=poll`
that counts the calls. **Caveat on that file:** it is the last state left on the
box, which runs a 2 s loop with 10 × `nanosleep(100 ms)`; FINDING-1's tables were
taken at 4 s and 20 × 100 ms. The two constants must be changed back to
reproduce the published numbers verbatim. Recovered as found, not adjusted.

## The claim to make, and the claim not to make

Handoff §5.3 and §8.1 are explicit about this, and the boundary is load-bearing
for our own design rationale:

* **The claim:** the reference implementation ships an RTEMS-5-era workaround
  that makes it unusable on RTEMS 6 today, and finding that took a `--wrap=poll`
  interposer plus CPU-idle attribution.
* **Not the claim:** "a reactor cannot run on RTEMS." A libevent reactor
  demonstrably does, once steered to kqueue — the section above is that
  demonstration. Our blocking thread-per-connection backend avoids the class by
  not depending on a reactor; that is a different and weaker statement than
  being the only thing that works, and the stronger version was written into a
  previous draft and retracted.

## Sourcing

Every claim above, with where it comes from. No claim in this document is
unsourced.

| claim | source |
|---|---|
| versions table | `evidence/FINDING-1-libevent-poll-spin.md` header |
| the line, its text and comment | `patches/pvxs-evhelper.cpp.diff` (context lines); quoted identically in FINDING-1 |
| line number 183 | diff hunk `@@ -175,17` + 8 context lines → the removed line is 183 |
| backend comparison table (148,081 / 1 / 0; 33.6 / 97.9 / 97.7 %) | handoff §5.3 table; same table in FINDING-1 |
| three-descriptor discrimination test | handoff §5.3 block; fuller form in FINDING-1 |
| `evutil_make_internal_pipe_()` prefers `pipe()` | FINDING-1 "Root cause" |
| compiled-in backends kqueue/poll/select, EPOLL/DEVPOLL/EVENT_PORTS undef | handoff §5.3; re-read directly from `pvxs/bundle/O.RTEMS-xilinx_zynq_a9_qemu/include/event2/event-config.h` on the box during this recovery (lines 82, 97, 115, 176, 233, 257, 459) |
| select backend untested → hypothesis | handoff §5.3, stated there as a requirement on how it is written |
| end-to-end pvxinfo / 1.25 / 7.5 / pvxmonitor | handoff §5.3 and FINDING-1 "Consequence" |
| SIGKILL peer-close detected under kqueue, with log lines | FINDING-1 "Consequence"; handoff §5.3 |
| the exact one-line change | `patches/pvxs-evhelper.cpp.diff`, generated by `diff -u` on the box against `patches/evhelper.cpp.orig` |
| measurements after 2026-07-22 were taken with the line disabled | `evidence/DEVIATIONS.md` lines 48-56 |
| reproduction recipe (~90 lines, 4 s, 20 × 100 ms) | FINDING-1 "Reproduction" |
| probe source is the last-left variant at 2 s / 10 × 100 ms | read from the recovered `repro/probe/probeApp/src/probeMain.c` during this recovery |
| narrow-claim / not-the-stronger-claim discipline | handoff §5.3 final section and §8.1 item 1 |
