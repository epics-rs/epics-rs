# RT-Linux measurement — roadmap 4, the numbers behind `pi-lock-evaluation.md`

This is the measured companion to `doc/pi-lock-evaluation.md`. That document is
**static analysis only** — its §7 opens with *"Nothing was measured. Every claim
is static analysis"*, and its §6 execution order closes at step 7: *"Make it
reachable and measurable … Add a latency regression that fails when a
low-priority holder delays a high-priority waiter beyond a stated bound."* This
document is that step 7, run on a real PREEMPT_RT kernel. It does not re-derive
the lock taxonomy; it measures whether the one inversion the PI mutex was built
to kill (L1 / L7 / L33, the record gate) is actually killed on the kernel, and
what the RT scheduling path buys the IOC's monitor and scan latency.

## Fidelity limit — read this before any absolute number

**The box is itself a QEMU/KVM i440FX guest** (`systemd-detect-virt` → `kvm`,
DMI *"Standard PC (i440FX + PIIX, 1996)"*, CPU *"QEMU Virtual CPU version
2.5+"*). Every absolute microsecond below is a **VM number**, not bare metal:
the ~500 µs cyclictest maxima are dominated by host-scheduler and virtio exit
jitter that a bare-metal PREEMPT_RT box would not show. **Only the comparative
arms are defensible** — RT-vs-generic scheduling on the *same* guest, and
PI-on-vs-PI-off on the *same* guest, because both sides carry the identical VM
tax and it cancels in the difference. No claim here is a bare-metal latency
bound; every headline is a ratio or a delta measured on one guest under one
kernel.

## Provenance

| | |
|---|---|
| Host | `ssh coding-agent@192.168.2.129` |
| OS | Ubuntu 26.04, glibc 2.43-2ubuntu2, x86_64 |
| Kernel | `7.0.0-28-realtime` (PREEMPT_RT mainline), `/sys/kernel/realtime = 1` |
| Virt | QEMU/KVM, i440FX + PIIX guest, 12 vCPU, 15 GB RAM |
| RT throttle | `sched_rt_runtime_us = 950000` / `sched_rt_period_us = 1000000` (kernel default; RT capped at 95 % — relevant to the PI-off tail, see §4) |
| Toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)`, rootless rustup |
| Harness | `examples/rt-probe` (this commit), release build; PI-off = default (`parking_lot` fallback), PI-on = `--features linux-rt` (pthread `PTHREAD_PRIO_INHERIT`) |
| Priority | obtained with `sudo chrt` / `sched_setscheduler`, **no** `/etc/security/limits.d` edit |

The harness prints `n / min / mean / p50 / p90 / p99 / p99.9 / max` in µs for
every arm. Sample counts are stated per line and are never aggregated across
arms.

---

## §1 cyclictest — kernel scheduling-latency floor

The kernel baseline the IOC sits on. `cyclictest -l 100000 -m -S -p 90 -i 200`,
12 threads SCHED_FIFO-90, one per vCPU. This is *not* an IOC number — it is the
best case any RT thread on this guest can hope for.

```
# no load, n=100000/thread × 12
# Min Latencies: 00007 00007 00008 00008 00007 00008 00007 00007 00010 00006 00009 00007
# Avg Latencies: 00017 00019 00020 00018 00020 00021 00019 00018 00019 00018 00020 00019
# Max Latencies: 00512 00520 00525 00540 00575 00530 00547 00543 00536 00535 00538 00522
```

```
# under 12× SCHED_OTHER load (sha256sum /dev/zero), n=100000/thread × 12
# Min Latencies: 00009 00009 00009 00009 00009 00009 00008 00009 00009 00009 00009 00009
# Avg Latencies: 00009 00010 00010 00010 00010 00009 00011 00009 00009 00010 00010 00009
# Max Latencies: 00531 00542 00481 00453 00502 00476 00493 00494 01347 00479 00471 00502
# Histogram Overflows (>400us bucket): 00003 00003 00002 00002 00002 00002 00003 00003 00003 00002 00002 00002
```

**Comparative finding:** SCHED_FIFO-90 latency is **flat across load** — Avg
~17-21 µs quiet vs ~9-11 µs loaded (loaded is *lower* because the CPUs stay warm
and busy), Max ~0.5 ms in both. The 12× SCHED_OTHER compute load does not touch
the FIFO thread — exactly the PREEMPT_RT guarantee: a runnable RT thread
preempts SCHED_OTHER immediately. One 1347 µs outlier under load (single sample
out of 1.2 M) is the VM tax, not a scheduling failure.

**Not measured:** bare-metal floor (this is a guest); sub-µs behaviour (timer
resolution here is coarse); any load pattern other than pure CPU (no I/O-bound
or IRQ-storm load was applied).

---

## §2 IOC scan jitter — periodic record processing

`RT:SCAN` is a `calc` record at `SCAN = ".1 second"`; the harness subscribes over
CA and measures `|actual period − 100 ms|`. n=200 per arm.

| scheduling | load | mean | p50 | p90 | p99 | p99.9 / max |
|---|---|---:|---:|---:|---:|---:|
| SCHED_OTHER | none | 118.8 | 101.8 | 240.7 | 346.4 | 399.8 |
| SCHED_FIFO(60) | none | 65.7 | 51.2 | 146.1 | 234.3 | 305.4 |
| SCHED_OTHER | 12× | 309.6 | 162.2 | 553.9 | 2295.8 | 3061.5 |
| SCHED_FIFO(60) | 12× | 165.6 | **27.6** | **75.2** | 4926.8 | 8324.2 |
(µs of deviation from the 100 ms period)

**The last column is one sample, not a percentile.** `report()` computes
`us[ceil(n·p)−1]`, so at **n=200** `p99.9` resolves to index 199 — the
maximum. The header says "p99.9 / max" because at this sample size they are
the same number. Every figure in that column is therefore a single worst
event, and the 8324.2 µs entry in particular must not be read as a
steady-state tail; see the second correction below.

**Comparative finding:** FIFO scheduling roughly halves typical jitter
(quiet mean 65.7 vs 118.8 µs; loaded p50 27.6 vs 162.2 µs — the body of the
distribution is 6× tighter under load). The tail column appears to move the
other way (8324 vs 3061 µs), but at n=200 that comparison is one FIFO sample
against one SCHED_OTHER sample and it does not survive a larger run: **at
matched n=1799 FIFO beats SCHED_OTHER on every statistic including the
tail** — see the second correction below.

**Correction (2026-07-24) — the tail is not scan-on-the-pool.** The first
version of this section attributed the tail to `pi-lock-evaluation.md` §6
step 2's *"periodic scan … is a JoinSet tokio task"* and called that refactor
unbuilt. It was already built on the tree these numbers were taken from:
`6852ea13` (one dedicated `scan-%g` thread per rate at `ScanLow + ind`) and
`a4781dc0` (`ScanOwner`) are both ancestors of this document's own commit
`b1d99e04`, and `examples/rt-probe` boots its IOC through `ScanOwner::start`
(`main.rs:157`). The doc still says otherwise because it and the refactor sit
on different branches: `pi-lock-evaluation.md` lives on `main`, where none of
those three commits are present.

So at the measured commit the tick body already ran on `scan-0.1`
(pinned since by `server::scan::tests::a_periodic_tick_processes_on_its_own_banded_scan_thread`),
and the 8324 µs p99.9 is **not** scan execution inheriting SCHED_OTHER. What
this metric measures is CA-monitor *inter-arrival* at an in-process client:
scan thread → record process → CA server → CA client. The scan leg is banded;
the two CA legs are tokio-pool tasks and are the remaining SCHED_OTHER
exposure. Re-attributing the tail to them is consistent with §3, where the CA
server path is exactly what FIFO scheduling tightens.

**Second correction (2026-07-24) — the tail was decomposed, and three of the
claims above did not survive it.** `doc/rtlinux-scan-tail-decomposition.md`
supplies the per-stage timestamps this section lacked. Every CA monitor update
already carries the record's own `DBR_TIME_*` stamp, written by
`recGblGetTimeStamp` inside record processing on `scan-0.1`; pairing it with
the client's arrival stamp off the same `CLOCK_REALTIME` gives the identity
`dB = dA + dC` by construction, so each microsecond of chain deviation is
scan-side or hop-side arithmetically. Measured identity residual: `0.000000 µs`
on every arm. What that says about this section:

* **The 8324.2 µs headline is a single rare stall, not a tail.** The same
  FIFO + 12× arm re-run at **n=5199** (520 s) gives **max 1155.9 µs** and
  **p99.9 406.6 µs** — 7.2× and 20× below the figure above. Multi-millisecond
  stalls of that size are real but isolated, at roughly one per 500 s of FIFO
  monitoring (~0.04 % of samples).
* **There is no FIFO tail penalty.** At matched **n=1799**, FIFO(60) + 12×
  against SCHED_OTHER + 12× is **chain p99 130.8 vs 2670.7 µs (20.4×)** and
  **chain max 199.7 vs 3795.1 µs (19.0×)**. FIFO wins on every statistic; the
  apparent inversion above is the n=200 artefact.
* **The RT-throttle hypothesis is refuted.** Setting
  `sched_rt_runtime_us = -1` (throttle off entirely) and re-running the same
  arm changes nothing: chain max **173.7 µs off vs 199.7 µs on**, and hop
  transit p50 moves the *wrong* way (183.6 off vs 167.4 on). A 95 %/1 s cap
  would park FIFO threads 50 ms once a second — ~500 events across the 520 s
  long arm; **zero** deviations above 1155.9 µs were seen. The mechanism never
  fitted either: these FIFO threads are near-0 % duty (10 Hz scan, epoll-blocked
  CA workers).
* **Which leg dominates flips with the scheduling class.** Under SCHED_OTHER +
  load the **scan leg** owns 18/18 of the worst 1 % (scan-leg p99 2634.6 µs) —
  a dedicated `scan-0.1` thread is preempted by the hogs like anything else.
  Under FIFO the scan leg collapses to p99 **31.1 µs** (84.7×) and the **CA hop
  is the entire residual** (48 of the worst 52 samples at n=5199). So the
  re-attribution above is right in direction for the FIFO arm, and wrong about
  the SCHED_OTHER arm it was not making a claim about.

**Not measured:** jitter observed at the record rather than through CA is now
covered (that is the decomposition's scan-leg series); jitter at scan rates
other than 10 Hz remains unmeasured. The decomposition splits the hop only into
"server side" vs "client side" by scheduling class — encode, `write`, socket,
`read`, decode and the `mpsc` hop stay one bucket — and it leaves the rare
multi-millisecond hop stall unexplained.

---

## §3 CA and PVA monitor latency

Round-trip from `put` to the matching monitor callback. CA n=300, PVA n=300.

**CA monitor latency (µs):**

| scheduling | load | mean | p50 | p90 | p99 | p99.9 / max |
|---|---|---:|---:|---:|---:|---:|
| SCHED_OTHER | none | 327.4 | 309.9 | 429.4 | 532.1 | 591.2 |
| SCHED_OTHER | 12× | 634.6 | 576.5 | 924.3 | 1325.3 | 3472.5 |
| SCHED_FIFO(60) | 12× | **215.8** | **208.6** | **240.0** | **323.1** | **744.7** |

**PVA monitor latency (µs):**

| scheduling | load | mean | p50 | p90 | p99 | p99.9 / max |
|---|---|---:|---:|---:|---:|---:|
| SCHED_OTHER | none | 1429.9 | 1408.3 | 1510.9 | 1625.6 | 1694.4 |
| SCHED_OTHER | 12× | 1310.5 | 1373.5 | 1407.5 | 3774.1 | 6337.2 |
| SCHED_FIFO(60) | 12× | **1264.0** | **1247.7** | **1270.0** | **2233.8** | **2277.6** |

**Comparative finding:** under load, FIFO scheduling both lowers and *tightens*
monitor latency. CA loaded p99 drops 1325 → 323 µs (4.1×) and max 3472 → 745 µs
(4.7×); PVA loaded p99.9 drops 6337 → 2278 µs (2.8×). The RT path's value is not
a lower median — it is a bounded tail: under SCHED_OTHER the p99.9 blows out
(the update thread waits behind the 12 compute hogs), under FIFO it does not.
PVA's absolute floor is ~1.3 ms vs CA's ~0.3 ms because the PVA monitor path
carries the FieldBuilder / structure-encode cost CA's raw DBR path does not —
that gap is protocol structure, present on both scheduling arms, and is not an
RT effect.

**Not measured:** array payloads (all arms are scalar `ao`/`calc`); many
concurrent subscribers (single monitor per arm); the network was loopback only.

---

## §4 The PI proof — the record gate under a real inversion

The headline. Three `std::thread`s, all pinned to **one** vCPU (`--cpu 6`, off
the boot CPU) and all SCHED_FIFO, reproduce the textbook priority inversion on
the actual record gate `PvDatabase::lock_record("RT:AO")` (the `dbScanLock`
analogue — `pi-lock-evaluation.md` L1):

* **holder**, FIFO **10**: takes `lock_record`, does a 10 ms wall-clock-bounded
  critical section, releases.
* **hog**, FIFO **30**: pure CPU, 20 ms spin / 5 ms sleep, never touches the
  lock — the *medium* priority that has no business delaying the high one.
* **waiter**, FIFO **50**: waits for the holder to own the gate, then times how
  long `lock_record` takes to acquire.

Without priority inheritance the FIFO-50 waiter is blocked by the FIFO-30 hog
(which preempts the FIFO-10 holder mid-critical-section) — the medium task
delays the high task through a lock it never touches. With inheritance the
holder is boosted to 50, preempts the hog, finishes its 10 ms section, and hands
off. Same binary layout, same pinning, same priorities; the only difference is
the `linux-rt` feature that swaps `parking_lot::Mutex` for a
`pthread_mutex_t` with `PTHREAD_PRIO_INHERIT`.

```
# PI OFF — parking_lot fallback (pi_active=false), n=150, --cpu 6
pi-proof low=10 med=30 high=50: n=150 min=23460.3 mean=24571.1 p50=24581.0 p90=24584.1 p99=24590.6 p99.9=24913.0 max=24913.0 (us)

# PI ON  — pthread PTHREAD_PRIO_INHERIT (pi_active=true), n=150, --cpu 6
pi-proof low=10 med=30 high=50: n=150 min=10011.8 mean=10015.5 p50=10014.6 p90=10018.6 p99=10030.9 p99.9=10061.1 max=10061.1 (us)
```

| | PI OFF | PI ON | delta |
|---|---:|---:|---:|
| n | 150 | 150 | |
| min | 23 460 | 10 012 | |
| mean | 24 571 | 10 016 | **−59 %** |
| p50 | 24 581 | 10 015 | |
| p99 | 24 591 | 10 031 | |
| max | **24 913** | **10 061** | **−14 852 µs (2.48×)** |
(µs to acquire the record gate)

**Comparative finding — the inversion is real, and PI removes it.** PI-on
acquisition collapses to **exactly the holder's 10 ms critical section**
(10 016 µs mean, 10 061 µs max — the tight distribution says the waiter blocks
for the CS and nothing else). PI-off inflates to **~24.6 ms** — the extra
~14.6 ms is the hog interference: at FIFO-10 the holder only runs in the hog's
5 ms sleep windows, so its 10 ms section stretches across ~2-3 hog cycles while
the FIFO-50 waiter sits behind it. **Worst-case latency for the high-priority
task drops from 24.9 ms to 10.1 ms — a 2.48× reduction — purely from
`PTHREAD_PRIO_INHERIT` on the record gate.** This is the measured proof behind
the PI flip that made L1/L7/L33 blocking PI locks: the lock the port most
clearly shares with C's `dbScanLock` now inherits priority the way C's
`epicsMutex` does (`pi-lock-evaluation.md` §5), and the number is 14.9 ms of
worst-case inversion removed.

The 24.6 ms figure is bounded (not the unbounded hang seen in an earlier
harness draft) because the hog yields 5 ms every 25 ms — a bounded-inversion
model. A hog that never yields would make the PI-off arm unbounded; that is the
qualitative difference PI defends against, and it is why the PI-off number is a
*floor* on the damage, not a worst case.

**Not measured:** inversion with more than one medium hog (a single FIFO-30
spinner); the RTEMS target (this is the Linux `pthread` PI path — RTEMS priority
is proven separately in `doc/rtems-priority-on-target-measurement.md`, and RTEMS
PI is a distinct mechanism); nested lock chains (single gate, single waiter).

---

## §5 `EPICS_RS_ALLOW_RT_PRIORITY` — both directions on the real kernel

The opt-in switch that gates the SCHED_FIFO path, exercised as root so the
`CAP_SYS_NICE` question is out of the way and only the switch logic is under
test. `enter_ioc_thread(CaServerLow)` then reads back the *kernel's* view via
`sched_getscheduler` / `sched_getparam`.

```
# switch = NO
rt-policy: EPICS_RS_ALLOW_RT_PRIORITY=Some("NO") -> RtPolicy::Disabled
rt-policy: enter_ioc_thread(CaServerLow) verdict=Disabled; kernel policy=SCHED_OTHER prio=0
# switch = YES
rt-policy: EPICS_RS_ALLOW_RT_PRIORITY=Some("YES") -> RtPolicy::AllowRealtime
rt-policy: enter_ioc_thread(CaServerLow) verdict=Realtime; kernel policy=SCHED_FIFO prio=20
# switch = unset (Linux default)
rt-policy: EPICS_RS_ALLOW_RT_PRIORITY=None -> RtPolicy::Disabled
rt-policy: enter_ioc_thread(CaServerLow) verdict=Disabled; kernel policy=SCHED_OTHER prio=0
```

**Finding:** the switch controls the real kernel scheduling class, verified by
the kernel itself, both directions: `YES` → `SCHED_FIFO` prio 20
(`ThreadPriority::CaServerLow`), `NO` and unset → `SCHED_OTHER` prio 0. On Linux
the default is Disabled (unset behaves as `NO`), the inverse of the RTEMS default
(`DEFAULT_POLICY = AllowRealtime` there). The `CaServerLow = 20` mapping matches
the opt-in SCHED_FIFO path landed in the blocking CA driver.

---

## §6 Correctness under PREEMPT_RT — the gate on the RT kernel

Stage 2 context for the measurements above: the full test gate on this kernel,
compared against the host.

| run | result |
|---|---|
| `cargo nextest run --workspace -E 'not package(epics-oracle-rs)'` | **10031 passed / 10031, 2 skipped** (exit 0) |
| `cargo nextest run --workspace` (incl. oracle) | 10182 run, 14 failed — **all 14 in `epics-oracle-rs`** |
| `-p epics-ca-rs --features rtems-exec-model` | 580 / 580 |
| `-p epics-pva-rs --features rtems-exec-model` | 1353 / 1353 |
| `-p epics-bridge-rs --features rtems-exec-model` | 685 / 685 |
| `-p epics-base-rs --features rtems-exec-model` | 3540 / 3541 (see flake below) |
| `cargo test --doc --workspace` | exit 0 |

* **The 14 `epics-oracle-rs` failures are env-gated, not RT regressions.**
  `epics-oracle-rs` boots against a C EPICS ground-truth tree at
  `/home/stevek/work/epics-base/bin/linux-x86_64`, which is absent on the box
  and present on the host — which is why the host passes all 10182 and the box
  fails exactly those 14. Excluding the package is the correct isolation, not a
  workaround for an RT defect.
* **The one feature-ON base failure is a load-induced ordering flake.**
  `runtime::background::future_exec::tests::a_yielding_task_releases_the_worker_to_a_queued_task`
  failed only inside the concurrent full-workspace run (load avg ~20), in
  0.014 s — an ordering flip, not a timeout. Per the 50× isolation protocol it
  was re-run alone on **both** machines: **RT-box 50/50 pass, host 50/50 pass**.
  It is a scheduler-ordering flake exposed by concurrent load, identical on
  generic and RT kernels; it is not an RT-specific regression.

---

## §7 What this whole document does not establish

* **No bare-metal number.** Everything is a KVM/i440FX guest measurement.
  Absolute microseconds carry the VM tax; only the comparative arms (RT-vs-OTHER,
  PI-on-vs-off) survive removing it.
* **No RTEMS PI measurement.** §4 is the Linux `pthread` PI path. RTEMS priority
  is measured in `doc/rtems-priority-on-target-measurement.md`; RTEMS PI is a
  separate mechanism and is not measured here.
* **Single-inversion, single-gate.** §4 uses one medium hog, one waiter, one
  gate. Nested chains, multiple waiters, and multiple mediums are unmeasured —
  each can only make the PI-off arm worse, so the 2.48× is a lower bound on PI's
  value, not an upper.
* **Loopback only, scalar only.** No array payloads, no real network, no
  many-subscriber fan-out in §2/§3.
* **The scan-tail item in §2 is closed; one narrower item replaces it.** The
  per-leg decomposition is `doc/rtlinux-scan-tail-decomposition.md`. It
  apportions the chain arithmetically (`dB = dA + dC`, residual `0.000000 µs`),
  retires the FIFO-under-load p99.9 blowup as an n=200 percentile artefact
  (n=5199: max 1155.9 µs, p99.9 406.6 µs), refutes the RT throttle by
  disabling it, and finds the dominant leg to be the **scan leg under
  SCHED_OTHER** and the **CA hop under FIFO**. What stays open is the **rare
  multi-millisecond hop stall**: 2 events across ~1000 s of FIFO arms (5657.1
  µs and 1155.9 µs, 99.5 % and 99.96 % hop-side), ~1 per 500 s. Its mechanism
  is unidentified — nothing there distinguishes a tokio scheduling artefact,
  a loopback TCP interaction, or host-side KVM preemption — and its rate makes
  it expensive to bisect.
* **No CA-leg refactor is justified from this box.** The decomposition's §6
  verdict: under FIFO the residual hop is chain p99 144.8 µs / hop-transit p99
  354.3 µs, at or below the ~0.5 ms cyclictest floor §1 measures on this
  guest. A refactor aimed at it would be targeting a quantity smaller than the
  floor it must be measured through, so any claimed gain would be
  unfalsifiable here. The lever that does pay already exists —
  `EPICS_RS_ALLOW_RT_PRIORITY` plus the banded priorities, 84.7× on the
  dominant leg — and needs adoption, not restructuring.

This closes `pi-lock-evaluation.md` §7's *"Nothing was measured"* for the Linux
PI path and delivers its §6 step 7 measurable-regression: the record gate under
a real FIFO inversion costs a high-priority task **24.9 ms worst-case with PI
off and 10.1 ms with PI on**, on kernel `7.0.0-28-realtime`.
