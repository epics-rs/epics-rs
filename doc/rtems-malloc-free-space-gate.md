# `malloc_free_space()` tracks the RTEMS wall exactly, and is still 37–55× too low as a gate

Measured 2026-07-27 on gv100, `xilinx_zynq_a9_qemu`, image `caioc-e10`. Pair to
`doc/rtems-thread-heap-wall.md`. Handed to **refusal-fidelity**.

## The identity chain, verified end to end

Three separate sources say the same field:

```c
/* rtems-bringup/kernel/cpukit/libcsupport/src/mallocfreespace.c */
size_t malloc_free_space( void )
{
  Heap_Information info;
  _Protected_heap_Get_free_information( RTEMS_Malloc_Heap, &info );
  return (size_t) info.largest;
}
```

```c
/* epics-base modules/libcom/src/osi/os/RTEMS-posix/osdPoolStatus.c, __RTEMS_MAJOR__ >= 5 */
LIBCOM_API int epicsStdCall osiSufficentSpaceInPool ( size_t contiguousBlockSize )
{
    size_t n = malloc_free_space();
    return (n > (50000 + contiguousBlockSize));
}
```

and `crates/epics-base-rs/src/server/status_pv.rs:215` publishes
`mem_usage().largest_free` — the same `Heap_Information.largest` — as
`RTEMS:MEM_BLK`.

So reading `RTEMS:MEM_BLK` over CA *is* sampling the input to C base's CA
admission gate, with no source change and no instrumentation in the path.

## Does it track? Yes — to the byte, per client

Two ramps, both quiesced (see the staleness note in the companion document).

Unsqueezed, 133 clients, `MEM_FREE` against `MEM_BLK`:

| held | `MEM_FREE` | `MEM_BLK` | gap |
| --- | --- | --- | --- |
| 0 | 232,339,816 | 232,303,600 | 36,216 |
| 40 | 157,500,784 | 157,456,952 | 43,832 |
| 80 | 100,181,712 | 100,126,256 | 55,456 |
| 120 | 34,880,960 | 34,812,128 | 68,832 |
| 133 | 9,403,296 | 9,329,424 | 73,872 |

It falls 232.3 MB → 9.3 MB with the largest free block never more than 74 KB
behind the total. Squeezed, sampling every single client near the wall (sq3),
the per-client decrement of `MEM_BLK` is:

```
1,590,184  1,589,744  1,589,936  1,589,936  1,589,936
1,590,184  1,589,744  1,589,936  1,589,936
```

Nine consecutive steps spanning 440 B. **`malloc_free_space()` is not pinned on
RTEMS.** This is the opposite of VxWorks, where `memFindMax` sat flat at 256 KiB
while the real wall moved, and no RTP query tracked it at all.

## And yet the gate passes at every refusal

The value tracks; the *threshold* does not match what is being admitted. From
the three arms that sampled each step, taking `MEM_BLK` as it stood before each
attempt:

| arm | last attempt that succeeded, from | first attempt that was refused, from |
| --- | --- | --- |
| sq3 | 3,658,208 B | 2,068,272 B |
| sq4 | 4,032,656 B | 2,442,776 B |
| sq5 | 3,885,424 B | 2,294,352 B |

The floor for admitting one more CA client is therefore bracketed at

> **2,442,776 B < floor ≤ 3,658,208 B** of largest free block

— that is 1.55× to 2.33× the 1,572,864 B a client declares, consistent across
three arms with three different fragmentation states (the `MEM_FREE − MEM_BLK`
gap at the wall was 6.45 MB, 1.48 MB and 3.11 MB respectively).

C's gate asks for `50000 + contiguousBlockSize`. At a 16,384 B CA buffer that is
**66,384 B**, so the floor above is **36.8× to 55.1×** the threshold. Evaluated
at the moment of every refusal in every arm:

| arm | `malloc_free_space()` at the refusal | `osiSufficentSpaceInPool(16384)` |
| --- | --- | --- |
| sq2 | 999,656 B | PASS |
| sq3 | 999,488 B | PASS |
| sq4 | 840,176 B | PASS |
| sq5 | 999,656 B | PASS |

On RTEMS, C base's gate would admit the client the OS cannot give a thread to.
It is not a stack-admission gate at that threshold — it only fires once the heap
is down to tens of kilobytes, by which point roughly two clients' worth of
allocation has already failed.

## `MEM_FREE` is not the substitute

The total is worse than the largest block, not better, because the gap opens up
under exactly the workload that matters. Idle it is 37,816 B; after 128,000
channels it is 5.8–7.5 MB; and after the load is released it is **38,481,056 B**
— `MEM_FREE` reports 44,053,888 B free while the largest block is 5,572,832 B.
An admission decision made on the total would be wrong by a factor of eight at
that moment. Only `largest` means anything here, which is the one thing C got
right.

## What this leaves for the budget constant

The measurable inputs, all from `doc/rtems-thread-heap-wall.md`: per client
1,592,296 B measured against 1,572,864 B declared, heap total 260,805,344 B,
free at idle ~232,341,500 B, and an admission floor of 2.44–3.66 MB of largest
free block. Two things follow that the constant has to survive:

1. A budget expressed in bytes reserved per client must reserve at least the
   floor, not the declared stack — a client admitted with 2.44 MB of largest
   block still fails.
2. Any gate that reads a *published* heap number instead of calling the query
   directly reads minutes-old data exactly when it matters: the publisher is at
   `ThreadPriority::Low` and was measured 495 s stale under load. A gate inside
   the worker pool calling `mem_usage()` on the accept path does not have this
   problem; a gate driven off the status PVs would.

## Evidence

`doc/vxworks-e10-rig/evidence-e10-heap/sq3.log.gz` (per-step ramp into the
wall), `sq4.log.gz`, `sq5.log.gz`, `rtemsramp-r256.log.gz` (the unsqueezed
`MEM_FREE`/`MEM_BLK` pair). Harness: `doc/vxworks-e10-rig/rtemssqueeze-e10.py`.
