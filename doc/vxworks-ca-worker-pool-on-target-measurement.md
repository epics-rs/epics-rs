# CA connection worker pool on VxWorks 7: banding and bound under load

Row E8. Measured 2026-07-26 on `x86_64-wrs-vxworks` under QEMU on the shared
box, with `realtime-ca-ioc.vxe` running as an RTP. Two guests, identical image,
differing only in `-m`: `1024M` (`OS Memory Size: ~958MB`) and `2048M`
(`OS Memory Size: ~1982MB`).

The question was whether `CAS_CLIENT_POOL` bounds its workers as the concurrent
client count rises, and whether every pooled worker lands on the right
scheduler band while connections are actually being served. Both are answered
with numbers below. The measurement also produced the connection-wall reading
that `doc/vxworks-port.md` §7 was waiting on, and it is not the wall §7
records.

Companion document: `doc/rtems-ca-worker-pool-on-target-measurement.md`, the
same measurement on `armv7-rtems-eabihf`. Where a figure differs from the RTEMS
one it is called out; the two ports share the pool implementation, so a
difference is either a target property or a defect.

---

## 0. What ran

Image, built on the box in a clone of this branch:

```
cargo +nightly build --profile release-embedded -j4 --no-default-features \
  -Zbuild-std=std,panic_abort --target x86_64-wrs-vxworks \
  --config 'patch.crates-io.libc-std.package="libc"' \
  --config 'patch.crates-io.libc-std.path="…/31d5776f9952aa349813d7fbef3addae1bf0a5ef-0.2.185"' \
  -p epics-ca-rs --bin realtime-ca-ioc --features client-core,bringup-probes
```

`--no-default-features` is load-bearing and `--profile release-embedded` is
not interchangeable with `--release`. Without the former the build stops with
eleven `E0433: cannot find hostname/discovery in crate` — the default features
pull in host-only discovery code that does not exist for this target. An
earlier revision of this section recorded `--release` and omitted
`--no-default-features`; that line never built, and the invocation above is
the one taken verbatim from `~/vx-rig-e8/build-poolprobe.log:109`.

The two `--config` lines come from `./scripts/libc-std-patch.sh nightly`, not
by hand. A hand-written bare `patch.crates-io.libc.path=…` does **not** reach
`std` under `-Zbuild-std`: the build fails with

```
error[E0425]: cannot find function `killpg` in crate `libc`
   --> library/std/src/sys/process/unix/vxworks.rs:179
```

because a config patch is only applied to the `-Zbuild-std` graph at exact
version equality with rust-src's own libc pin, and it has to be an **alias**
entry (`libc-std`) so the workspace graph keeps its committed pin. This is
already documented in `scripts/vxworks-check.sh`; it is repeated here because
it cost a build cycle.

Guest boot, RTP load, and the two drivers are in §9. Kill discipline follows
`doc/rtems-measurement-rig-shared-box-kill-safety.md`: the rig records only its
own pids and re-reads `/proc/<pid>/comm` before killing, because the shared box
also carries two long-running `qemu-system-arm` guests that must survive.

---

## 1. The probe patch, and why it is three hunks and not five

`doc/rtems-ca-pool-probe.patch` does not apply to this target. Of its five
hunks:

| RTEMS hunk | Disposition here |
|---|---|
| `task.rs` `enter_ioc_thread` + `rtems_priority_readback` | ported; path moved to `crates/epics-libcom-rs/src/runtime/task.rs` (issue 55 extraction), readback rewritten for the VxWorks instruments |
| `task.rs` `mod rtems_sched` (`pthread_getschedparam`) | folded into the readback; `libc::pthread_getschedparam` is already in scope, so no extern block is needed for the POSIX leg |
| `rtems-ca-ioc.rs` POOLPROBE | ported to `crates/epics-ca-rs/src/bin/realtime-ca-ioc.rs` |
| `blocking.rs` `client_pool_probe` | ported unchanged in substance |
| `stats.rs` `dump_tasks` | **dropped.** It targets `epics-rtems-boot`'s RTEMS backend, which a VxWorks image never links, and its reading is already produced: `crates/epics-rtems-boot/src/stats/vxworks.rs` prints `prio=` per registered thread from `taskInfoGet`. |

The result is `doc/vx-ca-pool-probe.patch` (committed separately, 3 hunks).
Dropping the `stats.rs` hunk is not a loss but a gain: the VxWorks census
`prio=` field is derived from `taskInfoGet`, which is a *third* priority
instrument independent of the two the readback uses. So each pooled worker's
band is asserted three ways:

1. `pthread_getschedparam` — the POSIX leg (`posix=`), in-thread.
2. `taskPriorityGet` — the **native** leg (`vx=`), in-thread. This one has no
   RTEMS counterpart. It is available to an RTP: `nmpentium` on
   `$SDK/vxsdk/sysroot/usr/lib/common/libc.a` shows `taskPriorityGet` as a
   `T`-defined symbol, unlike the kernel-only `taskIdListGet`/`taskEach`.
3. `taskInfoGet` — out-of-thread, from the census, at an unrelated time.

---

## 2. Bands of pooled workers under load — the pass criterion

`PRIOPROBE` fires once per thread inside `enter_ioc_thread`, so there is one
line per pooled worker ever created. On the 2048M guest, holding 141
concurrent connections:

```
$ grep -c PRIOPROBE console.log
300
$ grep "PRIOPROBE label=CAS-client" console.log | sed -E 's/label=CAS-client [0-9]+/label=CAS-client N/' | sort | uniq -c
    141 PRIOPROBE label=CAS-client N epics=20 applied=Realtime getsched_rc=0 policy=1 posix=76 taskprio_rc=0 vx=179 expect_posix=76 expect_vx=179
$ grep "PRIOPROBE label=CAS-event" console.log | sed -E 's/label=CAS-event [0-9]+/label=CAS-event N/' | sort | uniq -c
    141 PRIOPROBE label=CAS-event N epics=19 applied=Realtime getsched_rc=0 policy=1 posix=75 taskprio_rc=0 vx=180 expect_posix=75 expect_vx=180
```

300 = 18 baseline threads + 141 `CAS-client` + 141 `CAS-event`. After
collapsing the per-worker index, the 141 client lines and the 141 event lines
each collapse to **one** distinct tuple, so every pooled worker of a class got
byte-identical priority readings.

Mismatch count, over all 300 lines, comparing `posix` against `expect_posix`
and `vx` against `expect_vx`:

```
=== mismatches vx != expect_vx or posix != expect_posix ===
(count of mismatching PRIOPROBE lines:)
0
```

The arithmetic that holds:

| class | EPICS | `posix = 56 + epics` | `vx = 199 - epics` | `255 - posix` |
|---|---|---|---|---|
| `CAS-client` | 20 | 76 | 179 | 179 |
| `CAS-event` | 19 | 75 | 180 | 180 |

`vx = 199 - epics` and `vx = 255 - posix` are the same statement, since we set
`posix = 56 + epics`: VxWorks' POSIX layer inverts with `255 - posix`, and
`255 - (56 + epics) = 199 - epics`. Both forms were measured, in-thread, per
worker, and neither deviated once in 282 workers.

`policy=1` is `SCHED_FIFO`, `applied=Realtime`, `getsched_rc=0`,
`taskprio_rc=0` on every line — so the reading is not a default-value
artefact of a failed call.

Third instrument, from the census taken at load (out-of-thread,
`taskInfoGet`):

```
TASKDUMP tag=c6-42 id=0x000400e0 prio=179 name=CAS-client 0 stack_high=12160 stack_margin=2084992
TASKDUMP tag=c6-42 id=0x000400e3 prio=180 name=CAS-event 0 stack_high=5968 stack_margin=1042608
```

`prio=179` / `prio=180` agree with the in-thread `vx=` values.

Surrounding bands at the same census, for context — these are what the pooled
workers sit above (numerically lower = higher priority on VxWorks):

```
TASKDUMP tag=c6-42 id=0x0001003c prio=181 name=CAS-TCP     stack_high=6016  stack_margin=1042560
TASKDUMP tag=c6-42 id=0x00010040 prio=183 name=CAS-UDP     stack_high=5056  stack_margin=1043520
TASKDUMP tag=c6-42 id=0x00010034 prio=189 name=scan-owner  stack_high=5680  stack_margin=1042896
TASKDUMP tag=c6-42 id=0x00010038 prio=189 name=status-pv   stack_high=4728  stack_margin=519560
TASKDUMP tag=c6-42 id=0x000100a4 prio=189 name=c6-probe    stack_high=4544  stack_margin=1044032
TASKDUMP tag=c6-42 id=0x00010000 prio=220 name=iRealtime-ca-ioc stack_high=18800 stack_margin=46736
```

**Pooled workers (179/180) run above the acceptor (181), the search thread
(183), the reporter and the status-PV pusher (189).** That ordering is
intended, and §7 shows what it costs.

**Band criterion: PASS.** 282 pooled workers, 3 instruments, 0 mismatches.

---

## 3. The pool bound as concurrency rises

`POOLPROBE` prints `CAS_CLIENT_POOL.set_usage()` plus `worker_count()`.

### 3.1 1024M guest, plateau ramp

`poolramp.py` holds 1, 2, 4, 8, 16, 24, 32, 40 ramp connections, dwelling 75 s
at each (longer than the image's ~60 s census period), with 5 monitor
connections opened first and held throughout. Two independent derivations of
the served count: **D1** client-side completed handshakes still open, **D2**
`RTEMS:CA_CONN_CNT` read over CA.

| plateau (D1 ramp) | D1 total (+5 mon) | D2 `CA_CONN_CNT` at plateau exit | `FD_CNT` | `MEM_USED` |
|---|---|---|---|---|
| 1 | 6 | 6.0 | 11.0 | 17,145,856 |
| 2 | 7 | 7.0 | 12.0 | 25,559,040 |
| 4 | 9 | 9.0 | 14.0 | 25,608,192 |
| 8 | 13 | 13.0 | 18.0 | 25,706,496 |
| 16 | 21 | 21.0 | 26.0 | 34,291,712 |
| 24 | 29 | 29.0 | 34.0 | 42,876,928 |
| 32 | 37 | 37.0 | 42.0 | 51,462,144 |
| 40 | 45 | 45.0 | 50.0 | 60,047,360 |

D1 and D2 agree exactly at every plateau exit. (At each plateau *enter* D2
still reads the previous plateau's value — the status-PV pusher lags one update
cycle. See §7.2.)

`POOLPROBE` at the top of that run:

```
POOLPROBE seq=68 BUSY=47 SETS=47 CAP=141 WORKERS=94 REFUSED=4 CONNS=47
```

`BUSY == SETS == CONNS == 47`, `WORKERS == 2 × SETS == 94`, `SETS < CAP`.

### 3.2 2048M guest, straight ramp to the wall

```
POOLPROBE seq=6  BUSY=0   SETS=0   CAP=141 WORKERS=0   REFUSED=0 CONNS=0
POOLPROBE seq=7  BUSY=141 SETS=141 CAP=141 WORKERS=282 REFUSED=4 CONNS=141
POOLPROBE seq=8  BUSY=141 SETS=141 CAP=141 WORKERS=282 REFUSED=4 CONNS=141
… seq=9 … seq=15 identical …
POOLPROBE seq=16 BUSY=5   SETS=141 CAP=141 WORKERS=282 REFUSED=4 CONNS=5
… seq=17 … seq=23 identical …
POOLPROBE seq=24 BUSY=0   SETS=141 CAP=141 WORKERS=282 REFUSED=4 CONNS=0
… seq=25 … seq=42 identical …
```

At the top: `BUSY == SETS == CONNS == CAP == 141` and
`WORKERS == 2 × SETS == 282`. `SETS` never exceeded `CAP` in either run, and
`WORKERS == 2 × SETS` held on every one of the 42 + 125 `POOLPROBE` lines
across the two runs.

**Bound criterion: PASS.** Worker count tracked `2 × sets` exactly and sets
never passed the declared capacity, up to and including saturating it.

The `seq=6 → seq=7` step is not a missing print: the reporter's 10 s cadence
lapsed for the whole 402 s of the ramp. §7.2.

---

## 4. Two walls, and which resource each one is

This is the reading `doc/vxworks-port.md` §7 was waiting on, and the two guests
disagree, which is the useful part.

### 4.1 1024M (`~958MB`): wall at 47 concurrent, `EAGAIN` on worker spawn

```
[  602.5s] WALL attempt=43 (ramp #43) FAILED: REFUSED_BY_SERVER(CA_PROTO_ERROR:CAS: no resources for a new client (resource unavailable try again (os error 11)))  held=42 total=47
```

`os error 11` is `EAGAIN`, returned by the thread spawn inside the pool's
`acquire()`. Server side:

```
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (resource unavailable try again (os error 11)) — refused 10.0.2.2:38034 (refusal #1)
WARN  epics_ca_rs::server::blocking: refused a CA client for want of resources peer=10.0.2.2:38034 error=resource unavailable try again (os error 11) nth=1
```

At that wall `SETS=47`, well under `CAP=141`, and `FD_CNT=52` of
`FD_MAX=1000`. So neither the pool bound nor the fd table is what stopped it.

### 4.2 2048M (`~1982MB`): wall at 141 concurrent, the pool's own capacity

```
[  402.7s] mem2048 FAIL attempt=137 held=136 total=141 elapsed_conn=3.61s REFUSED_BY_SERVER(CA_PROTO_ERROR:CAS: no resources for a new client (worker pool at capacity)|hex=000b005000000000ffffffff00000030000000000000000000000000000000004341533a206e6f207265736f757263657320666f722061206e657720636c69656e742028776f726b657220706f6f6c2061742063617061636974792900000000)
[  402.8s] mem2048 === TOP ===
[  402.8s] mem2048 D1 client-side served = 136 ramp + 5 monitor = 141
[  402.8s] mem2048 deadline_hit=False consecutive_failures=4 ceiling_hit=False
[  492.9s] mem2048 SAMPLE top-held       CONN_CNT=141.0 REFUSED=4.0 FD_CNT=146.0 FD_MAX=1000.0 MEM_USED=171458560.0
```

Refusal frame, decoded field by field from that hex:

| bytes | field | value |
|---|---|---|
| `000b` | command | 11 = `CA_PROTO_ERROR` |
| `0050` | payload size | 80 |
| `0000` | data type | 0 |
| `0000` | data count | 0 |
| `ffffffff` | cid | `0xffffffff` |
| `00000030` | available | 48 = `ECA_ALLOCMEM` |
| next 16 B | echoed request header | all zero |
| remaining 64 B | message, NUL-padded | `CAS: no resources for a new client (worker pool at capacity)` |

96 bytes on the wire, same shape as the RTEMS addendum's frame, with a
different message tail. `available=48` is `ECA_ALLOCMEM` in both cases; only
the reason string distinguishes an `EAGAIN` refusal from a capacity refusal, so
a client cannot tell them apart from the status code alone.

Four consecutive refusals, then the driver stopped. `REFUSED=4` in `POOLPROBE`
matches the client's count exactly. The 141 already-established connections
were unharmed:

```
[  402.8s] mem2048 spot-check: 10/10 sampled held connections still answer a fresh READ_NOTIFY
```

### 4.3 What the pair of walls says

| | 1024M | 2048M |
|---|---|---|
| `OS Memory Size` | `~958MB` | `~1982MB` |
| concurrent connections reached | 47 | 141 |
| refusal reason | `EAGAIN` on spawn | `worker pool at capacity` |
| `SETS` at the wall | 47 (of `CAP=141`) | 141 (= `CAP`) |
| `FD_CNT` / `FD_MAX` at the wall | 52 / 1000 | 146 / 1000 |
| `MEM_USED` at the wall | 61,145,088 | 171,458,560 |
| reserved worker stack, `n × 3,145,728` | 147,849,216 | 443,547,648 |

Doubling guest RAM moved the wall from 47 to 141 concurrent connections and
changed the refusal reason. So:

* The 1024M wall is **not** the pool bound and **not** the fd table; it moves
  with guest RAM.
* It is **not** committed-memory exhaustion either: at that wall the RTP had
  61,145,088 B in use, 6.1 % of the ~958 MiB the guest reports. What scales
  with `n` and is large is the *reserved* stack — 3,145,728 B per connection
  (`CAS-client` Big 2,097,152 + `CAS-event` Medium 1,048,576) — 147,849,216 B
  reserved at the 1024M wall.
* At 2048M the binding constraint becomes `CAS_CLIENT_POOL_CAPACITY = 141`, and
  the pool's capacity arm behaves: it refuses cleanly with a CA-level error,
  keeps every established connection serving, and does not spawn a 142nd set.

The **exact** mechanism that turns "guest has 958 MB" into "spawn 48 returns
`EAGAIN`" is not measured here. Reserved-stack address space is the shape the
numbers have, but I did not instrument the RTP's VM arena, so §8 carries it as
open.

On the "44 clients at ~3 MiB each" in `doc/vxworks-port.md` §7: 44 was not
reproduced on either guest. The measured counts are 47 and 141, and the per
connection reservation is confirmed at 3,145,728 B. The client count is not a
constant of the port — it is a function of guest memory until the pool
capacity takes over.

The **1-in-3 wall-abort with mutex `EINVAL`** was **not observed**. Three wall
events were driven (1024M first ramp, 1024M second ramp, 2048M ramp) and all
three consoles contain zero matches for `EINVAL|Invalid argument|mutex|panic|
abort|Exception`:

```
console-run1-1024M.log             hits=0
console-run2-2048M-aborted.log     hits=0
console.log                        hits=0
```

Three observations with zero occurrences does not refute a 1-in-3 rate; it
fails to reproduce it. Stated as such.

---

## 5. Stack high-water per thread class, at each plateau

`STACKUSE` prints `size`, `current`, `high`, `margin` per registered thread.
Maximum `high` per class, per census pass, on the 2048M run:

| census tag | `CAS-client` visible / max `high` | `CAS-event` visible / max `high` |
|---|---|---|
| `c6-6` (pre-ramp) | 0 / — | 0 / — |
| `c6-12` | 87 / 13,240 | 86 / 5,968 |
| `c6-18` | 87 / 13,240 | 86 / 5,968 |
| `c6-24` | 87 / 13,240 | 86 / 5,968 |
| `c6-30` | 87 / 13,240 | 86 / 5,968 |
| `c6-36` | 87 / 13,240 | 86 / 5,968 |
| `c6-42` | 87 / 13,240 | 86 / 5,968 |

Identical to the 1024M run, whose plateaus swept n = 1 … 47: `CAS-client`
13,240 and `CAS-event` 5,968 at every plateau. **The high-water does not grow
with concurrency** — each connection's stack use is a property of the
connection, not of the load.

The "visible" column is a truncated sample, not the full 141: see §7.3.

Full per-class table, `size` and max `high` over the whole 2048M run:

| thread class | `size` | max `high` | used |
|---|---|---|---|
| `CAS-client` | 2,097,152 | 13,240 | 0.63 % |
| `CAS-event` | 1,048,576 | 5,968 | 0.57 % |
| `CAS-TCP` | 1,048,576 | 6,016 | 0.57 % |
| `CAS-UDP` | 1,048,576 | 5,056 | 0.48 % |
| `scan-owner` | 1,048,576 | 5,680 | 0.54 % |
| `scan-*` | 2,097,152 | 4,544 | 0.22 % |
| `scanOnce` | 2,097,152 | 3,392 | 0.16 % |
| `cbLow` | 2,097,152 | 3,392 | 0.16 % |
| `cbMedium` | 2,097,152 | 206,800 | 9.86 % |
| `cbHigh` | 2,097,152 | 3,392 | 0.16 % |
| `cbTimer` | 1,048,576 | 3,456 | 0.33 % |
| `c6-probe` | 1,048,576 | 4,544 | 0.43 % |
| `CAC-dial` | 524,288 | 3,248 | 0.62 % |
| `status-pv` | 524,288 | 4,728 | 0.90 % |
| `iRealtime-ca-ioc` (main) | 65,536 | 18,800 | 28.7 % |

Per connection: 13,240 + 5,968 = **19,208 B used of 3,145,728 B reserved
(0.61 %)**, flat from n = 1 to n = 141.

Two entries are worth the sizing decision §7 of `doc/vxworks-port.md` defers:

* `cbMedium` at 206,800 B is 15× the next-largest and 61× its own sibling
  `cbLow`/`cbHigh` — it is the class that actually needs `Big`.
* `iRealtime-ca-ioc`, the RTP's initial task, runs on a 65,536 B stack with
  18,800 B used and only 46,736 B of margin — 28.7 % consumed. That stack is
  the RTP's, not ours to class, and it is the tightest margin measured.

Committed memory per connection, from four consecutive 8-connection steps on
the 1024M run (25,706,496 → 34,291,712 → 42,876,928 → 51,462,144 →
60,047,360): each step is exactly **8,585,216 B for 8 connections =
1,073,152 B per connection**. So ~1.02 MiB of the 3 MiB reserved is actually
committed per connection — consistent with lazily-committed stack pages and
with the 19,208 B of it that is ever touched.

---

## 6. Bounded reuse, and no shrink

### 6.1 Reuse costs zero thread creations

The RTEMS run demonstrated reuse with 30 serial connect/disconnect cycles. The
concurrent counterpart here is stronger: a **second full concurrent ramp** to
the same count on a guest whose pool already holds its high-water mark.

On the 1024M guest, after the first ramp had reached 47 and released:

```
POOLPROBE seq=115 BUSY=0  SETS=47 CAP=141 WORKERS=94 REFUSED=4 CONNS=0
POOLPROBE seq=116 BUSY=47 SETS=47 CAP=141 WORKERS=94 REFUSED=8 CONNS=47
POOLPROBE seq=117 BUSY=47 SETS=47 CAP=141 WORKERS=94 REFUSED=8 CONNS=47
```

`SETS` 47 → 47, `WORKERS` 94 → 94. And the creation counter agrees:

```
$ grep -c "PRIOPROBE label=CAS-client" console-run1-1024M.log
47
$ grep -c "PRIOPROBE label=CAS-event" console-run1-1024M.log
47
$ grep -c PRIOPROBE console-run1-1024M.log
112
```

112 = 18 baseline + 47 + 47, unchanged across both ramps. `PRIOPROBE` fires
once per thread creation, so **the second ramp of 47 concurrent clients created
zero threads.** `MEM_USED` was byte-identical too (61,145,088 before and
after).

### 6.2 The pool does not shrink

1024M, after releasing all 47:

```
[  677.7s] SAMPLE top-held         CONN_CNT=47.0 REFUSED=4.0 FD_CNT=52.0 FD_MAX=1000.0 MEM_USED=61145088.0
[  752.7s] SAMPLE released         CONN_CNT=5.0  REFUSED=4.0 FD_CNT=10.0 FD_MAX=1000.0 MEM_USED=61145088.0
```

2048M, after releasing all 141:

```
[  492.9s] mem2048 SAMPLE top-held       CONN_CNT=141.0 REFUSED=4.0 FD_CNT=146.0 FD_MAX=1000.0 MEM_USED=171458560.0
[  582.9s] mem2048 SAMPLE released       CONN_CNT=5.0   REFUSED=4.0 FD_CNT=10.0  FD_MAX=1000.0 MEM_USED=171458560.0
```

`FD_CNT` drops (the sockets do close) and `CONN_CNT` falls to the 5 monitors,
but `SETS`/`WORKERS` stay at 141/282 and `MEM_USED` does not move a single
byte. Same deviation as RTEMS §4, now measured at 141 sets: **a burst of 141
clients permanently costs the IOC 282 threads and 154,460,160 B, whether or not
another client ever connects.** By design — the pool trades retention for
bounded steady-state cost — recorded because the retention is now a
154 MB-scale number rather than a 44 MB one.

---

## 7. Observed under load, not root-caused

### 7.1 Connection-establishment cost knees hard at ~80 concurrent

From the 2048M drive log (`last_conn` is the wall-clock cost of the whole
handshake: connect, VERSION, SEARCH, CREATE_CHAN, READ_NOTIFY with a decoded
reply):

```
[    0.6s] mem2048 UP held=8   total=13  last_conn=0.04s
[    1.0s] mem2048 UP held=16  total=21  last_conn=0.04s
[    1.3s] mem2048 UP held=24  total=29  last_conn=0.04s
[    1.7s] mem2048 UP held=32  total=37  last_conn=0.04s
[    2.1s] mem2048 UP held=40  total=45  last_conn=0.04s
[    2.5s] mem2048 UP held=48  total=53  last_conn=0.04s
[    2.9s] mem2048 UP held=56  total=61  last_conn=0.04s
[    3.3s] mem2048 UP held=64  total=69  last_conn=0.04s
[    3.8s] mem2048 UP held=72  total=77  last_conn=0.12s
[    4.1s] mem2048 UP held=80  total=85  last_conn=0.04s
[   48.0s] mem2048 UP held=88  total=93  last_conn=7.34s
[  106.9s] mem2048 UP held=96  total=101 last_conn=7.35s
[  166.1s] mem2048 UP held=104 total=109 last_conn=7.46s
[  224.2s] mem2048 UP held=112 total=117 last_conn=7.24s
[  282.5s] mem2048 UP held=120 total=125 last_conn=7.23s
[  340.7s] mem2048 UP held=128 total=133 last_conn=7.33s
[  399.1s] mem2048 UP held=136 total=141 last_conn=7.25s
```

Connections 1 – 80 cost 0.04 s each. Somewhere in 81 – 88 the cost jumps by
**180×** and then stays flat at 7.23 – 7.46 s per connection for the remaining
56. The driver logs every 8th success, so the transition is bracketed to
(80, 88] and not pinned.

Not root-caused. The measurement does exclude two candidates: it is not the fd
table (`FD_CNT=146` of 1000) and it is not the pool's capacity check (that
fires later, at 142). It is also not visible at all below the knee on the
1024M guest, whose wall at 47 is under it.

### 7.2 The reporter and the status-PV pusher starve while connections establish

During the 402 s ramp the 10 s reporter printed **nothing**: `POOLPROBE seq=6`
is the last pre-ramp line and `seq=7` is the first post-ramp line. The status
PVs froze at their boot values for the whole ramp, and D2 was therefore wrong
while D1 climbed:

```
[    1.8s] mem2048 SAMPLE held=32        CONN_CNT=0.0 REFUSED=0.0 FD_CNT=5.0 FD_MAX=1000.0 MEM_USED=16998400.0
[  106.9s] mem2048 SAMPLE held=96        CONN_CNT=0.0 REFUSED=0.0 FD_CNT=5.0 FD_MAX=1000.0 MEM_USED=16998400.0
[  340.7s] mem2048 SAMPLE held=128       CONN_CNT=0.0 REFUSED=0.0 FD_CNT=5.0 FD_MAX=1000.0 MEM_USED=16998400.0
[  402.8s] mem2048 SAMPLE top            CONN_CNT=0.0 REFUSED=0.0 FD_CNT=5.0 FD_MAX=1000.0 MEM_USED=16998400.0
[  492.9s] mem2048 SAMPLE top-held       CONN_CNT=141.0 REFUSED=4.0 FD_CNT=146.0 FD_MAX=1000.0 MEM_USED=171458560.0
```

Note the `top` sample at t=402.8 s: the ramp had finished and 141 connections
were established, and the status PV still read 0. It caught up within the
following 90 s hold, reading 141.0 exactly. So the values are correct but
arbitrarily stale under load — the read path answered every time (these
`SAMPLE` rows are CA reads that completed), it answered with old data.

The band measurements in §2 are the direct explanation for the shape: pooled
workers are at 179/180 and `status-pv`, `scan-owner`, `c6-probe` are all at
**189**, ten levels below. Under sustained connection establishment the 189
band gets no CPU on this 1-vCPU guest. That is the priority model doing exactly
what §4 of `doc/vxworks-port.md` says it should. The consequence for
*measurement* is what matters here, and it is sharp:

**On this target, any status PV or console counter is a lagging indicator under
load, and D2 alone cannot be trusted to derive a served count.** The 1024M
plateau run got exact D1/D2 agreement only because each plateau dwelled 75 s
with the load *static*. Every future row that reads `RTEMS:*` status PVs to
count anything under load needs the client-side derivation beside it.

### 7.3 The task census truncates at the pool's capacity

```
TASKDUMP begin tag=c6-42 count=192 capacity=192 dropped=109 source=registry
```

192 + 109 = 301 = 19 baseline + 282 pooled workers. `MAX_TASKS = 192` in
`crates/epics-rtems-boot/src/stats/vxworks.rs`, and 141 concurrent CA clients
alone need 282 slots. So at the pool's own capacity **the census is blind to
109 of 301 threads**, and the §5 per-class figures rest on 87 of 141
`CAS-client` and 86 of 141 `CAS-event` threads.

That does not weaken §5's conclusion — the high-water is identical across all
87 visible instances and across every pass, and it is identical to the 1024M
run where `dropped=0` at all times (max `count=113`) — but it does mean the
census as it stands cannot audit a saturated pool. The registry counts
correctly (`dropped` is exact); it is the dump that truncates.

The RTEMS run never hit this: its wall was 142 fds with a smaller worker
footprint, and `dropped=0` throughout.

### 7.4 The errlog leg of a refusal is dropped, silently

Every refusal produces two independent console records: a `WARN` from
`epics_ca_rs::server::blocking` and an `ERROR` from `epics_base_rs::errlog`.
They do not agree on how many refusals happened.

2048M, 4 refusals — errlog prints #1, #2, #4, and **not #3**:

```
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (worker pool at capacity) — refused 10.0.2.2:35096 (refusal #1)
WARN  epics_ca_rs::server::blocking: refused a CA client for want of resources peer=10.0.2.2:35096 error=worker pool at capacity nth=1
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (worker pool at capacity) — refused 10.0.2.2:35100 (refusal #2)
WARN  epics_ca_rs::server::blocking: refused a CA client for want of resources peer=10.0.2.2:35100 error=worker pool at capacity nth=2
WARN  epics_ca_rs::server::blocking: refused a CA client for want of resources peer=10.0.2.2:35110 error=worker pool at capacity nth=3
ERROR epics_base_rs::errlog: sevr=major CAS: no resources for a new client (worker pool at capacity) — refused 10.0.2.2:35124 (refusal #4)
WARN  epics_ca_rs::server::blocking: refused a CA client for want of resources peer=10.0.2.2:35124 error=worker pool at capacity nth=4
```

1024M, 8 refusals across two walls — errlog prints #1, #2, #4, #8 and not
#3, #5, #6, #7:

```
ERROR … (refusal #1)
WARN  … nth=1
ERROR … (refusal #2)
WARN  … nth=2
WARN  … nth=3
ERROR … (refusal #4)
WARN  … nth=4
…
WARN  … nth=5
WARN  … nth=6
WARN  … nth=7
ERROR … (refusal #8)
WARN  … nth=8
```

Counts:

```
console-run1-1024M.log         errlog=4 WARN=8
console.log                    errlog=3 WARN=4
```

Two facts follow. First, the errlog **counter** is right — it labels the 8th
refusal `#8` — so all 8 calls were made and the messages were lost after the
call, not before it. Second, **no discard notice was emitted**:

```
console-run1-1024M.log         discarded-notices=0
console.log                    discarded-notices=0
```

C base's errlog announces `errlog: N messages were discarded`. Ours drops
silently, so a refusal storm reads as fewer refusals than occurred to anyone
watching the console. The `WARN` leg is the reliable one; both walls also had
`POOLPROBE REFUSED=` matching the client's count exactly, so the pool's counter
is reliable too. Only the errlog console record undercounts. Not root-caused —
the drop mechanism (queue depth, sink, or the interleave with `println!` on the
same serial device) is not instrumented.

The ordering is also non-deterministic between the two sinks: in the 2048M
block `ERROR … #2` precedes `WARN … nth=2`, while in the 1024M block the
`nth=3` `WARN` appears with no `ERROR` at all around it.

---

## 8. UNFIXED

1. **The mechanism of the 1024M `EAGAIN` wall.** Measured that the wall moves
   with guest RAM (47 → 141 when RAM doubles) and that committed memory is not
   exhausted at it (61,145,088 B of ~958 MiB). Reserved stack address space
   (3,145,728 B per connection) is the term that scales, but the RTP's VM arena
   was not instrumented, so "why 47 and not 60" is unmeasured.
2. **The 180× connection-establishment knee at ~80 concurrent** (§7.1).
   Bracketed to (80, 88], flat at ~7.3 s per connection above it, fd table and
   pool capacity both excluded. Cause unknown.
3. **Status PVs and the console reporter starve under connection load**
   (§7.2). Understood as a band-ordering consequence, not fixed, and it
   invalidates D2-only derivations in every future row on this target.
4. **The task census truncates at `MAX_TASKS = 192`** (§7.3), which 141
   concurrent CA clients exceed on their own (282 worker threads). `dropped=`
   is honest; the dump is incomplete. A saturated pool cannot be fully audited
   with the census as it stands.
5. **The errlog leg of a refusal is dropped without a discard notice** (§7.4).
   4 of 8 lost on one guest, 1 of 4 on the other.
6. **The pool never shrinks** (§6.2). By design; now a 154,460,160 B / 282
   thread retention rather than RTEMS's 44 MB / 94 thread one.
7. **`available=48` (`ECA_ALLOCMEM`) cannot distinguish an `EAGAIN` refusal
   from a capacity refusal** (§4.2). Only the human-readable tail differs, so a
   client that logs the status code alone cannot tell "add RAM" from "raise the
   pool bound".
8. **The 1-in-3 wall-abort with mutex `EINVAL` was not reproduced** in three
   wall events (§4.3). Zero occurrences is not a refutation at that claimed
   rate.
9. **`MEM_FREE` / `MEM_MAX` remain `NaN`** (`MEM_FREE=-1` in every `C6` line).
   Pre-existing, §3.1 of `doc/vxworks-port.md` is the standing rejection.
10. **No C counterpart.** C base supports VxWorks 6.6–6.9 only, so there is no
    C IOC on this target to compare a wall or a band against. The comparison
    used throughout is against the RTEMS port beside it.

---

## 9. Reproduction

Rig scripts are preserved verbatim in `doc/vx-rig-e8/` — the box they run on is
not backed up. Ports are the E8 block: host `31534`/`35064`/`35075`, ftpd
`2141`, passive `60020-60025`, console socket `/tmp/vxcon-e8.sock`.

| file | role |
|---|---|
| `doc/vx-rig-e8/ftpd-e8.py` | host ftpd `netDrv` loads the RTP through; `masquerade_address = 10.0.2.100` |
| `doc/vx-rig-e8/boot-e8.sh` | boots the guest, records only its own pids, bridges the console |
| `doc/vx-rig-e8/stop-e8.sh` | kills by recorded pid after re-reading `/proc/<pid>/comm` |
| `doc/vx-rig-e8/poolramp.py` | plateau driver: 1,2,4,8,16,24,32,40 held, 75 s dwell each, D1+D2 |
| `doc/vx-rig-e8/wallprobe2.py` | straight ramp to the wall, own deadline, logs its own progress |
| `doc/vx-rig-e8/phaseramp.py` | as `wallprobe2`, but logs **every** connection and splits the handshake into `connect`/`search`/`create`/`read` — the split that root-caused §11. Runs its ramp at module level, so it cannot be imported |
| `doc/vx-rig-e8/retention.py` | idle / burst / after-everyone-left, sampling on fresh connections each time — §12. Self-contained because `phaseramp` cannot be imported |
| `doc/vx-rig-e8/wallprobe.py` | first-cut ramp (logs failures only) — kept because §6.1's reuse run used it |

```sh
# 1. host ftpd
setsid python3 ftpd-e8.py ~/vx-rig-e8/ftp/root > ftpd.log 2>&1 < /dev/null &
echo $! > ftpd.pid

# 2. guest.  The argument is -m; both readings in this document come from
#    the same image on 1024M and then 2048M.
./boot-e8.sh 1024M      # or 2048M

# 3. load the RTP
echo 'rtpSp "/host.host/realtime-ca-ioc.vxe"' > ~/vx-rig-e8/con.in

# 4a. plateau ramp (section 3.1, section 5)
python3 -u poolramp.py 75 120 > poolramp-drive.log 2>&1

# 4b. straight ramp to the wall (section 4.2).  Arguments:
#     ceiling, hold seconds, tag, internal deadline seconds.
python3 -u wallprobe2.py 200 90 mem2048 600 > wallprobe2-mem2048.log 2>&1

# 4c. ramp to the wall logging EVERY connection with the handshake split into
#     phases (sections 10, 11).  Same four arguments as wallprobe2.
python3 -u phaseramp.py 200 90 medium1024 300 > phaseramp-medium1024.log 2>&1

# 4d. retention: idle / burst / after-everyone-left, on fresh connections each
#     time (section 12).  Arguments: burst, settle seconds, tag, hold seconds.
#     The hold is load-bearing -- a burst faster than the status-PV scan is
#     sampled through pre-burst values and the retention figure comes out zero.
python3 -u retention.py 40 75 retA2 90 > retention-retA2.log 2>&1

# 5. stop -- by recorded pid only
./stop-e8.sh
```

`wallprobe2.py` carries its own deadline because the first 2048M attempt was
driven under an external `timeout 400` and was decapitated at ~141
connections, leaving no record of its own ramp; the coincidence with
`CAP=141` was very nearly mistaken for the capacity wall. A driver that logs
only failures and relies on an external timeout produces exactly one useless
run.

Do not use `~/rtems-bringup/rigpid.sh` on a VxWorks guest: its `rig_is_qemu`
hardcodes `comm == "qemu-system-arm"`, so `qemu-system-x86_64` guests are
silently skipped and accumulate while the operator believes they were cleaned
up. `stop-e8.sh` checks the comm it actually expects, per pidfile, and never
uses `pkill`.

---

## 10. Follow-up round: the wall is a reservation ceiling

Measured 2026-07-26/27, same rig, same box. UNFIXED 1 asked why `EAGAIN` lands
at 47 concurrent on the `~958MB` guest when only 61,145,088 B of it — 6.1 % —
was committed. The discriminator was a `StackSizeClass` A/B at fixed guest RAM:
if the wall moves when the *declared* per-connection stack moves, reservation
is a binding term, and the RTP's VM arena never has to be instrumented.

Three arms, one guest size (`1024M`, `OS Memory Size: ~958MB`), one image each,
differing only in `client_roster()`:

| arm | `CAS-client` | `CAS-event` | declared per conn | wall | failure at the wall |
|---|---|---|---|---|---|
| A | Big 2,097,152 | Medium 1,048,576 | 3,145,728 | 47 | `EAGAIN` refusal, IOC survives |
| B | Medium 1,048,576 | Medium 1,048,576 | 2,097,152 | 59 | `EAGAIN` refusal, 0 panics |
| C | Small 524,288 | Small 524,288 | 1,048,576 | 80 | mutex `EINVAL`, RTP aborted |

Each arm's class is confirmed from the target's own census, not from the source
edit — `size=` moves and `high=` does not:

```
STACKUSE tag=c6-6  id=0x000300fa name=CAS-client 0 size=1048576 current=1536 high=12160 margin=1036416
STACKUSE tag=c6-12 id=0x000600e3 name=CAS-client 7 size=524288  current=6096 high=13240 margin=511048
```

The high-water is invariant across all three arms: `CAS-client` 13,240 B,
`CAS-event` 5,968 B. Changing the class changes what is reserved and nothing
about what is touched, which is the point of the experiment.

### 10.1 Which model survives

Two candidate readings of the wall, each predicting the C arm from A and B:

*Wall proportional to declared stack.* Totals at the wall would be constant.
They are 147,849,216 / 123,731,968 / 83,886,080 B — a 76 % spread, and the
model predicts the C arm walls at 147,849,216 / 1,048,576 = 141, which is
`CAS_CLIENT_POOL_CAPACITY`. It would have refused with "worker pool at
capacity". **Falsified**: the C arm walled at 80, with `EAGAIN`.

*Each thread costs its declared stack plus a fixed overhead.* Fitting that
overhead from arms A and B alone gives 1,004,885 B per thread and predicts the
C arm at 79.2 connections. **Measured 80.** Taking the overhead as exactly
1 MiB — a round number chosen a priori, no fitting — the totals at all three
walls become:

| arm | per conn incl. 2 × 1 MiB | wall | total reserved at the wall |
|---|---|---|---|
| A | 5,242,880 | 47 | 246,415,360 |
| B | 4,194,304 | 59 | 247,463,936 |
| C | 3,145,728 | 80 | 251,658,240 |

246.4 / 247.5 / 251.7 MB: a 2.1 % spread across arms whose declared stacks
differ threefold. **The wall is a reserved-address-space ceiling of ~248 MB on
this guest, and each pool thread consumes its declared stack plus ~1 MiB.**
That answers UNFIXED 1: `EAGAIN` at 47 is not committed-memory exhaustion at
all — 47 × 5,242,880 = 246,415,360 B is the whole ceiling, reached while
mimalloc had committed 61,145,088 B.

The ceiling itself moves with guest RAM, which is why §4 saw the wall move.
`1470MB` and `1982MB` both reached 141 (`CAS_CLIENT_POOL_CAPACITY`) without an
`EAGAIN`, so their ceiling is only bounded below, at ≥ 141 × 5,242,880 =
739,246,080 B. One exact point and one bound are consistent with a roughly
fixed ~710–730 MB of non-arena reservation, but two points cannot establish
that and it is not claimed.

### 10.2 What the per-connection class should be

Not what the arithmetic first suggests. At the Small arm, 2,097,152 B of the
3,145,728 B per connection — **67 %** — is the fixed per-thread overhead, not
declared stack. So the class is the minor lever and has sharply diminishing
returns: Big→Medium on the client cut the declared stack 33 % and bought 26 %
more connections; Medium→Small on both cut it a further 50 % and bought 36 %.
The major lever is **threads per connection**, because the overhead is charged
per thread: at Medium, one thread per connection instead of two would halve the
per-connection cost exactly, doubling the wall — more than any class change
achieved.

On the class question as asked: Big is not justified by anything measured here.
It costs 20 % of the wall on the small guest and its `CAS-client` high-water is
13,240 B, identical to Medium's and to Small's. **Medium/Medium is what this
evidence supports** — 79× headroom over the measured high-water, and the only
one of the three arms that walled cleanly, refusing with `EAGAIN` and zero
panics while the IOC kept serving.

Small is **not** recommended on this evidence: 40× headroom, and its wall event
took the whole RTP down (§10.4).

The caveat that keeps this from being a shipping recommendation: 13,240 B was
measured on `READ_NOTIFY` against a single `ao` record. CA command dispatch
depth is workload-dependent, and a database with long `FLNK` chains or large
array puts has not been measured on this target. What the evidence does
establish is that **Big buys nothing measurable and costs a fifth of the
connection wall**, and that the class is not where the leverage is.

---

## 11. The 180× knee and the census truncation were one defect

UNFIXED 2 (the connection-cost knee, §7.1) and UNFIXED 4 (the census
truncating at `MAX_TASKS = 192`, §7.3) were filed as separate observations.
They are the same root cause, fixed in `fix(vxworks-stats): grow the task
registry instead of capping it at 192`.

`TaskRegistry` in `crates/epics-rtems-boot/src/stats/vxworks.rs` held a fixed
192-entry `ids` array. The count is not a coincidence: 19 baseline threads plus
two per connection reaches 192 at

```
19 + 2 × (n + 5) = 192   →   n = 81.5
```

which is exactly where §7.1 bracketed the knee, `(80, 88]`, and where the
single 402 s ramp showed its one spike, at n = 83.

Once the array was full, `insert()` called `retain_live()` — one `taskInfoGet`
kernel query per entry, holding the registry `Mutex` — on **every** subsequent
registration, and reclaimed nothing, because at a saturated pool every task in
it is still live. One sweep timed at 3.674 s, and `enter_ioc_thread` registers
twice per CA connection (`CAS-client` and `CAS-event`), giving 2 × 3.68 s ≈
7.36 s per connection against the 7.23–7.46 s §7.1 measured. The phase-split
driver settles what §7.1 could not say:

```
[    0.3s] mem1536 UP n=  1 total=  6 total_s= 0.045 connect= 0.000 search= 0.027 create= 0.006 read= 0.011
[   10.3s] mem1536 UP n=136 total=141 total_s= 0.037 connect= 0.000 search= 0.028 create= 0.005 read= 0.004
```

`connect` is 0.000 on every line, at every count, so the cost was never in the
accept path. It sat in `search` — the leased worker's path to its first read —
because `enter_ioc_thread` registers before the worker reaches its socket.

After the fix the registry grows, and `SWEEP_THRESHOLD_MIN` keeps only its
second job: the point at which exited tasks are swept, re-armed to twice the
live count so the sweep stays amortised against the threads that exist.

Verified on target, both symptoms at once:

```
[   10.4s] mem1536 D1 client-side served = 136 ramp + 5 monitor = 141
TASKDUMP begin tag=c6-66 count=301 capacity=unbounded dropped=0 source=registry
```

The 141-client ramp fell from **402 s to 10.4 s**, and the saturated census
reports all 301 tasks — 19 baseline + 141 `CAS-client` + 141 `CAS-event` —
with `dropped=0`, where it previously reported `count=192 dropped=109`.

The RTEMS shim has the same symptom and **is not fixed**:
`crates/epics-rtems-boot/csrc/rtems_stats.c:148` defines
`EPICS_RTEMS_DUMP_MAX_TASKS 192` and sizes three static arrays by it. That
capacity is filled inside `epics_rtems_dump_collect`, called from a
`rtems_task_iterate` visitor, where allocating is not safe — so the same fix
does not transfer, and the constraint is structural rather than an oversight.
Nothing in the VxWorks path runs in that context: `register_task` is called
from `enter_ioc_thread` at thread startup, and `snapshot` already allocates.
Not verifiable from this rig either way; it is an `armv7-rtems-eabihf` item.

---

## 12. The pool never shrinks, measured against C `rsrv`

UNFIXED 6 recorded the non-shrink and called it "by design". This round put a
number on it and checked the reference, and the conclusion changes: **on a
memory-constrained guest it is a defect, not a design choice.**

C `rsrv` is the reference shape and it is the opposite of a pool. Each accepted
TCP client gets a thread created for it —
`epicsThreadCreate("CAS-client", epicsThreadPriorityCAServerLow, epicsThreadGetStackSize(epicsThreadStackBig), camsgtask, pClient)`
at `modules/database/src/ioc/rsrv/caservertask.c:109` — and an event thread from
`db_start_events(client->evuser, "CAS-event", …)` at `:1514`. Both are torn
down per client: `destroy_tcp_client` calls `db_close_events(client->evuser)`
and `camsgtask` returns on disconnect. **C's steady-state retention after a
burst is zero threads.**

Ours, measured directly rather than argued from the source. Burst of 40 held 90 s
so the status-PV scan refreshes, then every ramp connection dropped, then 75 s
settle, then sampled again on *fresh* connections:

```
[    0.1s] retA2 idle      CONN_CNT=0.0  REFUSED=0.0 FD_CNT=5.0  MEM_USED=60047360.0
[   91.1s] retA2 top       CONN_CNT=40.0 REFUSED=0.0 FD_CNT=45.0 MEM_USED=60047360.0
[  166.2s] retA2 after     CONN_CNT=0.0  REFUSED=0.0 FD_CNT=5.0  MEM_USED=60047360.0
POOLPROBE seq=68 BUSY=0 SETS=45 CAP=141 WORKERS=90 REFUSED=0 CONNS=0
```

Everything cheap comes back and the one expensive thing does not:

| resource | at the top | after every client left |
|---|---|---|
| file descriptors | `FD_CNT=45` | `FD_CNT=5` — returned |
| connections | `CONN_CNT=40` | `CONN_CNT=0` — returned |
| committed heap | `MEM_USED` unchanged | unchanged — nothing to return |
| **threads and their stack reservation** | `SETS=45 WORKERS=90` | **`SETS=45 WORKERS=90`, `BUSY=0 CONNS=0`** |

Held across every reporter cycle to the end of the run. A cold-pool repeat
(`rtpDelete` then `rtpSp`, `SETS=0 WORKERS=0` confirmed before the burst) walled
at 42 sets and ended the same way, `POOLPROBE seq=20 BUSY=0 SETS=42 CAP=141
WORKERS=84 REFUSED=2 CONNS=0`.

`MEM_USED` moving zero bytes across a 40-client burst is not the status-PV
staleness of §7.2 — `CONN_CNT` and `FD_CNT` tracked correctly in the same
samples. It is the pool working as intended: those 45 sets already existed, so
the burst allocated nothing. The §6.2 figure of 1,073,152 B per connection is
growth of the *pool*, charged on first use of a set, not per connection.

**Why this is a defect and not a trade.** §10 established that the binding
resource on this guest is reserved address space, ceiling ~248 MB. The 42
retained sets are 42 × 3,145,728 = 132,120,576 B — **53 % of the guest's entire
RTP reservation ceiling, held permanently with zero clients attached.** A
transient burst is thereby converted into a permanent exhaustion: the address
space is never returned, so nothing else in that RTP — the PVA server, the
database, file I/O — can ever have it back. C, retaining zero, has no such
mode.

**Why it is nonetheless not a one-line fix.** The pool exists because of the
measured per-thread leak on the sister target: every `std::thread` leaks 128 B
on RTEMS, so create-and-destroy-per-client leaks without bound. Shrinking the
pool reintroduces thread destruction. Two things must be said honestly here:
the VxWorks per-thread-exit leak is **unmeasured** — it cannot be measured
through a pool that never destroys a thread — and even granting a leak of the
RTEMS size, retaining a thread costs 3,145,728 B of reservation to avoid 128 B,
a ratio of 24,576:1. The arithmetic favours shrinking decisively; the missing
measurement is what the shrink would cost, not whether it is worth wanting.

The structural fix is the same one §10 points at from the other side: bound the
pool by a **reservation budget** rather than by a fixed set count.
`CAS_CLIENT_POOL_CAPACITY = 141` bounds the wrong quantity — on the `~958MB`
guest it is never the binding term, so the IOC walks past the real ceiling into
a failing `pthread_create` whose outcome ranges from a clean refusal to an RTP
abort (§13). A budget bound would refuse before thread creation can fail, and
would give idle-set release a natural trigger. That is a semantic change to a
public constant and is **not** made here; it is raised for sign-off.

---

## 13. The wall-abort mutex `EINVAL` reproduced

UNFIXED 8 recorded that the 1-in-3 wall-abort with mutex `EINVAL` did not
reproduce in three wall events, and the follow-up brief judged it not fixable
on that basis, asking only that it be captured if seen. **It was seen, twice,
in four wall events this round**, and it is no longer unexplained in the way it
was.

A arm, `~958MB`, at the wall — three panics, `CAS-client 48`, `CAS-client 49`
and `CAS-event 49`:

```
ERROR epics_base_rs::errlog: sevr=fatal panic on thread `CAS-client 48` at /home/coding-agent/.rustup/toolchains/nightly-x86_64-unknown-linux-gnu/lib/rustlib/src/rust/library/std/src/sys/pal/unix/sync/mutex.rs:69: failed to lock mutex: invalid argument (os error 22) -- that thread is gone and nothing restarts it; the IOC keeps listening and keeps answering searches, so from outside it still looks healthy
```

C arm, `~958MB`, at the wall — one panic, and this time it took the process:

```
ERROR epics_base_rs::errlog: sevr=fatal panic on thread `CAS-event 79` at …/sync/mutex.rs:69: failed to lock mutex: invalid argument (os error 22) -- …
fatal runtime error: an irrecoverable error occurred while synchronizing threads, aborting
0xffff800012127000 (CAS-event 79): RTP 0xffff800008cc2000 has been deleted due to signal 6.
```

What the four wall events say:

| arm | guest | wall | panics | outcome |
|---|---|---|---|---|
| A (Big/Medium) | ~958MB | 47/48 | 3 | IOC survived, kept serving |
| B (Medium/Medium) | ~958MB | 59 | 0 | clean `EAGAIN` refusal |
| C (Small/Small) | ~958MB | 80 | 1 | **RTP deleted, signal 6** |
| A | ~1470MB, ~1982MB | 141 | 0 | pool-capacity refusal |

It fires **only at the reservation wall** — never below it, in any of the
sub-wall ramps this round, and never on a guest large enough that the pool
capacity is reached first. The failing mutex is in the worker body, not in the
census registry: the `PRIOPROBE` line for `CAS-client 48` printed immediately
before its panic, and `register_task()` runs before that readback inside
`enter_ioc_thread`, so the registry mutex had already locked successfully.

That converts "1-in-3, unexplained" into a resource-exhaustion-correlated
failure: at the point where thread creation is failing, a mutex is being locked
that was never successfully initialised, and `pthread_mutex_lock` returns
`EINVAL`. Whether the uninitialised mutex is one of std's or one of ours is
**not** established here, and neither is the exact allocation that fails — that
would need the RTP arena instrumented, which §10 deliberately avoided needing.

The consequence is the part that matters for the port, and it is worse than
UNFIXED 8 implied: **a client that fills the pool can take the whole IOC
down.** Not "one worker thread is lost while the IOC looks healthy" — in the C
arm the RTP was deleted by signal 6. This is the strongest argument for the
budget-bounded admission in §12: the pool must refuse *before* thread creation
can fail, because the failure mode past that point is not a refusal at all.

---

## 14. New: a panicking worker leaks its pool set permanently

Not one of the four follow-up items; found while measuring §13, in shared code
rather than in the VxWorks backend, so it is recorded and not fixed here.

After the three A-arm panics of §13, with every client disconnected:

```
POOLPROBE seq=13 BUSY=2 SETS=50 CAP=141 WORKERS=100 REFUSED=0 CONNS=0
```

`CONNS=0` and `BUSY=2`. Three worker threads died across two sets, and those
two sets are leased forever: nothing returns a `SetLease` whose worker panicked,
so `became_free()` is never reached for them and `free_if_idle()` can never
reclaim them. The capacity is permanently 139 instead of 141, and each further
panic costs another set.

This is `WorkerPool`'s accounting, in `worker_pool.rs`, shared with
`armv7-rtems-eabihf` — not a VxWorks-backend defect, and not reachable from the
E8 row's scope. The invariant it breaks is the symmetric-accounting one: a set
is marked busy by the actor that leased it, and must be released by the actor
that finished with it, but a panicking worker is neither — it exits without
passing through either owner. Stated here so it is not rediscovered from the
`BUSY=2 CONNS=0` line alone.

It compounds §13: at the reservation wall a panic is likelier, and each panic
permanently shrinks the pool that the wall already constrained.

## 15. The `StackSizeClass` decision, measured against a realistic database

§5 and §10.2 could only report a **13,240 B** `CAS-client` high-water,
because every driver in those rounds did `READ_NOTIFY` against a single
`ao`. That is the shallowest CA request there is, so §10.2 refused to
decide the class off it. This is that measurement redone against a record
set chosen so each shape the decision depends on is actually driven.

### 15.1 What was driven

`E8_STACK_DB` (`crates/epics-ca-rs/src/bin/realtime-ca-ioc.rs`), through
`doc/vx-rig-e8/stackload.py`, 4 connections, one cold RTP on a `1024M`
guest (`OS Memory Size: ~958MB`):

| op | reply / effect | outcome |
| --- | --- | --- |
| `get_WF` | 32,768 `DOUBLE` → 262,144 B, extended 24-byte header | 48/48 ok |
| `get_WFBIG` | 131,072 `DOUBLE` → **1,048,576 B** | 48/48 ok |
| `get_WF2` | 8,192 `DOUBLE` → 65,536 B | 48/48 ok |
| `get_SA` | `subArray` window via `ArrayKind::SubArray` → 32,768 B | 48/48 ok |
| `get_WF_ctrl` | `DBR_CTRL_DOUBLE`, full control block → 262,224 B | 48/48 ok |
| `get_WF_time` | `DBR_TIME_DOUBLE`, stamped → 262,160 B | 48/48 ok |
| `get_BIG_ctrl` | `DBR_CTRL_DOUBLE` on the 1 MiB array → 1,048,656 B | 48/48 ok |
| `get_SA_ctrl` | `DBR_CTRL_DOUBLE` on the window → 32,848 B | 48/48 ok |
| `put_WF` | `WRITE_NOTIFY`, 262,144 B inbound | 48/48 ok |
| `put_WFBIG` | `WRITE_NOTIFY`, 1,048,576 B inbound | 48/48 ok |
| `put_chain` | FLNK chain, `H` → `L1..L15` (see §16) | 48/48 ok |
| `put_fan` | `dfanout`, 8 targets | 48/48 ok |
| monitors | 6 per connection incl. a `DBR_CTRL_DOUBLE` one on `WF` | established |

Plus a 380 s hold at that load (`HOLD ok=192 fail=0`) so census passes
land while the workload is at its deepest.

### 15.2 The numbers

Verbatim, the last census pass (`tag=c6-54`), and identical in the three
passes before it:

```
STACKUSE tag=c6-54 id=0x00030047 name=CAS-client 0 size=2097152 current=1536 high=65912
STACKUSE tag=c6-54 id=0x00030069 name=CAS-event 0  size=1048576 current=1536 high=7120
STACKUSE tag=c6-54 id=0x000300ac name=CAS-client 1 size=2097152 current=1536 high=65912
STACKUSE tag=c6-54 id=0x000300b0 name=CAS-event 1  size=1048576 current=1536 high=7120
STACKUSE tag=c6-54 id=0x000300b9 name=CAS-client 2 size=2097152 current=1536 high=65912
STACKUSE tag=c6-54 id=0x000300bd name=CAS-event 2  size=1048576 current=1536 high=7120
STACKUSE tag=c6-54 id=0x000300c5 name=CAS-client 3 size=2097152 current=1536 high=65912
STACKUSE tag=c6-54 id=0x000300c9 name=CAS-event 3  size=1048576 current=1536 high=7120
STACKUSE tag=c6-54 id=0x0001001b name=cbMedium     size=2097152 current=1008 high=206800
STACKUSE tag=c6-54 id=0x00010034 name=scan-owner   size=1048576 current=2096 high=6664
STACKUSE tag=c6-54 id=0x0001003c name=CAS-TCP      size=1048576 current=2384 high=5568
```

* **`CAS-client` = 65,912 B**, all four threads, four census passes. This
  is **5.0×** the 13,240 B one-`ao` figure, so the old number was indeed
  not usable for the decision.
* **`CAS-event` = 7,120 B**, all four threads. 1.19× the old 5,968 B.
* Largest consumer in the process is not a CA thread at all: `cbMedium` at
  **206,800 B**.

**The high-water is invariant, and that is the load-bearing result.**
65,912 B did not move — not by one byte — across payloads of 8 B, 32,768 B,
65,536 B, 262,144 B and 1,048,576 B, across `DBR_DOUBLE` / `DBR_TIME_DOUBLE`
/ `DBR_CTRL_DOUBLE`, across `subArray` and `compress`, or with six monitors
per connection. Array payloads are on the heap, so growing them grows
`MEM_USED`, never the stack. It also did not move across the three
`StackSizeClass` arms of §10.1 — class changes reservation, never usage.

An earlier pass of this run appeared to show an asymmetry (`CAS-client 0`
at 65,912 B, clients 1–3 at 12,992 B). That was a census landing mid
round 1, before the other three connections had done the deep work; by
`c6-24` all four had converged. Single-instance was proved with `rtpShow`
(one RTP, 39 tasks) rather than assumed, because a second resident RTP
sharing port 5064 would have split the readings.

### 15.3 What the class costs, and what C actually does

Utilisation against each class, and against C:

| | bytes | `CAS-client` 65,912 B | `CAS-event` 7,120 B |
| --- | --- | --- | --- |
| `Small` | 524,288 | 12.57 %, 7.96× headroom | 1.36 %, 73.6× |
| `Medium` | 1,048,576 | 6.29 %, 15.9× headroom | 0.68 %, 147× |
| `Big` | 2,097,152 | **3.14 %, 31.8× headroom** | — |
| C-on-VxWorks `epicsThreadStackBig` | **22,000** | **300 % — would overflow** | 32.4 % |

C's two stack tables are not the same table, and this is the fact that
settles the parity question:

* POSIX (`libcom/src/osi/os/posix/osdThread.c:506-509`) —
  `STACK_SIZE(f) = f * 0x10000 * sizeof(void *)` over `{1, 2, 4}`, i.e.
  524,288 / 1,048,576 / 2,097,152 on 64-bit. **Byte-identical to our
  `StackSizeClass::bytes`.**
* VxWorks (`libcom/src/osi/os/vxWorks/osdThread.c:63-64`) —
  `{4000, 6000, 11000} * ARCH_STACK_FACTOR`, and x86_64 takes the `#else`
  giving `ARCH_STACK_FACTOR 2`: **8,000 / 12,000 / 22,000 B.**

So `rsrv`'s `epicsThreadStackBig` for `CAS-client`
(`caservertask.c:109-111`) is 22,000 B on this target, and our `Big` is
**95.3×** that. But our own measured need, 65,912 B, is **3.0× C's entire
`Big` allowance** — C's VxWorks number would overflow this port's
`CAS-client` outright. The gap is the port's own: `park_on` pins each
connection's async state machine onto the connection stack
(`doc/vxworks-port.md` §7), where C's `camsgtask` keeps almost nothing.
**"Match C's class" is therefore not available as a decision rule here;
only the measured byte count is.**

### 15.4 Judgement

Against the reservation model of §10.1 — each pool thread costs its
declared stack plus a flat ~1 MiB, against a ~248 MB reserved-address-space
ceiling — the class is the second-biggest lever on the wall, and the
measured walls were **47** (`Big`/`Medium`), **59** (`Medium`/`Medium`),
**80** (`Small`/`Small`).

**`Big` is not justified by anything measured.** It holds 2,031,240 B of
untouched reservation per connection for a 65,912 B worst case that proved
invariant under every dimension this round could vary, and it costs 2 MiB
of a ~5.2 MiB per-connection reservation on the target where reservation is
the binding resource. `Medium` keeps a 15.9× margin over the measured worst
case and takes the wall from 47 to 59 concurrent clients (+25.5 %) — and
that wall is measured for exactly that configuration (§10.1 arm B), not
predicted.

**The change is nevertheless NOT made, and the blocker is named rather than
worked around.** `client_roster` is shared with `armv7-rtems-eabihf`, and no
`CAS-client` stack high-water has ever been measured there — the RTEMS
measurement doc records priorities and retention, not stack use. Shipping
`Medium` off an `x86_64-wrs-vxworks` number would change an unmeasured
target, and on 32-bit RTEMS `Medium` is 524,288 B, half the byte count this
round validated. The one-line change is owed a second measurement, not a
second opinion; it is listed in §19. What *is* fixed now is the false
justification that would otherwise have settled this wrongly forever — the
roster's "`Big` is the parity answer" claim (commit `fbfd2847`).

`CAS-event` at 7,120 B of `Medium` is 0.68 %. `Small` would give it a 73.6×
margin and return a further 524,288 B per connection, but the
`Medium`-client/`Small`-event wall is unmeasured, so no wall number is
claimed for it.

## 16. `MAX_LINK_DEPTH = 16` silently truncates a legal FLNK chain

Found while establishing that the §15 high-water is depth-inclusive. The
`E8` set has a 32-deep chain `RTEMS:E8:H` → `L1..L32`; a CA put to `H`
processes `H` and `L1..L15` and stops. Measured with
`doc/vx-rig-e8/chainprobe.py`, reading every link rather than sampling:

```
[    9.3s] chain-1 H          = 1200.0
[    9.3s] chain-1 L1  depth=1  = 1201.0
...
[    9.3s] chain-1 L15 depth=15 = 1215.0
[    9.3s] chain-1 L16 depth=16 = 0.0
[    9.3s] chain-1 L17 depth=17 = 0.0
[    9.3s] chain-1 L18 depth=18 = 0.0
```

The cause is `process_entry_prelude`
(`crates/epics-base-rs/src/server/database/processing.rs:1253-1265`):

```rust
const MAX_LINK_DEPTH: usize = 16;
...
if depth >= MAX_LINK_DEPTH {
    eprintln!("link chain depth limit reached at record {name}");
    return Ok(None);
}
```

and the console confirms it fires, 436 times in one run:

```
link chain depth limit reached at record RTEMS:E8:L16
```

**This is a parity deviation.** C has no depth counter on the FLNK path:
`dbScanPassive` → `processTarget` (`db/dbDbLink.c:427-436`) guards only
against re-entering a record already in its own cycle (`psrc->pact = TRUE`),
which bounds *cycles*, not *depth*. C's `MAX_LOCK 10`
(`db/dbAccess.c:103,544-546`) is unrelated — it raises `SCAN_ALARM` after ten
attempts to process an already-active record. So a linear 32-deep FLNK
chain, which is legal and processes fully under C, stops halfway here.

Two aspects make it worse than a bare limit: the bail is `Ok(None)`, so
nothing is reported to the client and no record alarm is raised — the put
returns success — and the notice goes to `eprintln!`, which on this target
reaches only the console and not `errlog` (`doc/vxworks-port.md` §7), so a
production IOC would show no trace of it at all.

Not fixed here, and deliberately so: raising the bound trades a silent
truncation for unbounded recursion, and each level is a
`Pin<Box<dyn Future>>` (`processing.rs:615-626`) — a **heap** allocation per
link, on the target where §18 shows allocation failure at the wall killing a
thread. Depth is cheap in stack and expensive in heap here, so the bound is
entangled with the reservation budget and needs sign-off, not a constant
bump. Listed in §19.

The upside for §15: because the engine caps at 16, no database can drive the
recursion deeper, so 65,912 B covers the deepest chain this engine will ever
walk — the number is depth-inclusive by construction rather than by
choice of test data.

## 17. A monitored 1 MiB waveform kills the IOC at four clients

The first `WFBIG` run put `EVENT_ADD` on the 131,072-element array as well
as reading it. Round 1 completed on all four connections; then every
connection died at once. Verbatim:

```
memory allocation of 1048576 bytes failed
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
0xffff80000988a800 (CAS-client 3): RTP 0xffff800009444000 has been deleted due to signal 6.
memory allocation of 1048576 bytes failed
skipping backtrace printing to avoid potential recursion
```

`MEM_USED` had gone 43,278,336 → **211,804,160 B** on four connections, and
the failing allocation is exactly the array payload size. The whole process
is gone — not one thread, as in §18, but the RTP.

Re-running the identical workload with the `WFBIG` monitor removed and
everything else unchanged (`stackload.py … MONBIG=0`) survives a 480 s hold
with `HOLD ok=432 fail=0` and 80/80 on every op, including eighty 1 MiB gets
and eighty 1 MiB `WRITE_NOTIFY`s. So the reply path for a 1 MiB array is
fine at this guest size; it is the **monitor event queues** that exhaust the
heap — each subscriber's queue holds whole array copies, and four
subscribers on a 1 MiB array is enough.

This is the same family as the E10/E11 abort (`memory allocation of 64 bytes
failed` → signal 6 at 41 held connections) and it names the allocation:
a large-array monitor event buffer. C bounds this differently — a CA monitor
on a large array in C is bounded by `EPICS_CA_MAX_ARRAY_BYTES`/
`AUTO_ARRAY_BYTES` and refused with `ECA_TOLARGE` rather than aborting; see
`doc/`'s note that `EPICS_CA_MAX_ARRAY_BYTES` is not a ceiling in our
implementation. Not fixed here: the fix is a bounded per-subscriber event
queue with a documented overflow policy, which is `epics-base-rs` event-queue
work outside this row. Listed in §19.

## 18. The wall-abort mutex `EINVAL` is `semMCreate` returning NULL

§13 reproduced this and left the root cause open. It is now root-caused
statically *and* confirmed on target, and the failing allocation is named.

### 18.1 The static chain, from the SDK

Disassembling `pthreadLib.o` out of
`$SDK/vxsdk/sysroot/usr/lib/common/libc.a`: `std::sync::Mutex::new` stores
`PTHREAD_MUTEX_INITIALIZER`; the first `lock()` reaches
`pthread_mutex_lock+0x34`, which tests the magic `0xec542a37` and calls
`pthreadMutexInitComplete` → `pthreadMutexInit` → **`semMCreate`**. When
`semMCreate` returns NULL that path does a `semGive` and returns
`0x16` — **`EINVAL`, not `ENOMEM`** — which `InitComplete` passes through and
`pthread_mutex_lock` returns verbatim, so std panics with "invalid argument
(os error 22)".

`pthread_mutex_init` only *stamps* the magic and never calls `semMCreate`
itself, so **every** VxWorks pthread mutex materialises its semaphore on
first lock. Eager initialisation is not a workaround.

### 18.2 Confirmed on target

Built with `--wrap=semMCreate --wrap=pthread_mutex_lock`
(`doc/vx-rig-e8/build-e8.sh wrap`) and ramped to the wall on a cold RTP at
`1024M`. The interposers are allocation-free and lock-free by construction —
they run inside the failing path, so a `format!` would call the allocator
that is refusing and an `eprintln!` would lock the kind of object whose
creation just failed. Verbatim:

```
MTXPROBE semaphores_created=1
MTXPROBE semaphores_created=512
MTXPROBE semMCreate=NULL nth_null=1 succeeded_before=588 options=0x225
MTXPROBE lock rc=22 mutex=0xec90050 nth_fail=1 sem_ok=588 sem_null=1
MTXPROBE semaphores_created=1024
```

and the consequence:

```
ERROR epics_base_rs::errlog: sevr=fatal panic on thread `CAS-client 48` at
.../std/src/sys/pal/unix/sync/mutex.rs:69: failed to lock mutex: invalid
argument (os error 22) -- that thread is gone and nothing restarts it; the
IOC keeps listening and keeps answering searches, so from outside it still
looks healthy
```

The wall itself: 43 ramp + 5 monitor = **48** concurrent, refused with
`EAGAIN` (`REFUSED=2`), consistent with the 47 of §10.1's `Big`/`Medium`
arm.

### 18.3 What this tells the reservation budget

* **The failing allocation is a VxWorks semaphore object**, not mimalloc
  heap. That is why §17 and the E11 abort report a *byte* count from
  mimalloc while this one reports none: two different allocators fail at the
  same wall, and a budget that counts only heap bytes will not see this one.
* **Which mutex: whichever a freshly leased worker locks first.** There is no
  single guilty mutex — the census registry is exonerated (§13), and the
  identity `0xec90050` is simply the first `Mutex` that thread touched. Any
  `std::sync::Mutex` reached first on a new thread is the trigger.
* **The count is the budget number: 588 live semaphores**, at 49 leased sets
  / 98 workers / 48 connections. Every pooled worker pair therefore costs
  semaphores as well as stack reservation and a descriptor.
* **It is transient, not a hard ceiling.** Creations resumed and passed 1,024
  after the single NULL (`nth_null=1`, `nth_fail=1` for the whole run). So
  the trigger is a *burst* of thread creations outrunning reclamation, which
  means a budget bound has to throttle the creation rate, not only cap the
  total.
* **The `EAGAIN` spawn gate does not protect against it** — both fired in the
  same wall event.
* **A pool set leaks per occurrence**, confirming §14: `POOLPROBE BUSY=1
  SETS=49 CONNS=0` after every client left, i.e. one set held forever by the
  thread that panicked.

