# The CA admission wall against declared stack size, measured on VxWorks 7

Six booted images, one guest RAM, one probe. What varies is the declared stack
class of the two threads a CA client borrows. Measured on
`x86_64-wrs-vxworks` under QEMU on gv100, 2026-07-27.

This closes the gap e10-residue named twice: the earlier wall numbers were
taken against the client **count** only, so nothing in the tree established how
the wall moves with the declared stack **size**. `0176e2ac` took CAS-client from
`Big` to `Medium` on the strength of an unmeasured linearity assumption
("a thread costs its declared stack in reserved address space"); that
assumption is tested here and is **wrong in the form stated** — the wall is not
linear in declared stack size, though the underlying per-set cost is
declared-stack-plus-a-constant, which is the form the accounting wants.

Nothing in this measurement required a committed code change. The class edits
were working-tree edits on the rig, restored afterwards by `cp` from a backup.

## Method

`doc/vx-rig-e8/stackclass.sh` runs one point end to end: rewrite the `stack:`
class of a role in `client_roster()`
(`crates/epics-ca-rs/src/server/blocking.rs`), build
`realtime-ca-ioc.vxe`, boot the guest at a fixed `-m`, load the RTP, ramp CA
connections until the server stops taking them, then stop only its own recorded
pids. Held fixed across all six points: guest RAM `1024M` (the guest reports
`OS Memory Size: ~958MB`), the probe `doc/vx-rig-e8/phaseramp.py` with ceiling
200 and no hold, and the rig tree at `43ff13c7` plus that tree's own
uncommitted probe edits.

The declared class is confirmed twice per point, because a build that silently
kept the previous class is the one failure that would make the whole series
meaningless: once on the source before the build, and once **on target** from
the image's own C6 census, which prints the size the OS actually gave the task:

    STACKUSE tag=c6-6 id=0x000100b9 name=CAS-client 0 size=2097152 current=1536 high=12160 margin=2084992
    STACKUSE tag=c6-6 id=0x000100bd name=CAS-event 0 size=1048576 current=1536 high=5456 margin=1043120

Two of the six points cannot show that census from their own wall run — the one
that dies at the wall has no live task left to enumerate, and the one that
wedges reports `state=gone` for all 181 registry entries by the time the census
fires. Both were re-booted at a load well below the wall purely to record the
census (`census` mode, `evidence-*-cen.txt`); the wall numbers below are from
the original runs and were not perturbed to get the proof.

A "set" is one CA client: two threads, `client` and `event`. The five monitor
connections the probe holds for `RTEMS:{CA_CONN_CNT,CA_REFUSED_CNT,FD_CNT,FD_MAX,MEM_USED}`
are ordinary clients and are counted.

`StackSizeClass::bytes` is `f × 0x10000 × size_of::<usize>()`, and `usize` is 8
here, so `Small` = 524,288, `Medium` = 1,048,576, `Big` = 2,097,152 B.

## Measured

| client | event | declared per set | wall (sets) | first failure |
|--------|-------|-----------------:|------------:|---------------|
| Small  | Small  | 1,048,576 | 80 | handshake timeout, then EAGAIN refusals |
| Small  | Medium | 1,572,864 | 67 | EAGAIN refusal |
| Medium | Medium | 2,097,152 | 58 | EAGAIN refusal |
| Big    | Small  | 2,621,440 | 53 | fatal `memory allocation of 81 bytes failed` |
| Big    | Medium | 3,145,728 | 49 | EAGAIN refusal |
| Medium | Big    | 3,145,728 | 49 | EAGAIN refusal |

Row 3 is what the tree ships after `0176e2ac`. Verbatim, from
`doc/vx-rig-e8/logs-stackclass/phaseramp-scMedium.log`:

    [    2.6s] scMedium D1 client-side served = 53 ramp + 5 monitor = 58
    [    2.7s] scMedium first failure verbatim: REFUSED_BY_SERVER(CA_PROTO_ERROR:CAS: no resources for a new client (resource unavailable try again (os error 11))|hex=000b006800000000ffffffff00000030000000000000000000000000000000004341533a206e6f207265736f757263657320666f722061206e657720636c69656e7420287265736f7572636520756e617661696c61626c652074727920616761696e20286f73206572726f72203131292900000000000000)

`os error 11` is `EAGAIN` from `pthread_create`: the wall is reached when the OS
refuses another thread, and the driver turns that into the documented
`CA_PROTO_ERROR` refusal rather than a silent close. Four of the six points end
that way. The two that do not are noted below and are not treated as the same
resource without saying so.

### The wall follows total declared bytes, not the client thread's class

Rows 5 and 6 carry the same 3,145,728 B per set with the bytes on opposite
threads, and both refuse at **49 sets** — equal to the set, same failure text.
Row 4 moves 1,572,864 B off the event thread while leaving `client` at `Big`,
and the wall moves 49 → 53. So the cost is a property of the set's total
declared stack and is symmetric in which of the two threads declares it. Any
accounting that charges per thread rather than per class is therefore charging
the right shape.

### The wall is not linear in declared stack size

Halving the client's stack twice buys exactly nine sets each time, for
different byte savings:

* `Big` → `Medium`: −1,048,576 B per set, 49 → 58 (+9)
* `Medium` → `Small`: −524,288 B per set, 58 → 67 (+9)

A straight line `N = a + b·D` fitted to all six points gives slope
−1.382828 × 10⁻⁵ sets/B, intercept 90.75 sets, R² = 0.9454, and residuals
`+3.75, −2.00, −3.75, −1.50, +1.75, +1.75` — a systematic curve, not scatter.
The wall is a reciprocal, not a line: what is linear is the **cost per set**.

### Fit: each set costs its declared stack plus a constant

Fitting `1/N = D/B + K/B` over all six points (`doc/vx-rig-e8/stackclass-fit.py`):

    budget            B = 271,038,782 B  (258.48 MiB)
    per-set overhead  K =   2,441,947 B  (2.329 MiB)  -> 1,220,973 B per thread

    config                         declared  wall    pred   resid
    client=Small event=Small        1048576    80   77.65   +2.35
    client=Small event=Medium       1572864    67   67.51   -0.51
    client=Medium event=Medium      2097152    58   59.71   -1.71
    client=Big    event=Small       2621440    53   53.53   -0.53
    client=Big    event=Medium      3145728    49   48.51   +0.49
    client=Medium event=Big         3145728    49   48.51   +0.49
    R^2 (on N) = 0.9872    max |resid| = 2.35 sets

R² = 0.9872 with a worst error of 2.35 sets out of 80. The parameters are **not**
identified to that precision, and this must travel with the numbers: solving
`N₁(D₁+K) = N₂(D₂+K)` on adjacent pairs gives K rising monotonically with the
pair's declared size — 1,653,524 / 1,805,881 / 3,460,301 / 3,801,088 B per set
across the four adjacent pairs, a 2.3× spread. The effective per-set overhead
grows somewhat with the declared stack, so `K = 2,441,947 B` is a mid-range
value over 1,048,576–3,145,728 B declared, not a constant of nature. Quote it
with the bracket.

## Bearing on the `per_thread_overhead` over-charge

refusal-fidelity is reducing a charge of 3,670,016 B per thread against a
measured RTEMS figure of 1,167,383 B (3.1×). This series is independent
evidence: a different OS, a different architecture, a tree that contains no
`per_thread_overhead`, no `ThreadMemoryTarget` and no
`EPICS_RS_POOL_RESERVATION_MB`, so the wall measured here is the raw OS ceiling
rather than our own accounting's.

The measured VxWorks non-stack cost per thread is **1,220,973 B** — 4.6% above
RTEMS's measured 1,167,383 B, and 3.006× below the 3,670,016 B being charged.

Read as an overhead added to the declared stack, against the 271,038,782 B
budget:

| per-thread overhead | per set | sets admitted | vs 58 sustained |
|---|---:|---:|---|
| 3,670,016 B (charged now) | 9,437,184 | 28.7 | refuses 29 sets early |
| 1,167,383 B (RTEMS measured) | 4,431,918 | 61.2 | over-admits by 3 |
| 1,220,973 B (this fit) | 4,539,098 | 59.7 | over-admits by 2 |

So the direction of the fix is confirmed by target measurement on a second
platform: the number to charge is near 1.2 MB, not 3.5 MiB, and the present
charge would refuse at roughly half the concurrency the target sustains. The
form is confirmed too — declared stack plus a per-thread constant, symmetric
between the two threads of a set.

Two cautions belong with that. The 1,220,973 B is a **fitted intercept**, not a
directly observed per-thread allocation, and the adjacent-pair bracket above
spans 826,762–1,900,544 B per thread; it agrees with the RTEMS measurement but
does not independently confirm its precision. And 1,167,383 B read as a
*total* per-thread cost rather than an overhead on top of the declared stack
predicts 116 sets against 58 measured, so the two readings of that number are
not interchangeable.

## What is not the same resource

Two of the six wall runs did not end in an EAGAIN refusal, and their numbers are
reported above without pretending otherwise:

* **client=Big, event=Small (53 sets)** ends fatally. The 53rd set was created
  and then an 81-byte Rust allocation failed on the accept thread:

        memory allocation of 81 bytes failed
        0xffff800008c86c00 (CAS-TCP): RTP 0xffff800008c42000 has been deleted due to signal 6.

  The heap ran out before `pthread_create` did, so at this configuration the
  refusal path is unreachable and the process aborts instead. That is a
  refusal-fidelity gap in its own right, not a measurement artefact.

* **client=Small, event=Small (80 sets)** wedges before it refuses: the 81st
  set was leased and its handshake never completed (20 s timeout), and the next
  two attempts got clean EAGAIN refusals. `POOLPROBE seq=1 BUSY=80 SETS=81
  CAP=141 WORKERS=162 REFUSED=0 CONNS=80` — the IOC survived. Counted as 80
  sustained sets, with the caveat that the first failure at this configuration
  is a leased-but-dead set rather than a refusal.

A seventh run at `1152M` was discarded as a wall measurement for the same
reason: the failure there is a 20 s handshake timeout at 100 held clients with
the IOC alive and `SETS=106 CAP=141`, so it is not the thread-creation wall and
cannot be regressed against these six.
(`doc/vx-rig-e8/logs-stackclass/phaseramp-scSmall1152.log`.)

At `1536M` and `2048M` the wall is `CAS_CLIENT_POOL_CAPACITY = 141` — the
refusal text is `worker pool at capacity`, not `resource unavailable` — so a
RAM sweep at the shipped classes cannot measure the OS ceiling above `1024M`
at all. That is why this series varies the stack at fixed RAM rather than the
reverse.

## Reproducing

    ssh coding-agent@192.168.2.128
    cd ~/vx-rig-e8
    ./stackclass.sh Medium 1024M scMedium                 # one wall point
    ./stackclass.sh Big    1024M scBigEvSmall     Small   # bytes on the other thread
    ./stackclass.sh Big    1024M scBigEvSmall-cen Small census
    ./stackclass.sh restore                               # cp back, never git checkout
    python3 stackclass-fit.py

Transcripts: `doc/vx-rig-e8/logs-stackclass/`. Each point has its ramp log
(`phaseramp-<tag>.log`, one line per connection attempt with the handshake split
into connect / search / create / read) and the console evidence
(`evidence-<tag>.txt`: the guest's reported OS memory, the pool census at the
wall, and the on-target declared sizes).
