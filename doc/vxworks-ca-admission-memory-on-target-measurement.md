# What a CA client costs on VxWorks 7, and where the failing 64-byte allocation comes from

Measured on the qemu VxWorks 7 rig (`wrsdk-vxworks7-qemu-1.17.0`,
`itl_generic_3_0_0_5`, x86_64, single vCPU) with the `-Wl,--wrap` live-block
heap shim described in `doc/vxworks-dial-attempt-residue-on-target-measurement.md`.
Every number below is a reading; nothing here is scaled, extrapolated or
inferred from a Linux run. Where a question could not be answered by
measurement it is marked as unanswered rather than estimated.

The question came from the refusal-fidelity panel: on a 1024M guest their IOC
died with `memory allocation of 64 bytes failed` → signal 6 at `held=41`
(attempt 42, CAS-client 46) before it could reach its refusal path, while 1280M
and 2048M guests climbed to 141 and refused cleanly. Three things were wanted:
what consumes address space and heap up to the wall, which allocation is the
failing 64-byte one, and whether the 1024M/1280M difference is linear or a
threshold.


## 1. What one CA client costs

Three different costs, three different sizes, measured three different ways.

| cost | per client | how it was read |
| --- | --- | --- |
| declared task stack | **3,145,728 B** | `rtpShow` + the c6 `STACKUSE` probe: `CAS-client N size=2097152`, `CAS-event N size=1048576` |
| committed memory | **1,076,992 B** | `RTEMS:MEM_USED` 16,998,400 → 51,462,144 across 32 ramp connections |
| malloc heap | **13,142 B** | shim live-block total 426,182 → 921,802 B across 37 clients |
| guest RAM actually needed | **4.859 MB** | slope of the RAM sweep in §3 |

Each admitted client is exactly **two tasks**, confirmed by `rtpShow`'s task
count moving in twos across four arms — 251 / 253 / 255 / 257 tasks at 111 /
112 / 113 / 114 held connections, i.e. `2*(held+5 monitors) + 19 base tasks`.

The declared stacks are almost entirely unused. Measured high-water marks:

    CAS-client N   size=2097152  high=10320..12136  margin=2085016
    CAS-event  N   size=1048576  high=4800         margin=1043776

16,936 B of 3,145,728 B ever touched — the declaration is **185.7×** the
observed high-water use. The 19 base tasks declare 29,425,664 B between them
and touch at most 270,224 B (`cbMedium`), the rest sitting between 3,296 and
13,344 B.

That is the single most important number for a memory-bounded pool: the budget
that matters is the *declared* stack, not what a client touches, and it is
almost two orders of magnitude larger than the traffic justifies.


## 2. Where the heap goes, per site

Full `HEAPSITE`/`HEAPSIZE` tables were dumped once per second for the whole
ramp (`heapresidue_report(seq, 1)`), so the decomposition below is a difference
between two live snapshots: seq 1 (zero clients) and seq 22 (37 clients,
steady). Divide by 37 for the per-client column. PCs resolved with `addr2line`
against `ca-e11-unstripped.vxe`.

| B/client | blocks | site |
| --- | --- | --- |
| 4096.0 | 2 | `pthread_key_allocate_data` — `pthreadLib.c:9058`, 2048 B each |
| 2992.0 | 2 | `mpmc::Sender<worker_pool::Assignment>::send` → `Block<Assignment>`, 1496 B each |
| 2128.0 | 2 | `_InitialiseModuleTLSArea` — `tlsLibCommon.c:126`, 1064 B each |
| 1152.0 | 3 | `libc::vxworks::posix_memalign` — libc 0.2.185 `vxworks/mod.rs:2451` |
| 416.0 | 2 | `std::thread::Thread::new`, 208 B each |
| 286.5 | 7.2 | `semMCreate` — `semLib.c:423`, 40 B each |
| 250.8 | 3.1 | `OnceBox<Mutex>::initialize` — `sys/sync/mutex/pthread.rs:23` |
| 240.0 | 6 | `semBCreate` — `semLib.c:224`, 40 B each |
| 224.0 | 2 | `_taskCreate` — `taskLib.c:1289`, 112 B each |
| 224.0 | 2 | `pthread_create` — `pthreadLib.c:6474`, 112 B each |
| 186.8 | 2.0 | `RawVecInner::finish_grow` (second monomorphisation) |
| 160.0 | 2 | `std::thread::lifecycle::spawn_unchecked` closure box, 80 B each |
| 160.0 | 2 | `taskOpenWithGuard` — `taskLib.c:788`, 80 B each |
| 128.0 | 2 | `RawVecInner<System>::finish_grow` — **the 64-byte site, see §4** |
| 96.0 | 2 | `mpmc::context::Context::new`, 48 B each |
| 96.0 | 2 | `spawn_unchecked` `Packet`, 48 B each |
| 72.0 | 1 | `BlockingCaServer::serve` → `WorkerPool::spawn_set` → `SetHandle` |
| 32.0 | 1 | `BlockingCaServer::serve` |
| 32.0 | 2 | `taskLib`, 16 B each |
| 26.5 | 2 | `0x5484be`, 13 B each |

Total 13,142 B/client. **104 B of it — the `SetHandle` and one 32 B block — is
CA session state.** The other 99.2 % is the cost of creating two threads:
pthread keys, the module TLS area, the task control blocks, the semaphores, and
the worker-pool channel block. A CA client on this port is not an expensive
data structure; it is two threads wearing a data structure as a hat.

Set against the committed figure, the malloc heap is 13,142 of 1,076,992 B —
**1.2 %**. Bounding a client pool on heap usage would bound the wrong 1.2 %.


## 3. Linear or threshold: linear, R² = 0.998

Guest RAM was the only variable between arms — same image, same shim object,
same driver, `-m` changed and nothing else. Ceiling 200, one connection per
0.25 s, first refusal ends the arm.

| guest RAM | held | total clients | outcome |
| --- | --- | --- | --- |
| 768M | — | — | RTP loads, IOC never prints; see §6 |
| 832M | — | — | same |
| 896M | 1 | 6 | `EAGAIN` refusal |
| 1024M | 32 | 37 | `EAGAIN` refusal |
| 1152M | 59 | 64 | `EAGAIN` refusal |
| 1280M | 82 | 87 | `EAGAIN` refusal |
| 1408M | 110 | 115 | `EAGAIN` refusal |
| 1536M | 134 | 139 | `EAGAIN` refusal |
| 2048M | 136 | 141 | **`worker pool at capacity`** |

Least squares over the six memory-limited arms:

    total_clients = 0.20580 * RAM_MB - 175.59        R² = 0.99825
    residuals (clients): -2.8, +1.9, +2.5, -0.8, +0.8, -1.5
    => 4.859 MB of guest RAM per CA client

Increments per +128 MB are 31, 27, 23, 28, 24 — scatter of ±15 % around a
constant, with no step anywhere. **It is linear, not a threshold.** The
appearance of a threshold between 1024M and 1280M is the linear supply curve
crossing `CAS_CLIENT_POOL_CAPACITY`: at 2048M the refusal text changes from
`resource unavailable try again (os error 11)` to `worker pool at capacity` and
the count stops at exactly 141 clients, which is the cap and not a memory
limit.

The refusal itself is graceful in every memory-limited arm: `pthread_create`
returns `EAGAIN`, the server answers `CA_PROTO_ERROR` status 48 with
`CAS: no resources for a new client (resource unavailable try again (os error 11))`,
and every already-held connection keeps answering `READ_NOTIFY`.

Ramp rate does not move the wall. At 1024M, one connection per second and one
per 0.03 s both stop at held=32 with the identical refusal frame.


## 4. The failing 64-byte allocation

It is Rust std's per-thread TLS destructor list:

    std::sys::thread_local::destructors::list::register
        library/std/src/sys/thread_local/destructors/list.rs:14
      -> Vec::<(*mut u8, unsafe extern "C" fn(*mut u8))>::push
      -> RawVec::<(*mut u8, unsafe extern "C" fn(*mut u8)), System>::grow_one
      -> RawVecInner::<System>::grow_amortized
      -> RawVecInner::<System>::finish_grow          (pc 0x21c355)

The element is 16 B and `RawVec::MIN_NON_ZERO_CAP` is 4 for element sizes up to
1024, so the first push on an empty list allocates **exactly 4 × 16 = 64 B**.
The disassembly shows it directly: `mov $0x4,%r14d` / `cmp $0x5,%rsi` in
`grow_amortized` at `0x21c39f`.

Two independent measurements agree that this is the 64-byte allocation on the
admission path:

* the site holds 93 live blocks of exactly 64 B at 37 clients, growing by
  exactly 2 per client — one for the `CAS-client` task, one for `CAS-event`;
* of the fifteen sites in the image whose live blocks are all exactly 64 B,
  this is the only one whose live count moves with the client count. The other
  fourteen hold 1 to 12 blocks and do not change across the whole ramp.

**Why this aborts instead of refusing.** The allocation happens *inside the
newly created thread*, after `pthread_create` has already returned success. Up
to that instant a shortage is reportable: `spawn_set` sees the error and the
server answers `CA_PROTO_ERROR`. One instruction later the thread exists, and
the first thing it does is an infallible `Vec::push` in std's own TLS
machinery; std has no fallible form of it, so a null return goes to
`handle_alloc_error` and takes the whole RTP down with signal 6.

So the two observed outcomes are the two sides of one boundary. Whichever of
the two reservations is the last one to fit decides the failure mode:

* the *stack* reservation fails first → `EAGAIN` → clean refusal;
* the stack reservation just fits and the *64 B heap block* fails → abort.

The consequence for a fix is structural, not a matter of handling the spawn
result better: **admission has to be decided before `pthread_create` is
called.** No error handling downstream of a successful spawn can convert this
into a refusal, because the failing allocation is not on a path the server
controls. A pool bounded by summed reserved memory does exactly this, and the
budget item it needs is the 3,145,728 B of §1, not the 13,142 B of heap.


## 5. What was not reproduced

**The abort itself did not occur on this rig.** Eighteen arms reached the
server and drove a ramp: sixteen on the instrumented image with the `HEAPFAIL`
hook live, two on the shim-free control build. Every one of them refused
gracefully, at every guest size from 896M to 2048M. The hook — it fires on any
null return from any of the six wrappers and prints the size, the call site and
a full live-block dump — printed no line in any arm (`grep -l HEAPFAIL` over
the whole log directory: 0 files).

The reason is consistent with §4: `pthread_create` needs 3 MiB of address space
while the 64 B block usually comes out of a mimalloc segment that is already
reserved, so the stack reservation loses first and the graceful path wins. For
the abort to happen, the 64 B request has to coincide with a mimalloc segment
boundary. That did not happen here.

The identification in §4 therefore rests on live-block accounting plus static
disassembly, not on catching the failing call. That is stated as the limit of
the evidence.

A shim-free control build (same source, same features, no `-Wl,--wrap`, no
report call) was measured to check that the instrumentation is not what decides
the failure mode:

| build | 1024M | 1280M |
| --- | --- | --- |
| instrumented (`ca-e11`) | held 32 | held 82 |
| control (`ca-ctl`) | held 34 | held 81 |

The shim costs between −1 and +2 clients, and the control refuses gracefully
too. The instrumentation is not the reason this rig does not abort.

The other panel's build reaches 41 at 1024M and 141 at 1280M, which is not on
this line. Two different builds cannot be reconciled from one side, and their
tree was read-only for this work, so no claim is made about which term of the
fit differs. What transfers is the **slope** — the per-client cost of §1 — not
the intercept.


## 6. Open

* 768M and 832M guests load the RTP (`rtpSp` returns a valid id) and then print
  nothing at all. No `HEAPFAIL`, no panic, no console output after the load.
  Unexplained; below the useful range but not diagnosed.
* Above roughly 40 held connections the IOC's own probe thread is starved by
  the `CAS-client`/`CAS-event` band and the console stops advancing, so the
  per-second heap dumps cover only the first ~40 clients of a long ramp. The
  §2 decomposition is measured at 37 clients for that reason. Per-connection
  accept latency over the same range grows from 0.02 s to 7.36 s.
* `RTEMS:CA_REFUSED_CNT` read 0.0 immediately after every refusal measured
  here, in every arm. Not investigated — it belongs to the refusal-fidelity
  panel's row, and is recorded here only because it was seen.
* A first pass of the RAM sweep produced an apparent RAM-independent death at
  held=112, reproduced three times. It was **an artefact of this rig**:
  `boot-e10.sh`'s watchdog (`bootsecs=290`) expired mid-ramp and killed qemu at
  the same wall-clock in all three arms, which looks from the client exactly
  like the IOC dying — `ACCEPTED_THEN_EOF` then `ECONNRESET` on every held
  connection. With the watchdog scaled to the ceiling, 2048M holds 111, 112,
  113 and 114 connections without incident. The finding is retracted; the
  corrected arms are the ones in §3.


## Artefacts

Rig, under `doc/vxworks-e10-rig/`:

* `heapresidue.c` — the `--wrap` shim, now with the `HEAPFAIL` reporter
* `build-shim-e10.sh` — the shim's compile recipe and its `.tbss` guard
* `apply-e11.py` — the minimal two-edit mutation set for this round
* `boot-e10.sh` — guest RAM is now the `VXRAM` parameter
* `run-arm-e11.sh` — one arm: boot, wait for the IOC, drive, post-mortem, stop
* `rampprobe-e11.py` — the paced ramp driver, port 25064

Console and probe logs, gzipped, under `doc/vxworks-e10-rig/evidence-e11/`:
the 1024M full console (the source of §2), the probe log of every arm in §3,
the `rtpShow`/`STACKUSE` extracts behind §1, both control-build arms, and the
768M non-boot console.

The protocol framing in `rampprobe-e11.py` is taken from the refusal-fidelity
panel's `doc/vx-rig-e11/refusalprobe.py` on branch
`caucus/58EWEJWV91/refusal-fidelity-494e4108-1`, read with `git show`. Nothing
was merged or cherry-picked.
