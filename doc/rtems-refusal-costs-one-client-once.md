# The 1.6 MB that a refusal costs is paid once, not per refusal

Measured 2026-07-27 on gv100, `xilinx_zynq_a9_qemu`, image `caioc-e10`. Follow-up
to the open item in `doc/rtems-thread-heap-wall.md`. Handed to
**refusal-fidelity**, whose accounting invariant this is.

## The observation that needed isolating

In the sq3 arm, `MEM_FREE` was 9,066,552 B after the 110th client was admitted
and 7,450,640 B after the 111th was refused — a drop of **1,615,912 B**, almost
exactly one client's 1,592,296 B, and it had not come back 25 s later. The same
shape appeared in rf1 (1,602,912 B) and rf2 (1,590,576 B). If a
`pthread_create` that fails with `EAGAIN` leaves a half-created worker behind,
that is a leak on the refusal path and every refused client costs a client's
worth of heap.

## Method

Squeeze the heap with server-side channel state until the client ramp walls
below the pool cap, then keep attempting. Every attempt after the wall is
refused on the same path; the question is only whether the heap keeps falling.
Each attempt is followed by a 12 s quiesce and a freshness-checked sample, so
the readings are not the stale ones the low-priority pusher would otherwise
give (see the staleness note in the companion document).

`rtemssqueeze-e10.py 8 16000 0 0 rf2 12 131 110 8` — 128,000 channels, ramp to
the wall, then eight further attempts.

## Result: one drop, then flat

The ramp walls at held=113 and the first refused attempt costs one client:

| point | `MEM_FREE` | delta |
| --- | --- | --- |
| ramp-113 (last admitted) | 4,209,496 | — |
| after the first refusal | 2,618,920 | **−1,590,576** |

The next eight refusals cost nothing:

| refusal | `MEM_FREE` | delta |
| --- | --- | --- |
| 1 | 2,618,760 | −160 |
| 2 | 2,606,336 | −12,424 |
| 3 | 2,606,336 | 0 |
| 4 | 2,605,672 | −664 |
| 5 | 2,605,672 | 0 |
| 6 | 2,605,672 | 0 |
| 7 | 2,605,520 | −152 |
| 8 | 2,605,528 | +8 |

**13,392 B total across eight refusals**, against 1,590,576 B for the first.
`CA_REFUSED_CNT` advances 2 → 9 and `CA_CONN_CNT` stays at 122 throughout, so
all eight were genuine refusals on the same `EAGAIN` path, each returning
`(48, 'CAS: no resources for a new client (No more processes (os error 11))')`.
The guest console carries nine `refused a CA client` lines and no panic.

**So it is not a leak.** A leak on the refusal path would have shown eight more
1.59 MB drops, and there is not enough heap left at that point for even two of
them.

## What the one-time cost is, and what this measurement does not show

The single 1.59 MB is one client's allocation, retained. The reading consistent
with every number here is that the first refusal is the attempt that actually
performs a thread creation — one of the two threads a client needs succeeds and
the second fails — and the worker pool retains the created worker, as it retains
workers after a normal disconnect. Later attempts then find that worker already
present, fail again on the second thread, and allocate nothing.

That is inference from the accounting, not from the internals: this arm measured
heap totals over CA, not the pool's contents. Confirming it needs a look at what
`WorkerPool` holds after a failed spawn, which is refusal-fidelity's file. The
measured facts are only these — one client's worth is consumed at the transition
into refusing, at most 12,424 B by any refusal after it, and the IOC keeps
serving throughout.

## Evidence

`doc/vxworks-e10-rig/evidence-e10-heap/rf2.log.gz`,
`rf2.console.log.gz`. Harness: `doc/vxworks-e10-rig/rtemssqueeze-e10.py`
(`REFUSE_REPEAT`, the ninth positional argument).
