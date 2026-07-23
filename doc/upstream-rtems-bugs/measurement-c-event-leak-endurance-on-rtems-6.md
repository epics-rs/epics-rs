# Measurement — endurance: does the 5 blocks / 160 B per cycle slope hold past 600?

**Answer: yes, exactly. Across 3000 connect/disconnect cycles — five times the
600-cycle maximum of the bytes measurement — the leak is 5.000 blocks and 160.0
bytes per cycle in every segment, byte-exact, with zero failed allocations and a
thread census pinned at the idle 19. The used-heap slope does not bend at scale.**
Two allocator-internal counters do drift non-linearly (the free-block count and
the internal free-list search effort); neither is a second leak, and both are
recorded below so the drift is on the record rather than assumed absent.

This closes the "No endurance run: the longest continuous run was 600 cycles"
Limit in both
[`c-base-rtems-posix-event-leak.md`](c-base-rtems-posix-event-leak.md) and
[`measurement-c-event-leak-bytes-on-rtems-6.md`](measurement-c-event-leak-bytes-on-rtems-6.md),
and turns that document's §7 extrapolation from arithmetic-on-a-600-cycle-slope
into a measured 3000-cycle slope.

Taken **2026-07-24** on `coding-agent@192.168.2.128`, `qemu-system-arm -M
xilinx-zynq-a9`, RTEMS 6, BSP `xilinx_zynq_a9_qemu`, libbsd, 256 MB guest. Stock
image `~/rtems-cside/cioc-fd64.exe`,
sha256 `10a4db99c63159423a4d7bda2d6db5f1d57dcf73f6a7dc59d5aabc8f19e3efa1` — the
**same image** as the bytes measurement, zero source patches. Driver
[`repro/evleak/evendure.py`](repro/evleak/evendure.py): the identical external
server-side cycle as `evleak.py` (connect → version+names → recv 16-byte reply →
close), concurrency 1, 40 ms gap, 60 warm-up before baseline. Raw log:
[`evidence/cioc-evendure-3000-2026-07-24.log`](evidence/cioc-evendure-3000-2026-07-24.log).

---

## 1. The leak slope holds byte-exact to 3000 cycles

| reading | cycle | used blocks | bytes used | successful allocs | successful frees | failed allocs | threads |
|---|---|---|---|---|---|---|---|
| `E0` baseline | 0 | 18,751 | 32,303,896 | 20,091 | 1,340 | 0 | 19 |
| `E-at-500` | 500 | 21,251 | 32,383,896 | 29,597 | 8,346 | 0 | 19 |
| `E-at-1000` | 1000 | 23,751 | 32,463,896 | 39,103 | 15,352 | 0 | 19 |
| `E-at-2000` | 2000 | 28,751 | 32,623,896 | 58,109 | 29,358 | 0 | 19 |
| `E-at-3000` | 3000 | 33,751 | 32,783,896 | 77,115 | 43,364 | 0 | 19 |

Per-segment slope — four independent segments, two of 500 and two of 1000 cycles:

| segment | cycles | Δ used blocks | **blocks/cycle** | Δ bytes used | **bytes/cycle** | Δ allocs | Δ frees | **net/cycle** |
|---|---|---|---|---|---|---|---|---|
| `E0→500` | 500 | 2,500 | **5.000** | 80,000 | **160.0** | 9,506 | 7,006 | 2,500 → **5.00** |
| `500→1000` | 500 | 2,500 | **5.000** | 80,000 | **160.0** | 9,506 | 7,006 | **5.00** |
| `1000→2000` | 1000 | 5,000 | **5.000** | 160,000 | **160.0** | 19,006 | 14,006 | **5.00** |
| `2000→3000` | 1000 | 5,000 | **5.000** | 160,000 | **160.0** | 19,006 | 14,006 | **5.00** |
| **whole run** | **3000** | **15,000** | **5.000** | **480,000** | **160.0** | — | — | **5.00** |

Every segment is equal to the digit. Each cycle makes 19.01 allocations, returns
14.01, permanently retains **exactly 5.00** — the same three figures the 600-cycle
measurement reported, now unmoved after 3000. `Total bytes free` mirrors it at
−160.0 B/cycle (228,490,016 → 228,010,016). **`Failed allocations` is 0 at every
reading**; the guest's ~228 MB free area is nowhere near exhausted at 3000 cycles,
so no allocation-pressure artefact perturbs the slope. **Thread census is 19 at
every milestone** — the idle set — so at scale, as at 600, the growth is retained
heap, not lingering threads.

## 2. What DID drift — two allocator-internal counters (neither a leak)

**Free-block count grows non-linearly.** It is a clean 1.756/cycle for the first
1000 cycles, then bends:

| segment | Δ free blocks | free blocks/cycle |
|---|---|---|
| `E0→500` | 878 | 1.756 |
| `500→1000` | 878 | 1.756 |
| `1000→2000` | 602 | **0.602** |
| `2000→3000` | 1,005 | **1.005** |

The bytes measurement saw the flat 1.75/cycle over its 250–600 window and read it
as linear; past 1000 cycles it is not. This is free-list fragmentation
book-keeping — each retained 32-byte block strands a free fragment, but whether
adjacent frees coalesce varies — not a growth in retained memory (`Total bytes
free` moves by the flat −160/cycle throughout). `Largest free block` also erodes
slowly and non-monotonically, 228,460,968 → 226,595,976 (≈ 1.86 MB over 3000, with
the `500→1000` segment flat), a fragmentation nibble at the top block.

**Internal free-list search effort spikes super-linearly.** `Total number of
searches` (the allocator's own free-list walks) is the one counter that swings
hard:

| segment | Δ searches | searches/cycle |
|---|---|---|
| `E0→500` | 232,772 | 466 |
| `500→1000` | 671,631 | 1,343 |
| `1000→2000` | 4,593,170 | **4,593** |
| `2000→3000` | 1,532,708 | 1,533 |

`Maximum number of blocks searched ever` climbs 65 → 875 → 1,753 → 2,354 and then
plateaus at 2,359. The takeaway: the leak is not only retained memory, it makes
each subsequent allocation progressively **more expensive to service** as the
free list lengthens — a CPU-cost drift, non-linear and not captured by the byte
slope. It is bounded here (no failed allocation, search-depth plateaus), but it is
real and would compound on a longer run.

`Successful resizes` moved only 44 → 52 across the whole run (+2 per reading),
which tracks the five `rt malloc`/`epicsThreadShowAll` readings themselves, not
the cycle.

## 3. Limits

* **3000 cycles, one boot, one image, one BSP, single core.** Longer than any
  prior run by 5×, but still far from heap exhaustion; §7's ~19-day
  reconnect-once-a-second figure in the bytes document remains an extrapolation,
  now on a 3000-cycle rather than 600-cycle slope. No run to actual
  `failed allocations > 0` was performed.
* **The drift counters (§2) are allocator-internal and BSP-specific.** The
  free-block and search-effort behaviour is the RTEMS heap's, and would differ on
  another allocator or BSP; only the used-block / bytes-used leak slope is a
  property of the base defect.
* **Server-side cycle only.** This is the rsrv client cycle (5 blocks); the libca
  client circuit could not be endurance-run at all — it walls on libbsd sockets at
  ~200 cycles (see the libca-circuit measurement), so no libca endurance figure
  exists.
