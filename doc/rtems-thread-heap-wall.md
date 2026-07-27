# The armv7-rtems client wall: two of them, and what a client costs

Measured 2026-07-27 on gv100, `xilinx_zynq_a9_qemu` under qemu, image
`caioc-e10` (`realtime-ca-ioc`, `client-core,bringup-probes`,
`release-embedded`). Nothing under `crates/` was touched; this is a
measurement of the image as built from
`caucus/58EWEJWV91/e10-residue-503b2859-1`.

Handed to **refusal-fidelity**, who owns the admission budget.

## What does not transfer from VxWorks

On VxWorks the binding resource was reserved address space, the wall moved
linearly with guest RAM, and no RTP query tracked it. None of that holds here,
and the first thing to establish is that the VxWorks *method* does not either:

* `-m 256M` and `-m 512M` produce a byte-identical heap. `RTEMS:MEM_MAX`
  (`Free.total + Used.total`) is **260,805,344 B** in both, and the client wall
  lands at the same count. `xilinx_zynq_a9_qemu` fixes its memory size at BSP
  build time and ignores `-m` upward.
* Below 256M there is no guest at all. qemu refuses before boot:
  `kernel '/home/coding-agent/vx-rig-e10/caioc-e10.exe' is too large to fit in
  RAM (kernel size 267370496, RAM size ...)` at 192M, 160M, 128M and 96M.

So guest RAM is not a knob in either direction, there is no RAM ladder to fit a
line through, and the wall has to be found by consuming the heap instead.

## Two walls, and the cap is the one you meet first

**Wall 1 — the pool cap.** With the heap untouched, adding one CA client at a
time stops at 133 held connections; attempt 134 is refused with

```
status=48 text='CAS: no resources for a new client (worker pool at capacity)'
```

133 ramp connections plus the 8 the probe holds open on the status PVs is
**141**, which is `CAS_CLIENT_POOL_CAPACITY` (`blocking.rs:253`). At that point
`MEM_FREE` is 9,403,296 B and `MEM_BLK` 9,329,424 B — the heap is not what
stopped it.

**Wall 2 — the heap.** Squeezing the heap first with server-side channel state
(128,000–131,600 channels over 8 connections) moves the wall below the cap, and
then it is a different failure:

| arm | channels | last client admitted | connections at the wall | refusal |
| --- | --- | --- | --- | --- |
| sq2 | 128,000 | 111 | 120 | EAGAIN |
| sq3 | 128,000 | 110 | 119 | EAGAIN |
| sq4 | 130,400 | 113 | 122 | EAGAIN |
| sq5 | 131,600 | 112 | 121 | EAGAIN |

Every one of those is under 141, so the cap is not involved. The guest console
names the cause:

```
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (No more processes (os error 11)) — refused 10.0.2.2:51184 (refusal #1)
WARN  epics_ca_rs::server::blocking: refused a CA client for want of resources peer=10.0.2.2:51184 error=No more processes (os error 11) nth=1
```

`os error 11` is `EAGAIN` from `pthread_create`. The client sees
`(48, 'CAS: no resources for a new client (No more processes (os error 11))')`
and then EOF. **The IOC survives**: `CA_REFUSED_CNT` goes to 1, the post-release
sample answers normally, and a fresh client connects. This is the same
underlying failure as the VxWorks 800–864M band — `pthread_create` EAGAIN — but
there it landed on a mandatory thread (`scanOnce`) and aborted the RTP, while
here it lands on the CA accept path and is caught. On RTEMS it is that catch,
not the pool cap, that keeps the IOC alive at the memory wall.

## What a client costs: the declared stack, charged in full, up front

`CAS-client` is `StackSizeClass::Big` and `CAS-event` is `Medium`; on a 32-bit
target `bytes()` is `f * 0x10000 * 4`, so a client declares
1,048,576 + 524,288 = **1,572,864 B**. Four independent measurements:

| method | value |
| --- | --- |
| least squares, `MEM_USED` vs held, unsqueezed ramp, held 5…130 (126 samples) | **1,592,031 B/client**, R² = 0.99901 |
| same fit, held 0…133 / 40…120 | 1,605,474 / 1,588,319 B, R² = 0.99828 / 0.99752 |
| endpoint delta, sq1 (90 clients) | 1,592,107 B |
| endpoint delta, sq2 (111) / sq3 (110) | 1,592,283 / 1,592,296 B |
| nine consecutive single-client `MEM_BLK` steps, sq3 | 1,589,744 … 1,590,184 B (spread 440 B) |

Measured / declared = **1.0124** — an overhead of 19,432 B per client on top of
the declared stack. RTEMS is a single address space with one protected malloc
heap and no lazy paging, so a thread's declared stack is charged to the heap the
moment it is created. That is why the wall is linear in the client count with a
slope that is the declared stack: on this target the declaration *is* the cost.

The nine per-client steps agreeing to 440 B is the strongest form of the
linearity claim available here — it is not a fit, it is the same number nine
times in a row.

**Not tested: linearity in the declared stack *size*.** That needs
`StackSizeClass` changed in `crates/epics-ca-rs/src/server/blocking.rs`, and
there is no environment override (`rg` for `EPICS_RS_*STACK` across `crates/`
returns nothing). That file is not this panel's to touch this round, so the
claim "the wall is linear in declared stack" is established for *count* and
unestablished for *size*.

## There is a third ceiling, and it is not in the heap

The first attempt to reach the heap wall flooded 100 clients with unread monitor
subscriptions. It ended on the guest console with

```
[zone: mbuf_cluster] kern.ipc.nmbclusters limit reached
```

three times, with every reader on the box starved from the first wave onward,
and a fresh connection afterwards accepted at TCP but never answering
`CREATE_CHANNEL`. That is a libbsd pool, not `RTEMS_Malloc_Heap`, so
`malloc_free_space()` cannot see it at all.

Caveat on that arm, stated because it changes what it proves: the script that
drove it sent `EVENT_ADD` headers *without* their 16-byte payload (`psize=16`
promising a payload that never followed), so the server was correctly answering
with `CA_PROTO_ERROR` frames into sockets nobody read. The mbuf exhaustion
therefore cannot be attributed to monitor backlog specifically. What is
established is only that an mbuf-cluster ceiling exists on this target and is
invisible to the heap query. Finding where it sits is not measured here.

## Every number above is quiesced, and here is why

`RTEMS:MEM_FREE` and friends are **pushed**, not computed on read:
`status_pv.rs` has `PUSH_INTERVAL = 1 s` and the pusher runs at
`ThreadPriority::Low` (EPICS 10), eleven levels below `CAS-client` (20). Under
load it does not run. Measured directly: during a 4-client subscription flood
the same reading `MEM_FREE=229178856` was returned at t=2.6 s and again at
t=497.7 s — **495 seconds stale**, while the heap had in fact moved.

So the harness never samples under load. It stops the load, waits, and accepts a
reading only once `RTEMS:UPTIME` has advanced with wall clock (lag ≤ 4 s); every
sample line in the evidence carries its measured `lag=`. This is the same
priority band refusal-fidelity is fixing, met as a measurement obstacle.

## Inputs for the budget constant

`default_reservation_budget` / `EPICS_RS_POOL_RESERVATION_MB` **do not exist
anywhere in this worktree** — `rg` across the whole tree returns nothing. The
image measured here therefore has exactly two admission gates: the 141-thread
pool cap, and the `EAGAIN` catch on the accept path. Whatever budget is chosen
lands on top of these numbers, not alongside an existing one:

* heap total 260,805,344 B; free at idle 232,341,4xx B (four boots, spread 96 B)
* per client 1,592,296 B measured, 1,572,864 B declared
* the heap alone admits about (232,341,520 − 3,000,000) / 1,592,296 ≈ **144**
  clients from idle; the pool cap is **141**. They are three clients apart, so
  on this guest either can bind depending on the database and the client mix.
* a 160 MiB budget is **167,772,160 / 1,572,864 = 106 clients** — that is 72 %
  of free-at-idle and it would bind *first*, well before both the cap (141) and
  the heap (~144). On this 256 MB guest such a budget is not inert; it is the
  tightest of the three gates and costs about a quarter of the capacity the
  heap actually has.

## Evidence

`doc/vxworks-e10-rig/evidence-e10-heap/` — `rtemsramp-r256.log.gz` (the
unsqueezed 133-client ramp), `sq2..sq5.log.gz` (the four squeezed arms),
`sq3.console.log.gz` (the guest console carrying the EAGAIN refusal),
`rtemsheap-100.log.gz` (the mbuf arm). Harness:
`doc/vxworks-e10-rig/rtemsramp-e10.py`, `rtemsladder-e10.sh`,
`rtemssqueeze-e10.py`.
