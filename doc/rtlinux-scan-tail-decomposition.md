# Per-leg decomposition of the scan-jitter tail

`doc/rtlinux-rt-measurement.md` §2 measures one number — the deviation of CA
monitor inter-arrival from the nominal 100 ms scan period — and reports a
`SCHED_FIFO(60)` under-load p99.9 of **8324.2 µs**. Its own *Not measured*
paragraph states the limit: *"the tail was not decomposed per leg (no per-stage
timestamps between record process, CA server serialisation and client
delivery), so the split between the RT-throttle and the CA-pool legs is
unquantified."* Its correction paragraph then attributes the tail to the two
CA-pool legs by an argument from where the threads sit, not by measurement.

This document supplies the per-stage timestamps and closes that item. It does
not re-derive §2's other sections; it decomposes §2's metric, and it corrects
§2's headline number.

## Fidelity limit

Same box, same limit as `rtlinux-rt-measurement.md` §"Fidelity limit": this is
a **QEMU/KVM i440FX guest**, so every absolute microsecond carries a VM tax and
only the comparative arms are defensible. That constraint bites harder here
than in §2, and §6 below turns it into the central verdict: several of the
quantities being decomposed are *smaller than the guest's own kernel
scheduling-latency floor*, and a difference cannot be read through a floor that
large.

## Provenance

| | |
|---|---|
| Host | `ssh coding-agent@192.168.2.129` |
| OS / kernel | Ubuntu 26.04, `7.0.0-28-realtime` (PREEMPT_RT), `/sys/kernel/realtime = 1` |
| Virt | QEMU/KVM i440FX guest, 12 vCPU |
| Toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)`, rootless rustup, `--release` |
| Commit | `aa2e76ba` (`feat(rt-probe): decompose scan jitter into scan leg and CA hop`) |
| Harness | `examples/rt-probe` subcommands `scan-decomp` (single process) and `serve` + `watch` (split) |
| Load | 12 × `sha256sum /dev/zero`, `SCHED_OTHER`, nice 0 — the same generator as `rtlinux-rt-measurement.md` §1 |
| Elevation | `sudo chrt -f 60` on the probe process; `EPICS_RS_ALLOW_RT_PRIORITY` unset, so `RtPolicy::Disabled` makes `enter_ioc_thread` a no-op and the inherited FIFO(60) reaches every thread flat |
| RT throttle | `sched_rt_runtime_us = 950000` / period `1000000` except in §5, which sets `-1` and restores |

---

## §1 Method — two clocks, one identity

The measured chain is

```
scan-0.1 thread ─▶ record process ─▶ monitor post ─▶ CA server encode+write
                        │                                      │
                    stamps TIME                            socket ─▶ CA client decode ─▶ recv()
                                                                                            │
                                                                                      stamps arrival
```

Two points on that chain are observable without instrumenting the libraries:

* **`t_rec`** — the record's own `TIME`. `TSE = 0` routes
  `recGblGetTimeStamp` to `general_time::get_current()`
  (`crates/epics-base-rs/src/server/recgbl.rs:423`), read *inside* record
  processing on the `scan-0.1` thread, and carried to the client in the
  `DBR_TIME_*` payload the default subscription requests.
* **`t_arr`** — `SystemTime::now()` on the client the instant
  `MonitorHandle::recv` returns the snapshot.

Both are `CLOCK_REALTIME` on one host, so their difference is a real transit
and not a clock-domain artefact. With `T = 100 ms`:

| series | definition | what it is |
|---|---|---|
| **A** | `dA[i] = (t_rec[i] − t_rec[i−1]) − T` | **scan leg** — everything up to and including the timestamp read |
| **B** | `dB[i] = (t_arr[i] − t_arr[i−1]) − T` | **full chain** — exactly §2's metric |
| **C** | `C[i] = t_arr[i] − t_rec[i]` | **CA hop transit** — server encode + socket + client decode + channel |

These are not three independent estimates. Because `t_arr = t_rec + C`,

```
dB[i] = dA[i] + (C[i] − C[i−1])   =   dA[i] + dC[i]
```

holds **by construction**. Every microsecond of chain deviation is therefore
either scan-side or a change in hop transit, and the attribution is arithmetic
rather than inference. The harness prints the worst residual of that identity
on every run as a guard — if the two stamps were ever not the same clock, the
residual would be non-zero and every number below void. It is `0.000000 µs` on
every arm reported here.

Per-sample blame: a chain sample is blamed on `scan` when `|dA| ≥ |dC|` and on
`hop` otherwise. The "worst-1 % blame split" counts that verdict over the 1 %
of samples with the largest `|dB|`.

**A note on `p99.9`, which §2's headline depends on.** `report()` computes
`us[ceil(n·p)−1]`. At §2's **n = 200** that makes `p99.9` index 199 — *the
maximum*. §2's "p99.9 = 8324.2 µs" is therefore one sample, not a tail
percentile. Every arm below uses n ≥ 1199, and the long arm n = 5199, where
`p99.9` is the 6th largest sample and means what it says.

---

## §2 Single-process arms — the same topology §2 measured

`scan-decomp`, n = 1799 per arm (180 s), all values µs.

| arm | A scan p99 | A max | B chain p99 | B max | C hop p50 | C hop p99 | worst-1 % blame |
|---|---:|---:|---:|---:|---:|---:|---|
| `SCHED_OTHER`, no load | 165.8 | 6754.0 | 379.2 | 6757.9 | 277.3 | 514.5 | scan 5 / hop 13 |
| `SCHED_FIFO(60)`, no load | 101.7 | **133.6** | 287.0 | 5657.1 | 252.7 | 463.0 | scan 0 / **hop 18** |
| `SCHED_OTHER`, 12× | 2634.6 | 3503.7 | 2670.7 | 3795.1 | 543.8 | 1045.0 | **scan 18** / hop 0 |
| `SCHED_FIFO(60)`, 12× | **31.1** | **86.9** | **130.8** | **199.7** | 167.4 | 276.1 | scan 0 / **hop 18** |

Long arm, `SCHED_FIFO(60)` + 12×, **n = 5199** (520 s):

```
A scan leg |dev|:     n=5199 min=0.1 mean=9.2  p50=5.4   p90=22.1  p99=47.1  p99.9=80.5  max=454.8
B full chain |dev|:   n=5199 min=0.1 mean=37.1 p50=28.8  p90=77.3  p99=144.8 p99.9=406.6 max=1155.9
C CA hop transit:     n=5199 min=100.4 mean=198.4 p50=187.4 p90=269.9 p99=354.3 p99.9=398.6 max=1307.2
worst-1% blame split (n=52): scan=4 hop=48
```

Its worst chain sample, with the decomposition beside it:

```
   i    dB(chain)   dA(scan)   dC(hop)   blame
2118      -1155.9       -0.6   -1155.4   hop
2117       1116.2       -3.0    1119.1   hop
2115       -416.4     -451.2      34.8   scan
```

**Finding 1 — the leg that dominates depends on the scheduling class, and the
two arms have opposite answers.**

* Under `SCHED_OTHER` + load the **scan leg dominates**: worst-1 % blame is
  18 scan / 0 hop, and the worst chain sample (3795.1 µs) is 3503.7 µs scan
  against 291.3 µs hop. The `scan-0.1` thread being a dedicated thread does not
  protect it — at `SCHED_OTHER` it is preempted by the 12 hogs like anything
  else.
* Under `SCHED_FIFO(60)` + load the scan leg is **solved** and the **CA hop is
  the entire residual**: A p99 = 47.1 µs over 5199 samples, worst-1 % blame is
  48 hop / 4 scan, and the worst chain sample is 99.96 % hop.

So §2's re-attribution is **confirmed in direction for the FIFO arm it was
made about** — the residual `SCHED_OTHER` exposure under FIFO really is the CA
hop, and it is now measured rather than argued.

**Finding 2 — §2's 8324.2 µs magnitude does not reproduce.** Over 5199 FIFO +
12× samples (520 s) the largest chain deviation is **1155.9 µs**, 7.2× smaller
than §2's reported p99.9, and p99.9 itself is **406.6 µs**, 20× smaller. §2's
figure is a single sample out of 200 (see the `p99.9`-is-the-max note in §1).
Rare multi-millisecond stalls of that size do exist — the two no-load arms each
caught exactly one (6757.9 µs and 5657.1 µs), roughly one per 180 s — but they
are isolated events at a rate of order 0.5 %, not a routine tail. §2 reports one
such event as a percentile and thereby overstates the steady-state tail by an
order of magnitude.

**Finding 3 — those rare stalls are hop-side under FIFO and scan-side under
`SCHED_OTHER`.** The FIFO no-load 5657.1 µs event decomposes to 5631.0 µs hop /
26.1 µs scan (99.5 % hop); the `SCHED_OTHER` no-load 6757.9 µs event decomposes
to 6656.1 µs scan / 101.8 µs hop (98.5 % scan). Their mechanism is not
identified here — see §7.

---

## §3 Split arms — apportioning the hop between server and client

In the single-process rig the IOC and the client share one tokio runtime, so no
scheduling knob can reach one without the other. `serve` + `watch` puts them in
two processes, each `chrt`-ed independently, with the 12 hogs left at
`SCHED_OTHER` outside both. n = 1199 per arm (120 s), all values µs.

| arm | IOC | client | A scan p99 | A max | B chain p99 | C hop p50 | C hop p99 | worst-1 % |
|---|---|---|---:|---:|---:|---:|---:|---|
| S1 | OTHER | OTHER | 2756.0 | 6204.7 | 2912.4 | 507.4 | 801.6 | scan 11 / hop 1 |
| S2 | **FIFO** | OTHER | **34.2** | **75.2** | 321.5 | **139.5** | 372.6 | scan 0 / hop 12 |
| S3 | OTHER | **FIFO** | 2625.1 | 2650.2 | 2652.3 | 405.6 | 657.1 | scan 12 / hop 0 |
| S4 | **FIFO** | **FIFO** | **32.3** | 963.1 | **105.5** | **159.0** | **253.9** | scan 2 / hop 10 |

**Finding 4 — the scan leg is a pure function of the IOC's scheduling class.**
A p99 is 2756.0 / 2625.1 µs whenever the IOC is `SCHED_OTHER` (S1, S3) and
34.2 / 32.3 µs whenever it is FIFO (S2, S4) — an **~80× reduction**, and the
client's class moves it by 5 % (2756.0 → 2625.1, S1 → S3), i.e. not at all
beyond noise. This is the clean single-variable result in the whole set.

**Finding 5 — the server side owns most of the hop, but the two shares are not
additive.** Taking S1's C p50 = 507.4 µs as the baseline:

| elevate | C p50 | reduction | share of baseline |
|---|---:|---:|---:|
| IOC only (S2) | 139.5 | 367.9 | **72.5 %** |
| client only (S3) | 405.6 | 101.8 | 20.1 % |
| both (S4) | 159.0 | 348.4 | 68.7 % |

The single-side reductions sum to 469.7 µs while elevating both yields 348.4 µs.
They overlap because they are the *same* contention — CA server tasks and CA
client tasks are tokio-pool work competing with the same 12 hogs — so
"server share" and "client share" are not disjoint budgets and must not be
quoted as though they were. What the table does support is the ordering: the
**server side is the larger of the two**, by roughly 3.6× on typical transit.

Note S4 (both FIFO) has C p50 = 159.0 µs against S2's 139.5 µs — elevating the
client *raised* typical transit slightly. At n = 1199 that 19.5 µs is inside
run-to-run spread and is not a result; it is quoted only so the table is not
read as monotone.

**Tail columns in this table are single events and are excluded from every
claim above.** At n = 1199, `p99.9` is the 2nd-largest sample. S2's C p99.9 of
1919.5 µs against S1's 891.7 µs is one stall, not evidence that elevating the
IOC worsens the hop tail. Only p50/p90/p99 are load-bearing here.

---

## §4 What this says about `SCHED_FIFO` under load overall

Combining §2's arms at matched n = 1799:

| metric | OTHER + 12× | FIFO(60) + 12× | factor |
|---|---:|---:|---:|
| A scan leg p99 | 2634.6 | 31.1 | **84.7×** |
| A scan leg max | 3503.7 | 86.9 | 40.3× |
| B full chain p99 | 2670.7 | 130.8 | 20.4× |
| B full chain max | 3795.1 | 199.7 | 19.0× |
| C hop transit p50 | 543.8 | 167.4 | 3.2× |

Under a *correct* percentile at n = 1799, `SCHED_FIFO(60)` improves the loaded
chain on **every** statistic including the tail. §2's table shows FIFO improving
the body (loaded p50 27.6 vs 162.2 µs) while the tail got worse (p99.9 8324 vs
3061 µs) and calls that out as *"the FIFO tail under load is worse, not
better"*. That inversion is an artefact of `p99.9`-at-n=200 catching one rare
stall in the FIFO arm and not in the other. **There is no FIFO tail penalty.**

---

## §5 The RT throttle is not implicated — measured, not argued

§2 hypothesises *"the 95 % RT cap (`sched_rt_runtime_us`) periodically parks the
FIFO portion for 50 ms/period, landing on the tail samples."*

Negative evidence first: a throttle that parks FIFO threads 50 ms out of every
1 s would produce a ~50 ms deviation roughly once per second — about **500
events** across the 520 s long arm. **Zero** deviations above 1155.9 µs were
observed. The mechanism also does not fit: the throttle engages when RT
*runtime* exceeds 95 % of a period on a CPU, and this IOC's FIFO threads are
near-0 % duty (a 10 Hz scan plus epoll-blocked CA workers).

Positive test — `sched_rt_runtime_us = -1` (throttle disabled entirely), same
FIFO + 12× arm, n = 1199, restored to `950000` immediately after:

| | A scan max | B chain p99 | B chain max | C hop p50 | C hop p99 |
|---|---:|---:|---:|---:|---:|
| throttle on (`950000`) | 86.9 | 130.8 | 199.7 | 167.4 | 276.1 |
| throttle **off** (`-1`) | 40.1 | 120.3 | 173.7 | 183.6 | 274.7 |

**Finding 6 — disabling the RT throttle changes nothing.** Every statistic moves
by less than run-to-run spread, and C p50 moves the *wrong* way. The RT throttle
contributes **no measurable part** of this tail. §2's throttle hypothesis is
refuted.

---

## §6 Verdict — is a structural refactor of the CA legs warranted?

**No — and these numbers cannot justify one, which is a stronger statement than
"not yet".**

1. **The dominant leg under the default scheduling class is the scan leg, and
   it is already fixed by an existing knob.** `SCHED_OTHER` + load puts 18/18 of
   the worst 1 % on the scan leg (A p99 2634.6 µs); FIFO drops A p99 to 31.1 µs,
   84.7×. That lever is `EPICS_RS_ALLOW_RT_PRIORITY` plus the banded priorities
   already landed. It needs adoption, not a refactor.

2. **The residual hop tail sits at or below this guest's kernel latency
   floor.** `rtlinux-rt-measurement.md` §1 measures cyclictest max at ~0.5 ms
   quiet and ~0.5 ms loaded on this box — the best case *any* RT thread here can
   reach. The FIFO + load hop is C p99 = 354.3 µs and the whole chain is B p99 =
   144.8 µs (n = 5199). A refactor of the CA legs would be aiming at a quantity
   smaller than the noise floor it must be measured through. **On this box the
   improvement is unmeasurable in principle**, and any gain claimed for such a
   refactor from numbers taken here would be unfalsifiable.

3. **The mean is not where the risk is.** The one phenomenon that a
   throughput-shaped refactor of the CA hop would *not* address is the rare
   multi-millisecond stall of Finding 3 — one per ~180 s, 99.5 % hop-side under
   FIFO. That is a latency *event*, not a latency *level*; restructuring the
   encode/serialise path would move the 187 µs median and leave a 5.6 ms stall
   untouched.

**What is warranted instead**, in priority order:

1. **Adopt the RT path** (`EPICS_RS_ALLOW_RT_PRIORITY=YES` with the banded
   priorities) on any deployment that cares about scan jitter. 84.7× on the
   dominant leg, from a switch that already exists.
2. **Root-cause the rare hop stall** before touching hop structure. It is the
   only unexplained quantity left in the chain, it is the only one large enough
   to matter against a 0.5 ms floor, and its mechanism determines whether the
   fix is structural at all. See §7.
3. **Re-measure on bare metal** if a hop refactor is ever proposed. The case for
   one cannot be made or refuted on a KVM guest whose floor exceeds the
   quantity in dispute.

---

## §7 What this document does not establish

* **The rare multi-millisecond hop stall is unexplained.** Observed 4 times
  across ~1200 s of arms (6757.9 µs and 5657.1 µs no-load; 1155.9 µs and
  655.1 µs in the long FIFO arm). Its ~0.5 % rate makes it expensive to
  bisect, and nothing here distinguishes a tokio scheduling artefact, a loopback
  TCP interaction, and host-side KVM preemption. It is the open item.
* **The hop is not split below "server side / client side".** §3 apportions by
  scheduling class, not by stage: encode, `write`, socket, `read`, decode and
  the `mpsc` hop are one bucket. Splitting further needs timestamps inside both
  libraries, which this method deliberately avoids.
* **The server/client shares are not disjoint** (§3, Finding 5). Only their
  ordering is claimed.
* **10 Hz only, scalar only, loopback only.** One scan rate (`.1 second`), one
  scalar `calc` record, one subscriber, `127.0.0.1`. Array payloads,
  many-subscriber fan-out and a real NIC are unmeasured, and each could change
  which leg dominates.
* **No bare metal.** Same limit as `rtlinux-rt-measurement.md`; §6 item 3 turns
  it into a blocking condition for the refactor question.
* **`doc/rtlinux-rt-measurement.md` §2 is not edited here.** Its corrected text
  lives on `caucus/G2YPD0VTDV/scan-exec-6f54a3f2-1`; this branch carries the
  pre-correction copy. Findings 2, 4 and 6 above supersede §2's headline
  (`p99.9 = 8324.2 µs`), its *"the FIFO tail under load is worse, not better"*
  sentence, and its RT-throttle hypothesis respectively, and §2's *Not measured*
  bullet on per-leg decomposition is now answered. Reconciling the two documents
  is a merge-time edit, not a measurement.
