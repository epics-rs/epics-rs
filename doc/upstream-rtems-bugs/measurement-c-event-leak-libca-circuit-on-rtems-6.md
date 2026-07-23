# Measurement — the libca per-virtual-circuit heap cost, measured not derived

**Answer: a libca client virtual circuit leaks 18 `epicsEvent` blocks (plus ~1
non-event block) ≈ 19 heap blocks ≈ 610 bytes per connect/disconnect cycle, and
spawns 4 OS threads — measured directly. The standing derived figure of "6 blocks
= 192 B" is a ~3× undercount:** it counted only the two `tcpiiu` transport threads
(`recvThread`, `sendThread`) at 3 wrapper events each, and ignored both the ~2
further threads the connection spawns and the ~6 non-thread `epicsEvent`s in the
`cac`/`tcpiiu` machinery.

This replaces the derivation in the first bullet of **"Not measured / open"** in
[`c-base-rtems-posix-event-leak.md`](c-base-rtems-posix-event-leak.md) —
"libca-side per-circuit figure (predicted 6 blocks = 192 B) is derived from the
measured 32 B block cost, not separately measured" — with a target measurement,
and states precisely what could and could not be isolated on this rig.

Taken **2026-07-24** on `coding-agent@192.168.2.128`, `qemu-system-arm -M
xilinx-zynq-a9`, RTEMS 6, BSP `xilinx_zynq_a9_qemu`, libbsd, 256 MB guest. Image
`~/rtems-cside/cioc-evtrace-fd64.exe`
(sha256 `014fd80b104a6c7a095743e1aab2a169079f8006209c9b985e2ddf2b7a1ad07d`), the
same `--wrap` tracer image as the attribution measurement.

---

## 1. Why loopback cannot be used

The obvious rig — the IOC's own libca client connecting to its own rsrv — does
**not** build a virtual circuit. CA short-circuits a locally-served PV to
in-memory `dbCa` access, and the attempt prints

```
dbContext: preemptive callback required for direct in memory interfacing of CA
channels to the DB.
CALOOP done n=1 mode=1 connected=0 failed=1 ctxfail=0
```

`connected=0`: no `tcpiiu`, no threads, nothing to measure. A circuit only exists
when the server is a **different process**.

## 2. The rig — target libca client → host softIoc, TCP-direct

An external server on the box (linux `softIoc`, base R7.0.10, PVs `TEST:AI`,
`TEST:SI`) listens on `0.0.0.0:5064`. The guest's libca reaches it through the
SLIRP gateway with **`EPICS_CA_NAME_SERVERS=10.0.2.2:5064`** — a TCP-direct name
server, so no UDP broadcast search is needed (broadcast-to-self over libbsd is
unreliable, and the SLIRP search-reply address is not routable back). The
server-side of every circuit therefore allocates in the **host** process, and the
guest heap that `rt malloc` reads sees only the client (libca) side.

`caloop N pv mode` ([`repro/evleak/ciocEvTrace.c`](repro/evleak/ciocEvTrace.c)):
* `mode 0` — `ca_context_create` + `ca_context_destroy`, **no channel**
* `mode 1` — the same plus one channel `ca_pend_io`-connected to `pv`, i.e. one
  full client virtual circuit built and torn down

One connected remote circuit was confirmed before measuring:
`CALOOP done n=1 mode=1 connected=1 failed=0 ctxfail=0`.

## 3. Heap slopes (`rt malloc`)

**mode 0 — context only, 2 × 200 cycles** (concurrency 1). Raw:
[`evidence/cioc-evcirc-mode0-plus-socketwall-2026-07-24.log`](evidence/cioc-evcirc-mode0-plus-socketwall-2026-07-24.log).

| reading | cycles | used blocks | bytes used |
|---|---|---|---|
| `A0` baseline | — | 19,671 | 35,069,336 |
| `A1` | +200 | 20,071 | 35,082,136 |
| `A2` | +200 | 20,471 | 35,094,936 |

Both batches: **Δ 400 blocks / 12,800 B → 2.000 blocks, 64.0 B per cycle**,
byte-exact. A bare context lifecycle (no circuit, `ca_disable_preemptive_callback`)
leaks exactly 2 blocks and, by the tracer below, creates **no OS thread**.

**mode 1 — context + one remote circuit, 2 × 40 cycles**, paced (chunks of 8, 8 s
drain, 70 s TIME_WAIT flush between batches; see §5). Raw:
[`evidence/cioc-evcirc2-mode1-plus-single-2026-07-24.log`](evidence/cioc-evcirc2-mode1-plus-single-2026-07-24.log).

| reading | cycles | used blocks | bytes used |
|---|---|---|---|
| `C0` baseline | — | 18,835 | 34,754,728 |
| `C1` | +40 | 19,692 | 34,781,920 |
| `C2` | +40 | 20,532 | 34,808,648 |

| batch | Δ blocks | blocks/cycle | Δ bytes | bytes/cycle |
|---|---|---|---|---|
| A (`C0→C1`) | 857 | 21.425 | 27,192 | 679.8 |
| B (`C1→C2`) | 840 | 21.000 | 26,728 | 668.2 |

The two batches agree to ~2 % (not byte-exact — N is 40, not 200, and context
teardown is asynchronous). Mean **≈ 21.2 blocks / ≈ 674 B per mode-1 cycle**.

**Per circuit = mode 1 − mode 0 = ≈ 19.2 blocks / ≈ 610 B per cycle.**

## 4. Event census of one circuit (tracer, full teardown)

`caloop 1 mode=1` and `caloop 1 mode=0`, each traced with a 6 s settle so
teardown completes before `evtrace off`. Create records by caller PC (resolved in
the [attribution measurement](measurement-c-event-leak-attribution-on-rtems-6.md)):

| | `epicsEvent.cpp:43` (C++ `epicsEvent`) | `osdThread.c:179` (OS thread `suspendEvent`) | total event creates |
|---|---|---|---|
| mode 0 (context) | 2 | 0 | 2 |
| mode 1 (context + circuit) | 16 | 4 | 20 |
| **circuit = mode1 − mode0** | **14** | **4** | **18** |

So one circuit lifecycle creates **4 OS threads** and **18 `epicsEvent` blocks**
(14 C++ `epicsEvent` objects + 4 thread `suspendEvent`s), every one of which
leaks 32 B because `epicsEventDestroy` never frees.

**Cross-check against the heap:** 18 event blocks × 32 B = 576 B; the heap slope
gives ≈ 610 B and ≈ 19 blocks per circuit. The residual ≈ 34 B / ≈ 1 block is the
non-`epicsEvent` per-circuit leak. And mode 1's 21.2 blocks/cycle decomposes as
2 (context) + 18 (circuit events) + ~1 (circuit non-event) — the three
measurements close on each other.

**Where the derived "6" came from, and why it is low.** The derivation counted
`tcpiiu`'s `recvThread` + `sendThread` at (`suspendEvent` + `event` + `exitEvent`)
= 3 each = 6. Those two threads are a subset of the 4 measured here; the
derivation omitted the other ~2 threads this connection spawns and the ~6
non-thread C++ `epicsEvent`s in the `cac`/`tcpiiu` connection machinery (14 C++
events − 8 for the four threads' `event`+`exitEvent` members = 6 machinery
events).

## 5. Limits — what is and is not isolated

* **Configuration-dependence — the central caveat.** This is the
  `EPICS_CA_NAME_SERVERS` (TCP-direct) path, the only one that connects over
  SLIRP. The derived "6" was read from the **UDP-broadcast-search** `tcpiiu` path,
  which was **not** exercised on target. A broadcast-search client could spawn a
  different thread/event count. The measured 18 events / 4 threads is exact for
  the name-server path; it is not proven identical to the search path.
* **Individual `epicsEvent`s are not attributable to components.** Every C++
  `epicsEvent` construction funnels through one inlined PC (`epicsEvent.cpp:43`),
  so the tracer separates "C++ `epicsEvent`" from "OS-thread `suspendEvent`" but
  cannot say which C++ event is the `recvThread`'s vs the `cac` notify's. The
  4 threads likewise share `osdThread.c:179`; `rtems_object_get_name` is blind to
  pthread names on this target, so the 4 were counted, not named.
* **No large-N clean mode-1 slope — a hard rig wall.** A 200-cycle mode-1 burst
  exhausts libbsd's socket pool and is **fatal**: the client caRepeater cannot
  get a datagram socket and a thread suspends —
  `../repeater.cpp: Unable to create repeater socket because "No buffer space
  available" - fatal` (captured in the mode-0 log after its batches). This is TCP
  TIME_WAIT accumulation on rapid client reconnects, not the event leak; it caps
  the clean mode-1 measurement at N≈40 with ~2 % batch agreement rather than the
  byte-exact 2×200 the server-side cycle allowed. It is itself a finding: a
  flapping libca client on this BSP walls on sockets long before the heap.
* **mode 0 uses `ca_disable_preemptive_callback`.** That is why it spawns no
  thread; a preemptive-callback context would differ. mode 1 uses the same flag,
  so the differential is consistent, but "context cost" here is the
  non-preemptive context cost.
* **Two boots, one image, one BSP, single core.**
