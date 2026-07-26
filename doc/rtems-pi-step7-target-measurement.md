# Priority-inheritance locks on RTEMS — the on-target measurement (§5 step 7)

Step 7 of [`rtems-priority-locks-design.md`](rtems-priority-locks-design.md) owes
four readings from the target, not from a host: that the process obtains a
priority-inheritance lock protocol, that inheritance changes what a blocked
high-band thread waits for, that the band layout still holds after the L1 flip,
and that concurrent CA writes through the flipped gate neither deadlock nor
starve.

All four were taken on 2026-07-22 on the bring-up box, on
`xilinx_zynq_a9_qemu` under qemu-system-arm, from the flip tip plus the two
commits below. Provenance of the box, toolchain and patched `libc` is the table
in [`rtems-priority-on-target-measurement.md`](rtems-priority-on-target-measurement.md)
§1; nothing about it changed for this run.

Two shorthands used throughout:

* **posix** — the POSIX priority the port assigns, `posix = 56 + epics`.
* **core** — the RTEMS scheduler priority, `core = 255 - posix`. It counts
  **downwards**: a *lower* core number is *more urgent*. A priority-inheritance
  boost therefore shows up as core going **down**.

**Naming note (2026-07-25).** The target IOC binaries were later renamed —
`rtems-ca-ioc` → `realtime-ca-ioc`, `rtems-pva-ioc` → `realtime-pva-ioc`.
Every old name below is left exactly as captured, because this file is a
record of the tree as it stood, not a description of it as it stands.

## 0. The defect the measurement found first

The flip was not bootable on target. `PiMutex::new` initialised the
`pthread_mutex_t` in a stack local and then moved it into the returned value;
RTEMS 6 stores `flags = ((uintptr_t)mutex ^ POSIX_MUTEX_MAGIC) | protocol` at
`pthread_mutex_init` and revalidates it from the mutex's *current* address on
every operation (`rtems/posix/muteximpl.h:445-459`, magic at `:64`), so the
first `lock()` after the move returned `EINVAL` and the IOC died in the panic
hook.

No host arm could see it: glibc mutexes survive relocation, and the default host
build does not use this type at all. Fixed by boxing the `pthread_mutex_t`
before initialising it, so the object never moves with its owner (commit
`ff2fbab0`), with a regression test compiled on the PI arms only.

## 1. M1 — the process obtains PI (raw)

`report_lock_protocol()` is `is_pi_mutex_active()`'s first caller (§4.4 note 2),
printed from the boot path of both target binaries right after the panic hook:

```
rtems-boot: main() reached
epics-rs: lock protocol: PI is enabled, RT scheduling AllowRealtime
rtems-ca-ioc: serving 3 records on CA port 5064 (TCP + UDP search), RTEMS execution model, no tokio runtime
rtems-ca-ioc: RTEMS:AO
rtems-ca-ioc: RTEMS:LO
rtems-ca-ioc: RTEMS:MSG
```

`pthread_mutexattr_setprotocol(PTHREAD_PRIO_INHERIT)` therefore **succeeds** on
RTEMS 6 — the C-parity fall-back arm (`osdMutex.c` `globalAttrInit`) did not
fire. The three record lines after it are the boot proceeding *past* the first
`lock_record()`, which is what the pre-`ff2fbab0` image could not do.

With `EPICS_RS_ALLOW_RT_PRIORITY=NO` the same line reads:

```
epics-rs: lock protocol: PI is enabled, RT scheduling Disabled
```

— the lock protocol is a property of the mutex, independent of whether the port
is allowed to set thread priorities.

## 2. M2 — the inversion regression

Three banded threads over one gate: `pi-low` (`CaServerLow`, core 179) takes the
gate and works on the CPU for 3000 ms; `pi-med` (`ScanLow`, core 139) burns the
CPU for 6000 ms from t=300 holding nothing; `pi-high` (`ScanHigh`, core 129)
blocks on the same gate at t=600. Run twice per boot: once over the real L1
record gate (`PvDatabase::lock_record`), once over a `PTHREAD_PRIO_NONE` mutex
built in the same image — the two runs differ in the protocol and in nothing
else.

### RT scheduling on (the target default)

```
PIPROBE low  run=pi_gate t=8    acquired core=179
PIPROBE med  run=pi_gate t=306  spinning
PIPROBE high run=pi_gate t=606  asking for the gate
PIPROBE low  run=pi_gate t=3014 releasing hold_wall_ms=3004
PIPROBE high run=pi_gate t=3017 wait_ms=2410 prio=126
PIPROBE med  run=pi_gate t=6309 spin_actual_ms=6001
PIPROBE result run=pi_gate ... low_hold_wall_ms=3004 low_core_start=179 low_core_best=129 low_core_end=129

PIPROBE low  run=control_prio_none t=7    acquired core=179
PIPROBE med  run=control_prio_none t=307  spinning
PIPROBE high run=control_prio_none t=607  asking for the gate
PIPROBE med  run=control_prio_none t=6308 spin_actual_ms=6000
PIPROBE low  run=control_prio_none t=6316 releasing hold_wall_ms=6307
PIPROBE high run=control_prio_none t=6319 wait_ms=5712 prio=126
PIPROBE result run=control_prio_none ... low_hold_wall_ms=6307 low_core_start=179 low_core_best=179 low_core_end=179
```

| | L1 gate (PRIO_INHERIT) | control (PRIO_NONE) |
|---|---|---|
| `pi-high` wait | **2410 ms** | **5712 ms** |
| `pi-low` wall time for 3000 ms of work | 3004 ms | 6307 ms |
| `pi-low` core priority while holding | 179 → **129** | 179 (unchanged) |

Under inheritance the wait is `HOLD - HIGH_AT` = 2400 ms — bounded by what the
gate's owner does and by nothing else. Without it the middle band lands in the
wait and `pi-high` pays `SPIN` on top, for 2.4× the latency; `pi-low`'s 3000 ms
of work takes 6307 ms of wall time because `pi-med` preempts it. The mechanism
is directly observed rather than inferred: the holder's *scheduler* priority
moves to exactly the blocked waiter's band and back on release.

A second, timing-independent form of the same reading (`pi-med` dropped, the
holder yields the CPU in 20 ms sleeps, so only the boost is under test):

```
PIPROBE boost run=pi_gate           t=600 blocking core=129 posix=126
PIPROBE boost run=pi_gate           base_core=179 best_core=129 end_core=129 boosted_at_ms=623 release_t=3021
PIPROBE boost run=pi_gate           after_release_core=179
PIPROBE boost run=control_prio_none base_core=179 best_core=179 end_core=179 boosted_at_ms=-1
PIPROBE boost run=control_prio_none after_release_core=179
```

Boost applied 23 ms after the waiter blocked, released with the mutex, absent on
the control.

### RT scheduling off (`EPICS_RS_ALLOW_RT_PRIORITY=NO`)

```
PRIOPROBE label=cbLow    kname=cbLow    epics=59 applied=Disabled getsched_rc=0 policy=1 posix=1 core=254
PRIOPROBE label=CAS-TCP  kname=CAS-TCP  epics=18 applied=Disabled getsched_rc=0 policy=1 posix=1 core=254
PIPROBE boost run=pi_gate base_core=254 best_core=254 end_core=254 boosted_at_ms=-1
PIPROBE sanity t=2238 requested_sleep_ms=500 actual_sleep_ms=2214 mode=0x0400 core=254 posix=1
```

Every thread lands at core 254 — RTEMS pthreads inherit `POSIX_Init`'s near-idle
priority when the port does not set one. With one priority for the whole IOC
there is nothing to inherit (`best_core` never moves), and there is also no
inversion to measure: threads at equal priority under SCHED_FIFO run to
completion, which the sanity reading shows directly (a 500 ms sleep in a higher
band returns after 2214 ms, i.e. when the CPU-bound thread finished). The
`wait_ms=2` printed by the three-thread scenario in this image is **not** a
latency result — the scenario's threads never overlap, so the number measures
nothing. This half documents that turning RT priorities off removes the whole
band structure the PI flip exists to protect; it is not a second latency
data point.

### Limits of these numbers

* QEMU virtual time. The probe's own clock resolution is printed per boot
  (`min_nonzero_step_ns=1410`…`2330` across runs), so millisecond figures are
  sound and microsecond ones would not be. Every quantity above is ≥ 2400 ms
  against a 10 ms tick.
* Single core, no timeslice (`mode=0x0400`: preempt enabled, no `RTEMS_NO_PREEMPT`,
  no timeslice). Preemption is by priority only, which is what makes the
  contrast this clean and also what makes it *sharper* than a multi-core board
  would give.
* The absolute wall figures include a ~4 ms print/handoff overhead visible in
  the `t=` stamps; they are not subtracted anywhere above.
* **Instrument caveats found the hard way, both recorded in the probe source.**
  `pthread_getschedparam` returns the thread's *base* priority on RTEMS, so it
  cannot see an inherited boost at all — an earlier run using it reported "no
  boost" for the PI gate and the control alike. And a spin loop that calls a
  kernel object service every iteration spends nearly all its time with thread
  dispatch disabled, which makes the thread effectively non-preemptible and
  silently degenerates the scenario. Both readings are artifacts of the
  instrument, not of the scheduler; the numbers above are from the versions that
  avoid them.

## 3. M3 — the band census on the flip image

Every IOC thread, read back from the kernel by the thread itself at
`enter_ioc_thread` (`PRIOPROBE`) and cross-checked from the C side against the
scheduler's own object table (`TASKDUMP`). `posix = 56 + epics` and
`core = 255 - posix` hold on every one:

| thread | epics | posix | core |
|---|---|---|---|
| `main` | (not banded by design) | 1 | 254 |
| `status-pv` | 10 | 66 | 189 |
| `CAS-UDP` | 16 | 72 | 183 |
| `CAS-TCP` | 18 | 74 | 181 |
| `CAS-event-blocking <peer>` | 19 | 75 | 180 |
| `CAS-client-blocking <peer>` | 20 | 76 | 179 |
| `cbLow` | 59 | 115 | 140 |
| `cbMedium` | 64 | 120 | 135 |
| `scanOnce` | 67 | 123 | 132 |
| `cbTimer` | 70 | 126 | 129 |
| `cbHigh` | 71 | 127 | 128 |

All applied `Realtime` with `getsched_rc=0` and `policy=1` (`SCHED_FIFO`). The
per-connection pair is from threads created under the M4 load, so it covers
threads that did not exist at boot.

Two notes against the 2026-07-22 census:

* `scanOnce` now reads `epics=67 posix=123 core=132`, where the earlier census
  recorded `epics=60 posix=116 core=139`. The band changed in the tree between
  the two runs; the map did not.
* No `scan-N` periodic threads exist in this image — `DEMO_DB`'s three records
  are all `Passive`.

Unchanged and still true: libbsd's `_BSD` threads sit at core 100 and `IRQS` at
96, i.e. **above** every EPICS band (`CAS-TCP` at 181 is 81 levels below `IRQS`),
and `rtems_object_get_name` remains blind to pthread names — the listing above
uses the `_Thread_Get_name` workaround.

## 4. M4 — concurrent CA writes through the flipped gate

`caput-rs` loops from the box host side, reaching the guest over the SLIRP
hostfwd (TCP 5064, UDP 15076 → guest 5064). Liveness is proved by protocol
traffic, not by a bare connect: a search, a TCP circuit and a value come back.

```
$ caget-rs RTEMS:AO RTEMS:LO RTEMS:MSG
RTEMS:AO                       1.5
RTEMS:LO                       7
RTEMS:MSG                      rtems-ca-ioc

== A: 25 puts x 4 workers, ALL on RTEMS:AO
A wall=2.22s puts=100 rate=44.9/s
== B: 25 puts x 3 workers, DISJOINT records
B wall=1.61s puts=75  rate=46.5/s
== C: 25 puts x 8 workers, ALL on RTEMS:AO
C wall=4.36s puts=200 rate=45.8/s
== liveness after the load
RTEMS:AO                       525
RTEMS:LO                       625
RTEMS:MSG                      m25
```

375 writes across the three phases, zero failures, zero refusals, and the IOC
answers reads afterwards with the last value written. Same-record contention
(A, C) and disjoint-record traffic (B) run at the same rate, so the single L1
gate is not serialising anything visible at this scale — the rate is set by the
client side, which spawns a process and opens a fresh CA circuit per put, not by
the IOC.

## 5. What is not measured here

The C-side wait-order comparison (§6) is owed by the C-side panel and is not in
this report.
