# Measurement — how many bytes does the RTEMS-posix `epicsEventDestroy` leak cost per CA client cycle?

**Answer: 160 bytes and exactly 5 heap blocks per CA client connect/disconnect
cycle, on the stock C IOC. One leaked block is 32 bytes.** The two numbers are
measured independently of each other and multiply out with no residual:
5 × 32 = 160.

This closes the first bullet of **"Not measured / open"** in
[`c-base-rtems-posix-event-leak.md`](c-base-rtems-posix-event-leak.md) — "bytes
per leaked block … needs a target measurement" — and puts a measured number
against that document's prediction of **≥5 leaked blocks per rsrv client
cycle**. The predicted count is not merely bracketed by the measurement; it is
hit exactly, in both batches of both runs.

Taken **2026-07-23** on `coding-agent@192.168.2.128`, under
`qemu-system-arm -M xilinx-zynq-a9` (RTEMS 6.0.0, BSP `xilinx_zynq_a9_qemu`,
libbsd networking, 256 MB guest) — the same box, the same `~/rtems-cside/` tree
and the same image as
[`measurement-c-thread-priority-on-rtems-6.md`](measurement-c-thread-priority-on-rtems-6.md),
whose evidence discipline this document follows.

Like that document, this is a measurement, not a second bug report. The bug it
measures is the one already written up in
[`c-base-rtems-posix-event-leak.md`](c-base-rtems-posix-event-leak.md).

---

## 1. What was measured, on which image, with which instrument

| | |
|---|---|
| image (all cycle numbers) | `~/rtems-cside/cioc-fd64.exe` — **byte-identical to the session-3 image**, `sha256 10a4db99…f24bc0`; stock base descriptor configuration (64) |
| base | R7.0.10 `bf11a0c`, **zero source patches** ([`evidence/DEVIATIONS.md`](evidence/DEVIATIONS.md)) |
| heap instrument | **`rt malloc`** — base's `rt` bridge to the RTEMS shell (`libcom/RTEMS/posix/rtems_init.c:500`) running RTEMS 6's own `malloc` command. Discovered with `rt help mem`. |
| thread instrument | `epicsThreadShowAll 1` — base, `libComRegister.c:523` |
| port | 5164 only (this panel's allocation) |
| driver | [`repro/evleak/evleak.py`](repro/evleak/evleak.py), host-side raw CA sockets, `hold6.py`'s connect method unchanged |

**No app-side heap command was needed and none was added to the image that
produced the per-cycle numbers.** The brief allowed adding one if the RTEMS
shell had nothing; it does. `rt help mem` lists exactly one command, `malloc`,
and it prints the full RTEMS heap statistics block — free and used **block
counts**, total bytes free and used, and lifetime allocation/free counters:

```
RTEMShost> rt malloc
C Program Heap and RTEMS Workspace are the same.
Number of free blocks:                              37
Largest free block:                          230323352
Total bytes free:                            230348368
Number of used blocks:                           18440
...
Total number of successful allocations:          18946
Total number of successful frees:                  506
```

The used-**block** counter is what makes this measurement stronger than a
bytes-only one: the leak's *block count per cycle* is read directly, so
"bytes per block" is a division of two measured quantities rather than a
division by an assumed number of leaked objects.

One image in this document is **not** stock — `cioc-evloop-fd64.exe`, §5. It
carries an added application source file and contributes **only** the
per-block size; it contributes none of the per-cycle numbers.

---

## 2. What one cycle is — the per-client threads, read on target

The driver's cycle is: TCP connect → send `CA_PROTO_VERSION` + `CLIENT_NAME` +
`HOST_NAME` → receive the 16-byte version reply → `close()`. The reply is
waited for on purpose: it proves the server-side client task ran, so this is
**not** a bare TCP close that lands before the client task is created.

With **one connection held**, `epicsThreadShowAll 1` shows the two predicted
per-client threads and nothing else (run 1, log lines 304-348):

```
         CAS-TCP      0x16052c0    184614930     18      46       OK
         CAS-UDP      0x16053e8    184614931     16      41       OK
      CAS-beacon      0x1605cc8    184614932     17      44       OK
       CAS-event      0x191c0d8    184614934     19      49       OK
      CAS-client      0x189c0d8    184614935     20      51       OK
```

21 rows held versus **19 rows idle** — the two extra are exactly `CAS-client`
and `CAS-event`.

That is the whole predicted set, and it is created on **accept**, not on first
subscription: `req_server`'s accept branch calls `create_tcp_client`, which
calls `db_init_events()` and then `db_start_events(…, "CAS-event", …)`
(`caservertask.c:1491`, `:1514`), and `req_server` then creates `CAS-client`
itself (`caservertask.c:109`). So **the full predicted count of 5 applies to
this driver's cycle**, with no subset caveat:

| # | leaked `epicsEvent` | create | destroy |
|---|---|---|---|
| 1 | `CAS-client` thread `suspendEvent` | `os/posix/osdThread.c:179` | `:235` |
| 2 | `CAS-event` thread `suspendEvent` | `os/posix/osdThread.c:179` | `:235` |
| 3 | `client->blockSem` | `caservertask.c:1262` | `:1128` |
| 4 | `evUser->ppendsem` | `dbEvent.c:314` | `:396` |
| 5 | `evUser->pexitsem` | `dbEvent.c:320` | `:395` |

The sixth row of the prediction table — "+1 per cancelled monitor" — is
conditional and is measured separately in §6.

---

## 3. The measurement, run 1

Concurrency is **exactly 1** for the entire run: one socket open at a time, and
a 40 ms pause after each `close()` so the server-side teardown finishes before
the next `connect()`. rsrv's freelists and buffer pools therefore cannot raise
their concurrent high-water mark part-way through and fake a slope. 60 warm-up
cycles run **before** the baseline reading.

Raw log: [`evidence/cioc-evleak-2026-07-23.log`](evidence/cioc-evleak-2026-07-23.log)
(25,844 B), qemu pid 2939153.

| reading | cycles since previous | used blocks | total bytes used | free blocks | successful allocs | successful frees | threads |
|---|---|---|---|---|---|---|---|
| `T0` (after a 20-cycle smoke test) | — | 18551 | 32,297,496 | 66 | 19,338 | 787 | 19 |
| `HELD-1` — one connection open | — | 18566 | 32,298,696 | 68 | 19,365 | 799 | **21** |
| `T1` — that connection closed | 1 | 18556 | 32,297,656 | 71 | 19,369 | 813 | 19 |
| `B0` **baseline** | 60 (warm-up) | 18856 | 32,307,256 | 180 | 20,515 | 1,659 | 19 |
| `B1` | **250 (batch A)** | 20106 | 32,347,256 | 619 | 25,271 | 5,165 | 19 |
| `B2` | **600 (batch B)** | 23106 | 32,443,256 | 1,671 | 36,677 | 13,571 | 19 |

**Per-cycle slope, computed from each batch separately:**

| | cycles | Δ used blocks | blocks/cycle | Δ bytes used | **bytes/cycle** |
|---|---|---|---|---|---|
| warm-up (`T1`→`B0`) | 60 | 300 | 5.000 | 9,600 | 160.0 |
| **batch A** (`B0`→`B1`) | 250 | 1,250 | **5.000** | 40,000 | **160.0** |
| **batch B** (`B1`→`B2`) | 600 | 3,000 | **5.000** | 96,000 | **160.0** |

The two batch slopes are equal to the digit — that is the linearity check, and
it passes. `Total bytes free` moves by the same magnitudes in the opposite
direction (−40,000 and −96,000), so the two heap counters agree.

**The allocator's own counters say the same thing independently.** Over batch A
the heap served 4,756 allocations against 3,506 frees; over batch B, 11,406
against 8,406:

| | allocs/cycle | frees/cycle | **net unfreed/cycle** |
|---|---|---|---|
| batch A | 19.02 | 14.02 | **5.00** |
| batch B | 19.01 | 14.01 | **5.00** |

So each cycle makes ~19 allocations, returns ~14 of them, and permanently
retains exactly 5.

**Thread census is back to the idle count of 19 at every reading** (`B0`, `B1`,
`B2`, and `T1`) — 21 only while a connection is deliberately held. The growth
is retained memory, not lingering threads.

**One cycle in isolation.** Because 20 smoke cycles preceded `T0`, the pools
were already warm there, so `T0`→`T1` — a single held-and-closed connection —
is itself a clean single-cycle reading: **+5 blocks, +160 bytes**. It agrees
with the batch slopes at n=1.

---

## 4. The measurement, run 2 — a second boot

The whole run was repeated on a **fresh boot of the same image**, qemu pid
2952568. Raw log:
[`evidence/cioc-evleak-repeat-evmon-2026-07-23.log`](evidence/cioc-evleak-repeat-evmon-2026-07-23.log)
(32,415 B), first half.

| reading | cycles since previous | used blocks | total bytes used | free blocks | threads |
|---|---|---|---|---|---|
| `T0` — genuinely before any connection | — | 18440 | 30,445,544 | 36 | 19 |
| `HELD-1` | — | 18466 | 32,295,496 | 39 | **21** |
| `T1` | 1 | 18456 | 32,294,456 | 43 | 19 |
| `B0` **baseline** | 60 (warm-up) | 18756 | 32,304,056 | 153 | 19 |
| `B1` | **250 (batch A)** | 20006 | 32,344,056 | 592 | 19 |
| `B2` | **600 (batch B)** | 23006 | 32,440,056 | 1,644 | 19 |

| | cycles | Δ used blocks | blocks/cycle | Δ bytes used | **bytes/cycle** |
|---|---|---|---|---|---|
| warm-up | 60 | 300 | 5.000 | 9,600 | 160.0 |
| **batch A** | 250 | 1,250 | **5.000** | 40,000 | **160.0** |
| **batch B** | 600 | 3,000 | **5.000** | 96,000 | **160.0** |

Identical to run 1 in every cell. Two boots, four batches, one slope.

**This run also shows why the warm-up requirement exists.** In run 2 no smoke
test preceded `T0`, so `T0`→`T1` is the *first-ever* connection on that boot,
and it costs **+1,848,912 bytes and +16 net blocks** — about 1.85 MB of rsrv
buffer pool and freelist, paid once. From the second cycle onward the slope is
already the flat 5 blocks / 160 bytes shown above. Had a baseline been taken
before that first connection, that one-time 1.85 MB would have been smeared
across the batch and reported as leak. It is bounded caching, and the warm-up
excludes it by construction.

**Heap fragmentation, as a side observation.** The *free*-block count also grows
linearly — run 1: +439 over 250 cycles (1.756/cycle) and +1,052 over 600
(1.753/cycle). Each retained 32-byte block strands a free fragment beside it, so
the leak costs free-list entries as well as bytes. This is an observation, not a
claim about any allocation-failure threshold; none was reached (`Total number of
failed allocations: 0` at every reading).

---

## 5. Bytes per leaked block, measured directly

Dividing 160 by 5 already gives 32 B/block, and both operands are measured. The
division was nevertheless checked against a direct measurement of a single
`epicsEvent` lifecycle, so that the per-block figure does not depend on the
per-cycle count at all.

This is the **only** part of this document taken on a non-stock image:
`cioc-evloop-fd64.exe` (qemu pid 2947898), which adds one application source
file, [`repro/evleak/ciocEvLoop.c`](repro/evleak/ciocEvLoop.c), registering two
iocsh commands. It patches nothing in base or RTEMS; it calls base's public
`epicsEvent.h` API, exactly as the session-2 `ciocSizes.c` does. Declared in
[`evidence/DEVIATIONS.md`](evidence/DEVIATIONS.md) under "Session 4 additions".
Raw log: [`evidence/cioc-evloop-2026-07-23.log`](evidence/cioc-evloop-2026-07-23.log)
(12,190 B).

`evloop N` performs N × (`epicsEventCreate` + `epicsEventDestroy`) and nothing
else:

| reading | used blocks | total bytes used | successful allocs | successful frees |
|---|---|---|---|---|
| `EV-S0` | 18,458 | 30,446,160 | 18,964 | 506 |
| `EV-A` — after `evloop 10000` | 28,458 | 30,766,176 | 28,969 | 511 |
| `EV-B` — after a second `evloop 10000` | 38,458 | 31,086,176 | 38,974 | 516 |

| | events | Δ used blocks | blocks/event | Δ bytes used | **bytes/event** |
|---|---|---|---|---|---|
| `evloop` batch A | 10,000 | 10,000 | **1.0000** | 320,016 | 32.0016 |
| `evloop` batch B | 10,000 | 10,000 | **1.0000** | 320,000 | **32.000** |

Batch B is exact. Batch A's extra 16 bytes are the 5 unrelated allocations that
ran alongside it (allocations rose by 10,005 and frees by 5 in both batches —
the 5 are the `rt malloc` command's own work).

Those counters are themselves the defect stated in allocator terms: **10,000
create/destroy pairs produced 10,000 allocations and zero matching frees.**

`evsize` prints the struct the wrapper is made of:

```
EVSIZE sizeof(rtems_binary_semaphore)=24
```

So one leaked `epicsEventOSD` is **24 bytes of struct plus 8 bytes of RTEMS heap
block overhead and alignment = 32 bytes of heap consumed**.

**Cross-check: 5 blocks/cycle × 32 B/block = 160 B/cycle — the measured
per-cycle figure exactly, with zero residual bytes.**

---

## 6. Control: the conditional "+1 per cancelled monitor" row

The prediction table's sixth row is not unconditional.
`db_cancel_event` calls `db_sync_event` (which creates and destroys one further
`epicsEvent`, `dbEvent.c:572`/`:591`) **only** when a callback for that
subscription is pending or in progress concurrently with `event_task`
(`dbEvent.c:612-617`). So the expected slope for a monitor-carrying cycle is not
an integer — it is somewhere in [5, 6], and where it lands measures how often
that race is won.

[`repro/evleak/evmon.py`](repro/evleak/evmon.py) runs the same discipline
(concurrency 1, 60 warm-up, two batches) but each cycle also creates a channel
on `CIOC:HB100` (`SCAN = .1 second`) and subscribes a monitor, then closes with
the subscription still live. All 660 cycles established the subscription. Same
boot and log as run 2, second half.

| reading | cycles | used blocks | total bytes used | threads |
|---|---|---|---|---|
| `M0` baseline | 60 warm-up | 23,317 | 32,709,968 | 19 |
| `M1` | **200 (batch A)** | 24,317 | 32,741,984 | 19 |
| `M2` | **400 (batch B)** | 26,319 | 32,806,056 | 19 |

| | cycles | Δ used blocks | blocks/cycle | Δ bytes used | bytes/cycle |
|---|---|---|---|---|---|
| batch A | 200 | 1,000 | 5.000 | 32,016 | 160.08 |
| batch B | 400 | 2,002 | 5.005 | 64,072 | 160.18 |

**The sixth block almost never appears: 2 extra blocks across 600
monitor-carrying cycles (~0.3%).** The row is real code and the two extra blocks
in batch B are consistent with it firing twice, but at this load the sync path
is essentially never taken, and **the per-cycle floor stays at 5 blocks / 160
bytes.** No claim is made here about a load that would take it more often.

---

## 7. What the number means

At 160 bytes per connect/disconnect cycle, on this guest's ~261 MB allocatable
area, and with nothing else consuming heap, a client that reconnects once a
second exhausts the heap in roughly nineteen days; at ten cycles a second, in
under two. Those are arithmetic on the measured slope for one idle IOC, not
measured endurance runs — no long-run exhaustion test was performed, and a real
IOC has other consumers.

The load-bearing property is not the size but the shape: **the growth is
monotonic and unbounded in connect/disconnect count.** 160 B is small enough
that it will not be noticed in a short test and large enough to matter to a
long-lived IOC facing a flapping client or a reconnecting gateway.

The one-line fix in
[`c-base-rtems-posix-event-leak.md`](c-base-rtems-posix-event-leak.md) — adding
`free(pSem)` to `epicsEventDestroy` — is what returns all 5 blocks per cycle.

---

## 8. Limits of this measurement

* **Attribution of the 5 blocks is by count and size, not by address.** The
  measurement shows exactly 5 permanently-retained blocks per cycle, and shows
  independently that one leaked `epicsEventOSD` occupies exactly 32 bytes, and
  5 × 32 accounts for the measured 160 bytes with no residual. It does not
  photograph the five addresses and prove each one is an `epicsEventOSD`. Any
  other hypothetical 32-byte-per-cycle leak of exactly the predicted cardinality
  would be indistinguishable from this data alone. The source pairing in §2 is
  what identifies them; the measurement is what counts and sizes them.
* **One image, one BSP, one guest, single core.** Two boots, not more. No SMP
  configuration and no second BSP.
* **`sizeof(epicsEventOSD)` itself was not printed** — it is an opaque type
  outside `osdEvent.c`. What was printed is `sizeof(rtems_binary_semaphore)` =
  24, its only member. The 32 is total heap consumed, which is the figure that
  matters for a leak, and it was measured directly rather than derived from 24.
* **The 8-byte gap between 24 and 32 is not separately verified** as RTEMS heap
  block overhead versus alignment padding; no RTEMS heap-internals reading was
  taken to split it.
* **Concurrency 1 is enforced by construction, not proven by observation.** The
  driver holds one socket at a time with a 40 ms gap, and the thread census
  returns to 19 at every reading; no instrument sampled the concurrent client
  count *during* a batch.
* **No endurance run.** The longest continuous run was 600 cycles. The extrapolations
  in §7 are arithmetic on the slope, and are labelled as such.
* **The monitor control's ~0.3% rate is specific to one record at 10 Hz with one
  subscription and a 150 ms hold.** It is not a general statement about how often
  `db_sync_event` runs.
* **`evleak.py` prints its first reading as `T0-initial-after-20-smoke-cycles`.**
  That label is accurate for run 1 only; in run 2 no smoke test was run and the
  reading is genuinely pre-first-connection (§4 relies on that). The label was
  left unedited so the committed script is byte-identical to the one that
  produced both logs.

---

## 9. Reproducing it

On the box, from `~/rtems-cside/`:

```sh
./boot-cioc.sh ~/rtems-cside/cioc-fd64.exe    # ~45 s to the iocsh prompt
python3 evleak.py                             # 60 warm-up, then batches of 250 and 600
python3 evmon.py                              # the monitor control
tail -f cioc.log                              # console; iocsh input goes to the ciocin fifo

# the per-block size, on the variant image only
./boot-cioc.sh ~/rtems-cside/cioc-evloop-fd64.exe
printf 'evsize\n'       > ciocin
printf 'rt malloc\n'    > ciocin
printf 'evloop 10000\n' > ciocin
printf 'rt malloc\n'    > ciocin
```

Readings are bracketed in the console log by `#=== tag ===` comment markers,
which iocsh echoes; `rt malloc` and `epicsThreadShowAll 1` follow each marker.

To rebuild the variant image, drop
[`repro/evleak/ciocEvLoop.c`](repro/evleak/ciocEvLoop.c) and
[`repro/evleak/ciocEvLoop.dbd`](repro/evleak/ciocEvLoop.dbd) into
`cioc/ciocApp/src/`, add `cioc_SRCS += ciocEvLoop.c` and
`cioc_DBD += ciocEvLoop.dbd` to that directory's `Makefile`, and `make`.

### Checksums (verified on both ends)

SHA-256, computed under `~/rtems-cside/` on `192.168.2.128` and again on the
copy in this repository.

As in the priority measurement, the RTEMS serial console emits `CR LF` and this
repository's root `.gitattributes` sets `* text=auto eol=lf`, which would strip
every `CR` and break these checksums. `evidence/.gitattributes` (`*.log -text`)
prevents that. If a future edit of the attributes file re-normalizes these logs,
the rows below stop matching — that is the intended alarm, not a stale checksum.

| file in this directory | source on the box | sha256 |
|---|---|---|
| `evidence/cioc-evleak-2026-07-23.log` | `log/cioc-evleak-2026-07-23.log` | `bedb9948db8178cc51eeee8c0d2b710f9dccd020d6d4dd5d757f8868c6d044f2` |
| `evidence/cioc-evleak-repeat-evmon-2026-07-23.log` | `log/cioc-evleak-repeat-evmon-2026-07-23.log` | `60b9a954918103229ad9797368ebc7ba94d288a766220a8326b3ea70976c9021` |
| `evidence/cioc-evloop-2026-07-23.log` | `log/cioc-evloop-2026-07-23.log` | `2dafc6cb394e82d91b6cef7bbd56b9045340e8ac28d2f9b2c864de6f53761669` |
| `repro/evleak/evleak.py` | `evleak.py` | `d3efc1831aaf13bc9f50fa6a7320a298d3ce4527a0d55dbacd5e71589977a338` |
| `repro/evleak/evmon.py` | `evmon.py` | `89977aab0100576a21e25739fba751a9e47303481251fe739a7835e90e4ef352` |
| `repro/evleak/ciocEvLoop.c` | `cioc/ciocApp/src/ciocEvLoop.c` | `75640d0be5bc5710815fc8625a589367319ea9c4046e47e11c3ddac30e737695` |
| `repro/evleak/ciocEvLoop.dbd` | `cioc/ciocApp/src/ciocEvLoop.dbd` | `b6b3252a208cc24c7a70e411c9443b536c2de6cae5502e6e3c397b3d42e4e7f0` |
| `evidence/DEVIATIONS.md` (now includes "Session 4 additions") | `DEVIATIONS.md` | `796e097350fe4a933dd996454677a5c600c5a9800a03abb06123133dbab96a5d` |

Target images (on the box only; not copied into this repository):

| image | sha256 | stock? |
|---|---|---|
| `cioc-fd64.exe` — all per-cycle numbers | `10a4db99c63159423a4d7bda2d6db5f1d57dcf73f6a7dc59d5aabc8f19e3efa1` | yes, unchanged since session 3 |
| `cioc-evloop-fd64.exe` — §5 only | `964bcbf64d59ba522d689a2fd7725cd9608dbc171650bcfe2092c01301f24bc0` | **no** — adds `ciocEvLoop.c` |

`evidence/DEVIATIONS.md` replaces the copy committed with the priority
measurement (`bcd3a3ed…b00b44`); the earlier content is unchanged and the
session-4 section is appended after it.

## Sourcing

| claim | verified how |
|---|---|
| `rt malloc` exists and is the RTEMS shell's own command | `rt help` then `rt help mem` on target, run 1 log |
| every heap figure in §3, §4, §5, §6 | `rt malloc` blocks in the three raw logs, quoted by checksum above; no figure is paraphrased |
| every thread count | row count of each `epicsThreadShowAll 1` block in the same logs |
| `CAS-client` + `CAS-event` are the per-client threads, and both are created on accept | target census with one connection held (§2) **and** source: `caservertask.c:109`, `:1491`, `:1514` |
| the 5 create/destroy pairs of §2 | the prediction table of `c-base-rtems-posix-event-leak.md`, whose citations were verified there |
| `db_sync_event` is conditional on an in-flight callback | `dbEvent.c:605-635` read directly (`sync` flag set only in the `callBackInProgress` branch) |
| 32 bytes per leaked block | `evloop 10000` × 2 on the variant image, §5 |
| `sizeof(rtems_binary_semaphore)` = 24 | `evsize` on target, run 3 log |
| base is unpatched; what the variant image adds | `evidence/DEVIATIONS.md`, written on the box at measurement time |
| no port collision, no foreign process signalled | `ss -ltnp` and `ps` before booting; the other panel's guests were on 4441/8011, this panel used 5164 only; every qemu stop went through `boot-cioc.sh`'s own `qemu.pid` |
