# The CA server WorkerPool conversion, measured on target

Status: MEASURED 2026-07-23 on the RTEMS/QEMU box (`192.168.2.128`).
Subject: `38ea164d` — *feat(ca-server): borrow every CA client's two threads
from a worker pool* — the on-target half its author left UNFIXED.

Image: `rtems-ca-ioc` built from `84729f21` (which contains `38ea164d`) with
`--no-default-features --features client-core,bringup-probes`, through the
`has-thread-local` target spec ([deviation](rtems-tls-spec-deviation.md)).
One guest, `qemu-system-arm -M xilinx-zynq-a9 -m 256M`, `-serial null -serial
mon:stdio`, `-nic user,…,hostfwd=tcp:127.0.0.1:15064-:5064`, driven from the
host over that forward.

Instrument: [`doc/rtems-ca-pool-probe.patch`](rtems-ca-pool-probe.patch) —
**not merged**. Three readings the shipped image cannot produce:

* `PRIOPROBE` in `enter_ioc_thread`, which every pooled worker passes through,
  reading the band **back off the OS scheduler** (`pthread_getschedparam`)
  rather than reporting what was asked for;
* `POOLPROBE`, the pool's own `set_usage()`/`worker_count()` beside the
  connection count;
* `TASKDUMP`, the RTEMS task census. `rtems_object_get_name` is blind to
  pthread names — the `obj=` column comes back **empty** for every one of our
  threads — so the names are read from `_Thread_Get_name`, the `thread=`
  column, exactly as [the priority
  measurement](rtems-priority-on-target-measurement.md) established.

## 1. Thread census — the pooled workers, their names and their bands

302 `PRIOPROBE` lines over the run. **Every one** reads
`applied=Realtime getsched_rc=0 policy=1` — `SCHED_FIFO`, honoured by the
kernel, on every thread including all 284 pooled workers.

| role | count | distinct labels | `epics=` | measured `posix=` | `56 + epics` | `core=` |
|---|---:|---:|---:|---:|---:|---:|
| `CAS-client N` | 142 | 142 (`N` = 0…141) | 20 | **76** | 76 ✔ | 179 |
| `CAS-event N` | 142 | 142 (`N` = 0…141) | 19 | **75** | 75 ✔ | 180 |

`epics=20` is `ThreadPriority::CaServerLow` and `epics=19` is the
`CaServerLow - 1` the event role takes, i.e. the two bands `client_roster()`
declares — read back off the scheduler on the thread itself, not asserted from
the roster. The port's `posix = 56 + epics` map holds exactly on both.

**142 distinct labels per role, 284 threads, each printing once.** The label is
emitted at thread *creation*, so this is an independent witness of
`POOLPROBE`'s `WORKERS=284`: no worker was ever created twice, and the run's
284 creations are the whole lifetime cost.

The census, at two concurrent clients:

```
TASKDUMP begin tag=c6-12 count=39 scheduler_sc=0
TASKDUMP id=0x0b010009 core=181 posix=  74 sc=0 obj=       thread=CAS-TCP
TASKDUMP id=0x0b01000a core=183 posix=  72 sc=0 obj=       thread=CAS-UDP
TASKDUMP id=0x0b01000b core=150 posix= 105 sc=0 obj=       thread=CAC-dial 0
TASKDUMP id=0x0b010014 core=179 posix=  76 sc=0 obj=       thread=CAS-client 0
TASKDUMP id=0x0b010015 core=180 posix=  75 sc=0 obj=       thread=CAS-event 0
TASKDUMP id=0x0b010016 core=179 posix=  76 sc=0 obj=       thread=CAS-client 1
TASKDUMP id=0x0b010017 core=180 posix=  75 sc=0 obj=       thread=CAS-event 1
```

Both §9 deviations are visible in that listing and both are as recorded: the
name carries the **pool index, not the peer** (`CAS-client 0`, where the
pre-conversion image printed `CAS-client-blocking 10.0.2.2:57688`), and `obj=`
is blank because the kernel object has no name for a pthread.

### 1.1 The two iocsh threads are NOT MEASURED — they do not exist on target

`c04f8469` bands `iocsh-startup` and `iocsh-after-ioc-running` at
`ThreadPriority::Iocsh` = 91, i.e. `posix = 56 + 91 = 147`. **That reading
could not be taken, because neither thread is in any RTEMS image.**

Measured, not inferred: `grep -c 'thread=iocsh'` over every `TASKDUMP` in the
run returns **0**. The cause is structural — both threads are spawned by
`server::ioc_app`, and neither `epics-ca-rs`'s `rtems-ca-ioc` nor
`epics-bridge-rs`'s `rtems-pva-ioc` ever constructs an `IocApp`
(`rtems-ca-ioc` names `ioc_app.rs` once, in a comment). Every RTEMS entry point
drives `IocBuilder` directly, which is consistent with those binaries having no
iocsh at all: `rustyline` does not build for `armv7-rtems-eabihf`.

So `posix 147` is correct arithmetic under the port's map and is enforced by
`iocsh_threads_take_the_iocsh_band` on the host, but it is **unreachable on
this target today**. Listed under UNFIXED in §5.

## 2. Bounded reuse — a 30-cycle ramp adds nothing

The ramp is deliberately **one connection live at a time**. The pool grows to
the concurrent high-water mark, so a *serial* ramp can only add worker sets if
the driver is creating threads per accept instead of borrowing them;
concurrency would confound the two.

Sequence: hold 2 concurrent clients (establishing the high-water mark), release,
then 30 × (connect → CA `VERSION` → reply → close).

| point | `FD_CNT` | `CONNS` | `BUSY` | `SETS` | `WORKERS` | `TASKDUMP count` | heap `USED` |
|---|---:|---:|---:|---:|---:|---:|---:|
| idle baseline | 8 | 0 | 0 | 0 | 0 | 35 | 28,466,624 |
| 2 clients held | 10 | 2 | 2 | 2 | 4 | 39 | 31,648,112 |
| after release | 8 | 0 | 0 | 2 | 4 | 39 | — |
| **after 30 cycles** | **8** | **0** | **0** | **2** | **4** | **39** | **31,628,824** |

Driver: `CHURN done cycles=30 served=30 failed=0 t=30.1s` — every cycle
completed the `VERSION` exchange, so all 30 were served, not merely accepted.

`SETS`, `WORKERS` and the thread count are **identical before and after** the
30 cycles. The heap is 19,288 B *lower* after the ramp than at the two-client
point (the two clients' live per-connection buffers are gone) and sits
3,162,200 B above idle, which is the two retained sets' stacks:
2 × (`Big` 1,048,576 + `Medium` 524,288) = 3,145,728 B, plus 16,472 B of
allocator overhead.

**What this is not.** No control image was run. The pre-conversion cost is
arithmetic from the residue this workspace already measured — 2 creations per
accept × 30 accepts × 176–179 B of TLS residue ≈ 10.6 kB unrecoverable, plus
60 stack allocate/free cycles — not a second on-target reading taken here.
What *is* measured is that the converted driver's cost for those 30 accepts is
**zero new threads and zero new sets**.

## 3. Capacity and admission — where the refusal actually happens

Connections opened one at a time toward the wall, each completing `VERSION`,
all held.

* **142 established.** The 143rd (driver index 142) failed.
* Failure mode: **the guest accepted nothing and sent nothing** — the host's
  `recv` returned end-of-stream with zero bytes, no `VERSION` reply
  (`HOLD closed-after-accept at 142`).
* At the wall, held for 140 s across two marks, both identical:

```
FDPROBE   seq=38 FD_CNT=150 FD_MAX=150 CA_CONN_CNT=142
POOLPROBE seq=38 BUSY=142 SETS=142 CAP=142 WORKERS=284 REFUSED=0 CONNS=142
```

`FD_CNT = FD_MAX = 150`: **zero descriptors free**, 142 of them CA clients on
top of the 8 the IOC holds at idle. This is the `doc/rtems-fd-ceiling-deviation.md`
ceiling reproduced exactly — 150 − 8 = 142 — on a driver that has since been
converted, so the conversion did not move the wall.

**It is the fd wall, not the pool's refusal.** `REFUSED` is **0** at the wall
and 0 for the whole run: `refused_clients()` counts the pool's `acquire()`
failure, and that failure never happened. `accept` failed with `ENFILE` first,
before any client object existed — which is what `38ea164d` claims ("at the
wall the fd layer still refuses first, so the refusal a client actually meets
is where C has it"), now measured.

**The pool's EAGAIN arm was therefore not exercised.** `SETS` reached exactly
`CAP` = 142, so set #143 *would* have been refused with `WouldBlock` — but the
descriptor for client #143 could not be obtained, so that arm is unreachable in
this image rather than merely untaken. The capacity choice is vindicated as
*correct* (142 is exactly where the target stops) while the argument that it
keeps the EAGAIN arm from being dead code does not survive the measurement:
both walls coincide at 142, and the fd one is always first.

**Nothing was logged.** The serial console carries no accept error, no refusal
line — the only non-probe output in the whole run is 13 `TCP connect failed
… Connection refused` lines from the *client* half dialling its blackholed
name server. A console-less operator sees this wall only in `FD_FREE`.

### 3.1 The memory term, re-measured

`38ea164d` derives capacity as `min(142 fd, 151 memory)` using 241,199,000 B
free at idle. This image measures **231,289,888 B** free at idle, and the
cost per client set at the wall is
(254,509,936 − 28,466,624) / 142 = **1,591,854 B**, of which 1,572,864 B is the
`Big` + `Medium` stack pair. So the memory term on this image is
231,289,888 / 1,591,854 ≈ **145 sets, not 151**.

`min(142, 145) = 142` — the conclusion is unchanged, the margin is not. At the
wall the guest had **5,246,576 B free heap**, about 3.3 sets' worth.

## 4. The pool never shrinks — the deviation's magnitude on this target

Recorded in §9 of the design as a deviation; its size here is new.

| | `FD_CNT` | `CONNS` | `BUSY` | `SETS` | `WORKERS` | heap `FREE` | heap `USED` |
|---|---:|---:|---:|---:|---:|---:|---:|
| idle, before any client | 8 | 0 | 0 | 0 | 0 | 231,289,888 | 28,466,624 |
| at the wall | 150 | 142 | 142 | 142 | 284 | 5,246,576 | 254,509,936 |
| **all 142 released** | **8** | **0** | **0** | **142** | **284** | **6,632,808** | **253,123,704** |

Descriptors come back completely — 150 → 8, no leak. Heap does not: only
1,386,128 B is returned, and **224,657,080 B stays allocated for the life of
the process** with zero clients connected. Free heap ends at **2.87 % of its
idle value**.

That is the deviation working as designed — the high-water mark is retained,
and it is bounded, which is the property the conversion bought. But the
operational consequence is worth stating plainly: **one transient 142-client
peak leaves the IOC with 6.6 MB of heap for the rest of its run.** Anything
that later needs a large allocation — and a `Big` stack is 1 MiB — has almost
nothing to take it from. §5.

## 5. UNFIXED

1. **`posix 147` for the iocsh threads is unverified on target.** Neither
   RTEMS binary constructs an `IocApp`, so `iocsh-startup` and
   `iocsh-after-ioc-running` are not created; 0 occurrences in every
   `TASKDUMP`. `c04f8469`'s band is enforced on the host only. Either an RTEMS
   entry point has to go through `ioc_app`, or the claim should say
   "host-reachable roles" rather than read as an on-target fact.
2. **The pool's `EAGAIN` refusal arm is unreachable in this image.** Both walls
   are at 142 and `accept` fails first; `REFUSED` stayed 0 through a full ramp
   to the wall. Exercising it needs an fd cap above 142 (the `-D` route in
   `doc/rtems-fd-ceiling-deviation.md` §5) or a capacity below it — the latter
   being the connection-count limit C does not have.
3. **No high-water release.** 224,657,080 B retained after a 142-client peak,
   free heap down to 6,632,808 B, permanently. Bounded, but the bound is
   nearly the whole heap.
4. **The task census truncates at 192 entries.**
   `EPICS_RTEMS_DUMP_MAX_TASKS` in `csrc/rtems_stats.c` is 192, so the
   `count=192` rows at 142 clients are the *instrument's* ceiling, not a thread
   count — 79 `CAS-client` and 78 `CAS-event` rows were printed before the cap.
   `WORKERS=284` from `POOLPROBE` and the 284 distinct `PRIOPROBE` labels are
   the authoritative counts there. The `count=35`/`39` readings in §2 are well
   under the cap and are exact.
5. **Probe not merged.** `doc/rtems-ca-pool-probe.patch` is measurement-only;
   the shipped image publishes neither the pool's counters nor a task census.
   `CA_CONN_CNT` and `FD_FREE` remain the only published numbers, and neither
   sees `SETS`/`WORKERS`.

## 6. Reproduction

On the box:

```
cd ~/epics-rs && git fetch <bundle> HEAD:box-pool
git worktree add ~/pool-wt box-pool && cd ~/pool-wt
git apply doc/rtems-ca-pool-probe.patch
~/rtems-bringup/build-pool.sh pool/poolioc.exe 10.0.2.2:15076
cd ~/rtems-bringup/pool && ./run-pool.sh poolioc.exe 15064 pool
```

`run-pool.sh` walks the four phases above and records its own qemu pid,
killing only that one — the box is shared, per
[rtems-measurement-rig-shared-box-kill-safety.md](rtems-measurement-rig-shared-box-kill-safety.md).
Artefacts: `~/rtems-bringup/pool/{pool.log, pool-phases.txt, pool-drive.log}`.
