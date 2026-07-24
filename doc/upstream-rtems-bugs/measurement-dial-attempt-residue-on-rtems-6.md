# Per-connection-attempt heap residue on RTEMS 6, attributed by call site

**Measured 2026-07-24 on the RTEMS/QEMU box** (`xilinx_zynq_a9_qemu`,
qemu-system-arm 8.2.2, RTEMS 6.0.0 `2faafecb`, arm-rtems6-gcc 13.3.0, nightly
`rustc 1.99.0 (87e5904f5 2026-07-20)`, epics-rs `419b59d5d7c` on the box's
`box-measure2` branch). All four images were built through the custom target
spec carrying `has-thread-local: true` — i.e. **the flip is ON in every image
below**, so the 136 B `Arc<Thread::Inner>` block is already zero and cannot
mask anything.

## What this closes

`rust-std-rtems-tls-thread-leak.md` left this open:

> The ~48–51 B gap between the attributed `Arc`+header (136 B measured on bare
> threads) and the total per-*connection-attempt* residue (176–179 B) is
> unattributed. … it is **not** the thread handle and **not** a bare
> thread+socket dial (both go to 0.00 under the flip), so it is other
> per-attempt allocation in the real dial machinery — flip-independent.

**That remainder does not exist.** Measured directly, with absolute
per-call-site allocator accounting rather than a slope differential:

> On the flip image, the per-attempt dial shape and the pooled dial shape grow
> the heap **byte-identically**: +1936 B each over the *same* 209 dial
> attempts, in the *same* five size classes, with the *same* per-class counts.
> The per-attempt residue the `DialPool` removes is therefore **0 B/attempt**,
> not ~40–51 B/attempt.

The 176.0 ± 5 / 179.1 ± 3 B/attempt that `doc/calink-rtems-design.md` §13.4 and
`doc/pvalink-rtems-design.md` §9.11 measured on 2026-07-23 was correct for the
**stock** spec, and it was *all* thread-creation cost. The "~40 B remainder"
was an arithmetic residue between two differently-scoped numbers — a **bare
unnamed** thread (136 B, `tlsdtor` rig) subtracted from a **named thread inside
the real dial** (176 B, IOC differential) — not a separate allocation. Nothing
in the dial machinery outside the thread handle costs a retained byte.

## Why a `#[global_allocator]` counter could not answer this

It reads 0 on this target: `std`'s allocations reach libc `malloc` directly on
the paths that matter here, so a counter wrapped around the Rust
global-allocator hook never sees them. The accounting is therefore done one
level down, in C, where every allocation on the image really lands.

## Method — `-Wl,--wrap=malloc` + `arm-rtems6-addr2line`

`repro/dialresidue/heapattr.c`, compiled into the image and linked with

```
-Wl,--wrap=malloc -Wl,--wrap=free -Wl,--wrap=posix_memalign -Wl,--wrap=aligned_alloc
```

Every live block is recorded in a 131072-slot open-addressed pointer table with
its requested size and a *call-site id*; `free` removes it. Two incremental
indexes (per requested size, per site) mean a report never walks the big table.
`epics_heapattr_report()` is called from the IOC's existing 10 s C6 probe, on
the same line as the `dialpool attempts=` counter the residue is priced per.

Three things this design had to get right, each of which was wrong in a first
draft and was fixed against measured evidence:

* **Do not wrap `calloc`/`realloc`.** RTEMS implements both on top of
  `malloc()`/`free()` in *separate* translation units
  (`cpukit/libcsupport/src/calloc.c`, `realloc.c`), so their inner calls are
  wrapped already and an outer wrapper records the same pointer twice. Measured
  on the first smoke boot: 55 duplicate inserts per 10 s report. With
  `calloc`/`realloc` unwrapped, every production run below reports
  `tbl_ovf=0 untracked_free=0` — i.e. every allocation and every free is
  accounted, with none double-counted and none missed.
* **Delete with tombstones, not `NULL`.** Cutting a collision chain orphans
  every entry behind it, and an orphaned entry reads as a block that is never
  freed — a fabricated leak that grows.
* **Take the call site from a conservative stack scan, not
  `__builtin_return_address`.** `__builtin_return_address(0)` inside the
  wrapper always names the allocator shim, which is the useless answer, and
  deeper levels are not trustworthy here: rustc emits A32 with LLVM's frame
  layout while the C shim is built `-mthumb` with gcc's, and the two
  conventions disagree (the link shows the `____wrap_malloc_from_arm` veneers).
  The wrapper instead scans words upward from its own frame — that region is
  live caller frames — and keeps the first 6 that fall inside
  `[bsp_section_text_begin, bsp_section_text_end)`. The scan does pick up
  spilled non-return-address words (below, `pc[2]`/`pc[3]` of the CA site name
  functions that are not on the path), so **only the resolved Rust frames are
  claimed**, never the whole vector.

Mutual exclusion is interrupt-off, not a spinlock (which deadlocks a
uniprocessor under SCHED_FIFO when a high-priority thread preempts the holder)
and not a pthread mutex (which would allocate on first use from inside the
allocator).

**Kill discipline:** every boot went through `boot-measure.sh`, which is
own-pid only (`rigpid.sh`). No `pkill` of any kind was run.

## The four images

All four are `rtems-ca-ioc` with `--features client-core,bringup-probes`, a
compiled-in name server at `10.0.2.2:15076` where **nothing listens** (SLIRP
returns RST, so every dial fails fast), and a compiled-in
`EPICS_CA_CONN_TMO=5` so 1260 s yields ~250 attempts instead of the shipped
30 s cadence's ~40.

| image | dial shape | NS queue depth | site print threshold |
|---|---|---|---|
| `heapattr-ca.exe` | pooled (`CA_DIAL_POOL`, the shipping shape) | 256 (default) | 256 B |
| `heapattr-perattempt.exe` | **one transient thread per attempt** (pre-`9daff491`) | 256 (default) | 256 B |
| `heapattr-nsdepth8.exe` | pooled | **8** | 256 B |
| `heapattr-sites.exe` | pooled | 256 (default) | 8 B |

The per-attempt image is a source mutation that replaces `CA_DIAL_POOL.dial()`
with a `dial_one_shot()` that spawns a dedicated thread per attempt with the
same name / priority / stack class and the same `oneshot` reply channel
(`repro/dialresidue/apply-perattempt.py`). **That the mutation was live is not
assumed:** the IOC's own probe reads `dialpool workers=0` in that image (the
pool is never entered) against `workers=1` in the pooled one, with both images
completing the same 250 attempts.

## Result 1 — the per-attempt residue is 0 B/attempt

Steady window: attempts 41 → 250 (209 attempts, 104 reports), after warm-up.

| | live bytes at att 41 | at att 250 | Δ | lsq slope |
|---|---:|---:|---:|---:|
| pooled | 570702 | 572638 | **+1936 B** | 6.618 B/attempt |
| per-attempt | 570274 | 572210 | **+1936 B** | 6.629 B/attempt |
| **difference** | | | **0 B** | **0.011 B/attempt** |

Live *blocks* grew by exactly 20 in both. The growth decomposes into the same
five size classes, with the same counts, in both images:

| size | Δ count (both images) | Δ bytes |
|---:|---:|---:|
| 144 | +10 | 1440 |
| 208 | +1 | 208 |
| 64 | +3 | 192 |
| 28 | +3 | 84 |
| 4 | +3 | 12 |
| | | **1936** |

End-of-run accounting health, both images: `untracked_free=0 tbl_ovf=0
site_ovf=0 size_ovf=0` (pooled `alloc=177419 free=175629`; per-attempt
`alloc=179299 free=177517`). Every byte allocated on the image is in this
accounting.

Cross-checked against RTEMS's own heap, which knows nothing about the wrapper —
`epics_rtems_boot::stats::mem_usage()` over the same window: pooled +2176 B,
per-attempt +2112 B, a difference of 64 B over 209 attempts (0.31 B/attempt,
under one heap block). The 240 B by which each exceeds the wrapper's 1936 B is
20 new blocks × the 8 B RTEMS block header, plus one block of sampling skew.

## Result 2 — what *does* grow is bounded warm-up, not a leak

The dominant term, 144 B × 10 blocks, is not attempt-paced: it is wall-clock
paced at **one 144 B block per ~120 s**, identical in both dial shapes.
`addr2line` on the site's captured frames, both images:

```
pooled       site=6923  pc 0x003df0b1 0x001fc82c 0x002cff6e 0x0054cfb3 0x002cff6e 0x001fb5e4
per-attempt  site=5321  pc 0x003df601 0x001fe41c 0x0054d533 0x001fd1c4 0x002ef664 0x0054b043

0x003df0b1 / 0x003df601  __wrap_malloc
0x001fc82c / 0x001fe41c  epics_ca_rs::client::search::fire_searches::{closure#0}
0x001fb5e4 / 0x001fd1c4  epics_ca_rs::client::search::run_engine::{closure#0}
```

(The remaining words are spill garbage the conservative scan picked up —
`parse_db_with_breaktables`, `_API_Mutex_Unlock`, `RandomState::hash_one` are
not on this path and are **not** claimed as callers.)

That is `fire_searches`'s `ns_try_send(ns_tx, current_frame.clone())`: a search
datagram cloned into the `EPICS_CA_NAME_SERVERS` channel. With the name server
unreachable nothing drains that channel, so each retry's frame is retained —
until the channel is full, at which point `try_send` drops the frame, counts
`ca_client_nameserver_queue_drops_total` and warns. The channel is
`mpsc::channel::<Vec<u8>>(ns_queue_cap)` with `ns_queue_cap` =
`EPICS_CA_NAMESERVER_QUEUE_DEPTH`, default **256**.

**Bounded warm-up, proven by observing the ceiling, not by reading the
constructor.** At ~1 frame per 120 s the default 256 would take ~8.5 hours to
fill, so the depth was pinned to 8 and the same run repeated:

| seq | attempts | live 144 B blocks | live bytes |
|---:|---:|---:|---:|
| 1 | 3 | 7 | 567688 |
| 3 | 7 | 9 | 569192 |
| 12 | 25 | 12 | 570496 |
| 18 | 37 | 13 | 570544 |
| **24** | **49** | **14** | **570784** |
| 30 … 108 | 61 … 220 | **14** (flat) | **570784** (flat) |
| 120 | 244 | 14 | 570880 |

Growth stops dead at exactly **+8** blocks — the pinned depth — after attempt
49, and neither the block count nor total live bytes moves for the next 195
dial attempts (~20 minutes). RTEMS's own `MEM_USED` over the same window grows
440 B against 2176 B for the uncapped pooled image: capping the queue removes
~80% of all residual growth.

Ceiling at the shipping default: **256 × 144 B = 36,864 B**, reached only while
a configured name server is unreachable, and released as soon as it answers.
Not a leak.

## Result 3 — the remaining 496 B is churn, not growth

The fourth image re-ran the pooled configuration with the site print threshold
dropped from 256 B to 8 B, so the four small classes could be resolved
(attempts 40 → 248, 208 attempts; +1840 B total, the same shape as run 1).

| class | Δ over 208 attempts | site `pc[1]` | resolves to |
|---:|---:|---|---|
| 144 B | +10 | `0x001fc82c` | `epics_ca_rs::client::search::fire_searches::{closure#0}` |
| 64 B | +2 | `0x00544271` → `0x003cb6dc` | `calloc` ← `OnceBox<pthread Mutex>::initialize` |
| 28 B | +2 | `0x003200c4` | `epics_base_rs::runtime::background::timer_sleep::sleep_until` |
| 4 B | +2 | — | — |
| 208 B | +1 | — | — |

The ten 144 B blocks are spread over six site ids that all carry the *same*
`pc[1]` — `fire_searches` — and differ only in which spill word the
conservative scan caught at `pc[3]`; they are one site, confirming Result 2.

The 64 B class is a lazily-initialised `pthread` mutex box: a one-shot per
distinct lock, bounded by the number of locks in the image. The 28 B class is
the background timer's per-sleep allocation and is *churn*, not growth — the
same run shows four 28 B sites going `1 -> 0` against three going `0 -> 1`,
i.e. blocks being freed and re-taken at different call sites between the two
sampling instants, with a net of +2 blocks (56 B) over 208 attempts. Neither is
attempt-paced, and both are identical in the per-attempt image.

## What is therefore true

1. **There is no `~40–51 B` non-thread dial-machinery residue.** Per-attempt
   minus pooled is 0 B/attempt on the flip image, measured absolutely.
2. **The 176/179 B/attempt figure was entirely thread-creation cost**, which
   `has-thread-local: true` zeroes. The flip closes the per-attempt residue
   completely; no follow-on allocation work is owed for it.
3. **The only growth left under an unreachable name server is bounded**, at
   `EPICS_CA_NAMESERVER_QUEUE_DEPTH × 144 B` (36,864 B at the default), in
   `search::fire_searches`, and it is unaffected by the dial shape.

## Not measured / open

* The PVA half (`doc/pvalink-rtems-design.md` §9.11's 179.1 B/attempt) was not
  re-measured under `--wrap`; only the CA dial was. The CA result makes the
  same conclusion likely for PVA but does not establish it.
* The single 208 B block per ~209 attempts was not attributed: it stayed under
  the 8 B print threshold's ranking cut in the fourth run's listing.
* The measurement changes two compiled-in settings from the shipping defaults
  (`EPICS_CA_CONN_TMO=5` to reach 250 attempts in 1260 s; the queue depth in
  image 3). Neither changes what a single attempt allocates; both change how
  often it happens.
* The wrapper does not see allocations made through an allocator other than
  libc `malloc` (libbsd's internal pools). Those are invisible to both the
  wrapper *and* to `mem_usage()`, and the two agreed, so nothing on the CA dial
  path is hiding there.
* Box load: the pooled run finished with a 1-minute load average of 2.04 (a
  concurrent panel was active); the per-attempt and capped runs finished at
  0.17 and 0.27. The two headline runs are therefore *not* load-matched. They
  agree to the byte anyway, and the accounting is absolute block counting
  rather than a timing-sensitive slope, so load cannot move it.

## Artefacts

Console logs, all four boots plus the smoke boot, in `evidence/dialresidue/`,
gzipped because the full site listings run to 4.3 MB uncompressed (every other
log in `evidence/` is under 91 kB, so committing them raw would have doubled
this directory):
`heapattr-run1.log.gz` (pooled), `heapattr-perattempt1.log.gz` (per-attempt),
`heapattr-nsdepth8.log.gz` (queue capped at 8), `heapattr-sites.log.gz`
(low site threshold), `heapattr-smoke.log.gz` (the 180 s boot that exposed the
`calloc` double-count). Every number in this document was read from these
files with `repro/dialresidue/analyse.py`.

Rig sources in `repro/dialresidue/`: `heapattr.c`, `build-heapattr.sh`,
`apply-mutation.py`, `apply-perattempt.py`, `apply-nsdepth.py`,
`apply-nsdepth-revert.py`, `analyse.py`. On the box they live in
`~/rtems-bringup/` with `heapattr.c` staged into
`~/epics-rs/crates/epics-rtems-boot/csrc/`; the `.heapattr-orig` /
`.pooled-orig` backups beside each mutated file restore the tree.
