# Measurement — do EPICS base's own thread priorities take effect on RTEMS 6?

**Answer: yes. C is not inert on this target.** Every EPICS thread in the C IOC
runs at its own distinct, live-readback scheduling level, and the ladder `rsrv`
builds in source is present on the target with the exact numbers the *POSIX* arm
of base predicts.

Taken **2026-07-22** on `coding-agent@192.168.2.128`, one boot of the upstream
C IOC under `qemu-system-arm -M xilinx-zynq-a9` (RTEMS 6.0.0 `2faafecb`, BSP
`xilinx_zynq_a9_qemu`, libbsd networking, 256 MB guest). This closes
[`../rtems-scope-b-session-handoff.md`](../rtems-scope-b-session-handoff.md)
§8.0 **gap 1** — "measure whether C's own thread priorities take effect on
RTEMS 6" — which was the missing cell in the C column of that section's table.

This is a measurement, not an upstream bug. It lives in this directory for the
same reason `evidence/FINDING-3-per-connection-heap.md` does: it is C-side
target evidence from the same box and the same `~/rtems-cside/` tree, which is
not under version control there.

Full unedited console transcript of the boot it came from:
[`evidence/c-thread-priority-boot-console-2026-07-22.log`](evidence/c-thread-priority-boot-console-2026-07-22.log)
(90,898 B, `sha256 b1dfd048…49e50`, verified identical on both ends). Every
quotation below is a byte range of that file; nothing is paraphrased.

---

## 1. What was measured, and on which image

| | |
|---|---|
| image | `~/rtems-cside/cioc-fd64.exe` — **stock** base descriptor configuration (64), i.e. **not** the fd=150 deviation |
| base | R7.0.10 `bf11a0c`, **zero source patches** (`evidence/DEVIATIONS.md`) |
| app | `cioc`, whose only base-facing piece is `ciocFsHook.c`, base's own `WEAK` `epicsRtemsMountLocalFilesystem()` hook |
| port | 5164 only (this panel's allocation); another panel's guest was on 5064/5075/15076 and was never touched |
| tools | `epicsThreadShowAll 1` (base, `libComRegister.c:523`) and `rt stackuse` / `rt top` (base's `rt` bridge to the RTEMS shell, `rtems_init.c:500`) |

Nothing was patched, added to, or removed from the target image to take these
numbers — both listings are commands upstream base and RTEMS already ship.
Declared in `~/rtems-cside/DEVIATIONS.md` "Session 3 additions", recovered here
as [`evidence/DEVIATIONS.md`](evidence/DEVIATIONS.md).

`epicsThreadShowAll` is the load-bearing instrument: on RTEMS-posix its
`epicsThreadShowInfo` (`libcom/src/osi/os/RTEMS-posix/osdThreadExtra.c:23-43`)
prints `OSIPRI` from base's own record **and `OSSPRI` from a live
`pthread_getschedparam(pthreadInfo->tid, …)`**. `OSSPRI` is therefore what the
kernel currently believes, not a copy of what was requested — which is exactly
the distinction "do the priorities take effect" asks about.

---

## 2. The listing, with four connections held

`epicsThreadShowAll 1` with four CA clients connected, each holding 25 monitors
on the 10 Hz record plus a ~50 Hz read loop, so the per-connection threads
exist and are running (transcript lines 1076-1105):

```
RTEMShost> epicsThreadShowAll 1
            NAME       EPICS ID   PTHREAD ID   OSIPRI  OSSPRI  STATE
          _main_       0x65e670            0      0       0       OK
          errlog       0x6637e0    184614915     10      26       OK
          taskwd       0x7f9160    184614916     10      26       OK
      timerQueue       0x7f9638    184614917     70     178       OK
           cbLow       0x7f97e0    184614918     59     150       OK
        cbMedium       0x7f99a0    184614919     64     162       OK
          cbHigh       0x7f9b60    184614920     71     180       OK
        dbCaLink       0x7f9d98    184614921     50     127       OK
        scanOnce       0xdfced8    184614922     67     170       OK
         scan-10       0xefd790    184614923     60     152       OK
          scan-5       0xffdf40    184614924     61     155       OK
          scan-2      0x10fe6f0    184614925     62     157       OK
          scan-1      0x11feea0    184614926     63     160       OK
        scan-0.5      0x12ff650    184614927     64     162       OK
        scan-0.2      0x13ffe00    184614928     65     165       OK
        scan-0.1      0x15005b0    184614929     66     167       OK
         CAS-TCP      0x16052c0    184614930     18      46       OK
         CAS-UDP      0x16053e8    184614931     16      41       OK
      CAS-beacon      0x1605cc8    184614932     17      44       OK
       CAS-event      0x1aa7858    184614934     19      49       OK
      CAS-client      0x1e2a598    184614944     20      51       OK
       CAS-event      0x1f2b050    184614938     19      49       OK
      CAS-client      0x1fab7e8    184614933     20      51       OK
       CAS-event      0x20ac2a0    184614943     19      49       OK
      CAS-client      0x212ca38    184614935     20      51       OK
       CAS-event      0x222d4f0    184614940     19      49       OK
      CAS-client      0x22adc88    184614914     20      51       OK
OSD priority range min: 1 max 254, memory not locked
```

Twenty-seven rows; twenty-six carry a live readback (`_main_` does not, see §4),
and they hold **eighteen distinct priority levels**: 26, 41, 44, 46, 49, 51,
127, 150, 152, 155, 157, 160, 162, 165, 167, 170, 178, 180. Only two values are
shared — `errlog`/`taskwd` at 26 and `cbMedium`/`scan-0.5` at 162, both because
their OSI priorities are equal. **The failure mode §8.0 warned about — "all IOC
threads equal at the baseline" — does not occur.**

The same block with **zero** connections is at transcript lines 234-255: the
same nineteen rows minus the eight per-connection threads (sixteen distinct
levels), same numbers. So the ladder is set at `iocInit`, not by anything the
load did.

---

## 3. Which map this image used — the POSIX arm, decided by measurement

§8.0's caution was that an RTEMS 6 build of base compiles the **POSIX** arm, not
the RTEMS-score arm (`configure/toolchain.c:31-36`: `__RTEMS_MAJOR__ >= 5` ⟹
`OS_API = posix`), and that the two arms produce different numbers — so the map
had to be established, not assumed.

It is established by the last line of the listing and by every row above it:

* `OSD priority range min: 1 max 254` is `pcommonAttr->minPriority` /
  `maxPriority` printed by `epicsThreadShowAll` (`os/posix/osdThread.c:1028-1031`).
  It is what `find_pri_range` actually probed on this guest for `SCHED_FIFO` —
  a POSIX-arm quantity that has no counterpart in the score arm.
* The POSIX arm's map is linear over that range
  (`epicsThreadGetPosixPriority`, `os/posix/osdThread.c:129-143`):
  `oss = (int)(osi × (max−min)/100 + min)` = `(int)(osi × 2.53 + 1)`.

Every measured row reproduces it exactly — **20 of 20, no residual**:

| thread | OSIPRI | OSSPRI measured | POSIX arm predicts | score arm would give |
|---|---|---|---|---|
| `errlog`, `taskwd` | 10 | 26 | 26 | 189 (core) |
| `dbCaLink` | 50 | 127 | 127 | 149 |
| `cbLow` | 59 | 150 | 150 | 140 |
| `scan-10` | 60 | 152 | 152 | 139 |
| `scan-5` | 61 | 155 | 155 | 138 |
| `scan-2` | 62 | 157 | 157 | 137 |
| `scan-1` | 63 | 160 | 160 | 136 |
| `scan-0.5`, `cbMedium` | 64 | 162 | 162 | 135 |
| `scan-0.2` | 65 | 165 | 165 | 134 |
| `scan-0.1` | 66 | 167 | 167 | 133 |
| `scanOnce` | 67 | 170 | 170 | 132 |
| `timerQueue` | 70 | 178 | 178 | 129 |
| `cbHigh` | 71 | 180 | 180 | 128 |
| **`CAS-UDP`** | **16** | **41** | 41 | 183 |
| **`CAS-beacon`** | **17** | **44** | 44 | 182 |
| **`CAS-TCP`** | **18** | **46** | 46 | 181 |
| **`CAS-event`** | **19** | **49** | 49 | 180 |
| **`CAS-client`** | **20** | **51** | 51 | 179 |

(The score-arm column is `199 − osi`, `os/RTEMS-score/osdThread.c:94-102`,
expressed as a core priority; it is shown only to make the two arms visibly
different. This image is unambiguously the POSIX arm.)

**Mechanism, confirmed rather than assumed.** The POSIX arm only sets a
priority at all inside `setSchedulingPolicy` (`osdThread.c:146-168`), which is
called from `epicsThreadCreateOpt` under `if (wantPrioScheduling)`
(`:614-617`), and which is also the single site that calls
`pthread_attr_setinheritsched(&attr, PTHREAD_EXPLICIT_SCHED)` (`:164-166`).
Distinct per-thread levels are observed, therefore that function ran, therefore
both `wantPrioScheduling` (from `EPICS_ALLOW_POSIX_THREAD_PRIORITY_SCHEDULING`,
default `YES`, `configure/CONFIG_ENV:57`) and `pcommonAttr->usePolicy` (from
`find_pri_range`'s `ok`, `:334`) were true on this target, and `EXPLICIT_SCHED`
defeated the inheritance §5.9 describes. The prediction in the brief holds.

`memory not locked` on the same line is `mlockall` state, not scheduling state;
it does not bear on any number here.

---

## 4. The RTEMS-side view, and the POSIX↔core relation

`rt top` prints each task's RTEMS priority (`RPRI`/`CPRI`). It cannot name
POSIX threads — it prints the numeric object name, the *same* `(0x16f051)` for
every one of them — so the ID→name join comes from `rt stackuse`, taken in the
same state minutes earlier in the same boot (transcript lines 301-347):

```
0x0b010012 CAS-TCP               0x01605e40 0x01685e2f 0x01685b78 524272   1040
0x0b010013 CAS-UDP               0x016864c8 0x017064b7 0x017061e8 524272    760
0x0b010014 CAS-beacon            0x01716f70 0x01756f5f 0x01756d90 262128   1712
0x0b010015 CAS-client            0x01fab908 0x020ab8f7 0x020ab648 1048560   2024
0x0b010016 CAS-event             0x01da9f20 0x01e29f0f 0x01e29e30 524272   2088
```

With four busy connections, `rt top` (transcript lines 908-941, first frame)
shows all four `CAS-client` and two of the four `CAS-event` threads:

```
 ID         | NAME                | RPRI | CPRI   | TIME                | TOTAL   | CURRENT
------------+---------------------+---------------+---------------------+---------+--^^----
 0x09010001 | IDLE                |  255 |  255   | 10m18.075757        |  94.716 |  86.537
 0x0a010002 | IRQS                |   96 |   96   | 17.796071           |   2.727 |   5.006
 0x0a010001 | TIME                |   98 |   98   | 9.089438            |   1.392 |   1.264
 0x0b010011 | (0x16f051)          |   88 |   88   | 1.401594            |   0.214 |   0.830
 0x0a01000a | _BSD                |  100 |  100   | 0.381581            |   0.058 |   0.048
 0x0b010010 | (0x16f051)          |   90 |   90   | 0.137549            |   0.021 |   0.025
 0x0a01000b | _BSD                |  100 |  100   | 0.052811            |   0.008 |   0.013
 0x0b01000e | (0x16f051)          |   95 |   95   | 0.050889            |   0.007 |   0.016
 0x0a01000c | _BSD                |  100 |  100   | 0.050556            |   0.007 |   0.011
 0x0b01000f | (0x16f051)          |   93 |   93   | 0.052378            |   0.008 |   0.007
 0x0a01000e | _BSD                |  100 |  100   | 0.023965            |   0.003 |   0.006
 0x0b01000d | (0x16f051)          |   98 |   98   | 0.011485            |   0.002 |   0.002
 0x0a010003 | _BSD                |  100 |  100   | 0.001054            |   0.000 |   0.000
 0x0a010004 | _BSD                |  100 |  100   | 0.004518            |   0.000 |   0.000
 0x0a010005 | _BSD                |  100 |  100   | 0.000019            |   0.000 |   0.000
 0x0a010006 | _BSD                |  100 |  100   | 0.000262            |   0.000 |   0.000
 0x0a010007 | _BSD                |  100 |  100   | 0.000020            |   0.000 |   0.000
 0x0a010008 | _BSD                |  100 |  100   | 0.000025            |   0.000 |   0.000
 0x0a010009 | _BSD                |  100 |  100   | 0.008634            |   0.001 |   0.000
 0x0a01000d | _BSD                |  100 |  100   | 0.000031            |   0.000 |   0.000
 0x0b010020 | (0x16f051)          |  204 |  204   | 0.387106            |   0.059 |   0.815
 0x0b010002 | (0x16f051)          |  204 |  204   | 0.380590            |   0.058 |   0.777
 0x0b010015 | (0x16f051)          |  204 |  204   | 0.375755            |   0.057 |   0.809
 0x0b010016 | (0x16f051)          |  206 |  206   | 0.287026            |   0.043 |   0.827
 0x0b010017 | (0x16f051)          |  204 |  204   | 0.372788            |   0.057 |   0.753
 0x0b01001c | (0x16f051)          |  206 |  206   | 0.287092            |   0.043 |   0.758
```

`top` orders by CPU and prints 25 rows, so the *idle* listener threads never
appear in it. A second pass drove them deliberately — 771 connect/close cycles
plus a UDP name-search flood — and both then print their own numbers
(transcript lines 1106-1139):

```
 0x0b010012 | (0x16f051)          |  209 |  209   | 2.749477            |   0.360 |   9.741
 0x0b010013 | (0x16f051)          |  214 |  214   | 3.555741            |   0.466 |  13.945
 0x0a01000f | DHCP                |  254 |  254   | 0.280300            |   0.036 |   0.000
 0x0b010001 | (0x176119)          |  254 |  254   | 2.279776            |   0.299 |   0.000
 0x0b010003 | (0x16f051)          |  229 |  229   | 0.014008            |   0.001 |   0.000
 0x0b010004 | (0x16f051)          |  229 |  229   | 0.010606            |   0.001 |   0.000
```

`0x0b010012` is `CAS-TCP`, `0x0b010013` is `CAS-UDP`, `0x0b010001` is `_main_`,
`0x0b010003`/`0x0b010004` are `errlog`/`taskwd`.

**`rpri = 255 − osspri` on every thread where both were read** — 0x0b010012
(46/209), 0x0b010013 (41/214), `CAS-client` (51/204), `CAS-event` (49/206),
`errlog`/`taskwd` (26/229), `timerQueue` (178/77), `scan-5` (155/100), `scan-2`
(157/98), `scan-1` (160/95), `scan-0.5` (162/93), `scan-0.2` (165/90),
`scan-0.1` (167/88). Fourteen threads, no exception. So the two instruments are
two views of one number, and neither is a cached copy of a request.

`CAS-beacon` is the one CA server thread whose RTEMS-side number was **not**
read directly — it is too idle to enter `top`'s 25 rows. Its POSIX priority 44
*was* read directly; core 211 is the relation applied, not an observation.

**`_main_` at RPRI 254** is base's init task, and it confirms the second half of
§5.9's mechanism story from the other side: base lowers `POSIX_Init` to
`RTEMS_MAXIMUM_PRIORITY - 1` (`libcom/RTEMS/posix/rtems_init.c:1038`) and never
raises it, so the thread that runs `main()` and then `iocsh` sits one level
above idle while every thread it created sits far above it. Note that
`epicsThreadShowAll` reports `_main_` as `OSSPRI 0`: its `epicsThreadOSD` has
`tid == 0`, so `osdThreadExtra.c:33` skips the `pthread_getschedparam` call
entirely. **Base's own listing under-reports the main thread; only the RTEMS
listing shows it.**

---

## 5. `rsrv`'s ladder, source vs target

`rsrv` builds one descending ladder from `epicsThreadPriorityCAServerLow` (=20)
by repeated `epicsThreadHighestPriorityLevelBelow` (`caservertask.c:562-575`),
then takes `threadPrios[2]` for `CAS-TCP` (`:716`) and `threadPrios[4]` for
`CAS-UDP` (`:722`); `CAS-client` is created at `epicsThreadPriorityCAServerLow`
itself (`:109`) and `CAS-event` one level below it (`:1508-1515`).

On the POSIX arm `epicsThreadHighestPriorityLevelBelow` subtracts one and
subtracts *nothing further* when `max−min ≥ 100` (`osdThread.c:866-881`); here
`max−min = 253`, so the OSI ladder is the plain `20, 19, 18, 17, 16`. Measured
against the target:

| rsrv thread | source | OSI | POSIX (measured) | RTEMS core |
|---|---|---|---|---|
| `CAS-client` (TCP receiver, per connection) | `caservertask.c:109` | 20 | **51** | **204** |
| `CAS-event` (TCP sender, per connection) | `caservertask.c:1508-1515` | 19 | **49** | **206** |
| `CAS-TCP` (listener) | `threadPrios[2]`, `:716` | 18 | **46** | **209** |
| `CAS-beacon` | `threadPrios[3]`, `:758` | 17 | **44** | 211 (derived) |
| `CAS-UDP` (name receiver) | `threadPrios[4]`, `:722` | 16 | **41** | **214** |

The ladder in the source is the ladder on the target, in the right order, with
no collapsing and no clamping. The per-connection receiver is the most urgent CA
thread and the name receiver the least, exactly as the comment at
`caservertask.c:552-560` says.

---

## 6. Where libbsd sits relative to `CAS-TCP` — recorded, as asked

Read on the **same guest, same boot, same `top` frames** as the CA numbers:

| task | RTEMS core priority | relative to `CAS-TCP` (209) |
|---|---|---|
| `IRQS` (libbsd interrupt server) | **96** | 113 levels more urgent |
| `TIME` (libbsd timer) | **98** | 111 more urgent |
| every `_BSD` worker — `config_0`, `swi6: task queue`, `swi5: fast task`, `thread taskq`, `swi6: Giant task`, `kqueue_ctx task`, `swi1: netisr 0`, `bufdaemon`, `vnlru`, `syncer`, `softirq_0`, `bufspacedaemon` | **100** | **109 levels more urgent** |
| `DHCP` | 254 | 45 less urgent |
| `IDLE` | 255 | 46 less urgent |

**The entire libbsd network stack outranks every CA server thread in the C IOC**
— by a wide margin, and by construction rather than by tuning, since 96/98/100
are libbsd's own defaults and 204..214 are what base's linear map produces from
the CA server band.

The same is not true of base's *scan and callback* threads. With the linear map,
seven of them land above `_BSD`'s 100, and one lands above the interrupt server:

| base thread | OSI | POSIX | core | vs libbsd |
|---|---|---|---|---|
| `cbHigh` | 71 | 180 | **75** | above everything in libbsd |
| `timerQueue` | 70 | 178 | **77** | above everything |
| `scanOnce` | 67 | 170 | **85** | above everything |
| `scan-0.1` | 66 | 167 | **88** | above everything |
| `scan-0.2` | 65 | 165 | **90** | above everything |
| `scan-0.5` | 64 | 162 | **93** | above everything |
| `scan-1` | 63 | 160 | **95** | **above `IRQS` (96)** |
| `scan-2` | 62 | 157 | 98 | ties `TIME`, above `_BSD` |
| `scan-5` | 61 | 155 | 100 | ties `_BSD` |
| `cbLow` | 59 | 150 | 105 | below |

This is the *measured* form of a hazard this repository had so far only derived
from source: the doc comment on `map_epics_priority_rtems`
(`crates/epics-base-rs/src/runtime/task.rs:936-941`) states that base's linear
map puts EPICS 91 at core 24 and names EPICS **63** as the crossover with the
interrupt server. EPICS 63 is `scan-1`, and it is on the target at core 95,
one level above `IRQS` at 96 — the predicted crossover, at the predicted place.
Nothing in this measurement contradicts that comment; it confirms it.

---

## 7. What this gives §8.0

**Gap 1 is closed with a number, and the "C is inert too" branch is dead.**
"Reach C's blocking-thread level" now has a target:

| property | C — RSRV, **measured this boot** | us, predicted from `map_epics_priority_rtems` (unmeasured by this panel) |
|---|---|---|
| priorities distinct per role | yes, 18 live levels over 26 threads | yes by construction |
| ladder order `client > event > TCP > beacon > UDP` | yes | yes (76/75/74/73/72 posix) |
| `CAS-client` | posix 51 / core 204 | posix 76 / core 179 |
| `CAS-TCP` | posix 46 / core 209 | posix 74 / core 181 |
| `CAS-UDP` | posix 41 / core 214 | posix 72 / core 183 |
| below the libbsd band | yes, by 109 levels | yes, by 81 levels |
| scan/callback band above libbsd | **yes — 7 threads, one above `IRQS`** | **no — the map's image is core `100..=199`, so it cannot happen** |
| init task / `main` | core 254 | core 254 (same lowering) |

Two consequences worth stating plainly, neither of which this panel should act
on alone:

1. **The CA ladder is a match in shape and order, and a deliberate offset in
   absolute value.** Ours is ~28 levels more urgent than C's for the same role,
   because the two maps have different shapes. §5.9 already records that offset
   as a deliberate deviation. Nothing here argues for changing it — the
   *relative* ordering, which is what the ladder is for, is identical.
2. **On the scan/callback band the two are not equivalent, and ours is the safer
   one.** C on this target lets a 1 Hz periodic scan preempt the network
   interrupt server. This is not a bug report against base — it is the
   consequence of a range-scaled map meeting a BSP whose network threads sit at
   96-100 — but it is a real behavioural difference between the two IOCs, and it
   belongs in the comparison rather than only in a code comment.

**Gap 2 (measure ours, same reading) is not closed by this document.** No Rust
image was booted by this panel. The two sides were also *not* read in the same
guest instance: this reading is from the C guest (`cioc-fd64.exe`, qemu pid
2061756, ports 5164), while the Rust reading is being taken on a separate guest
on the same box. They share the BSP, RTEMS build, libbsd build and qemu
invocation; they are not the same boot, and any claim that needs one boot — for
example a direct CPU-share comparison — is not supported by this material.

---

## 8. One observation that is not a controlled experiment

The first attempt to set up the four connections drove one connection at full
rate with no throttle. With that single connection saturating the guest, the
*next* `connect()` completed but its CA version reply never arrived within the
10 s socket timeout, twice in a row (host-side driver output, not console
output, so it is not in the transcript):

```
  connecting 1
  connecting 2
Traceback (most recent call last):
  File "/home/coding-agent/rtems-cside/hold3.py", line 45, in <module>
    s.sendall(HELLO); s.recv(16)
                      ^^^^^^^^^^
TimeoutError: timed out
```

A running `CAS-client` at core 204 outranks `CAS-TCP` at 209, and the guest has
one core, so the accept path being starved by an active connection is the
behaviour the measured ladder predicts. That is consistent, not proven: no
control was run with priorities flattened, and a plain CPU shortage would look
similar from outside. It is recorded because it is the only *dynamic* evidence
taken, and because the same driver at a 20 ms throttle brought four connections
up every time (`repro/priority/hold5.py`, `hold6.py`).

---

## 9. Limits of this measurement

* **One boot, one image, one BSP.** No repetition across boots, no second BSP,
  no SMP configuration (this guest is single-core).
* **`CAS-beacon`'s RTEMS-side number is derived**, not read (§4).
* **No Rust-side reading was taken here**, and the two guests are separate
  instances (§7).
* **`_main_`'s POSIX priority was never read** — base's listing cannot show it
  and `top` shows only the RTEMS side (§4). Its RTEMS core 254 is direct.
* **Nothing about lock wait discipline or priority inheritance was measured.**
  §8.0 gaps 3 and 4 are untouched by this document. Priorities being live on
  threads says nothing about what happens to a thread blocked on a mutex.
* The `top` frames were taken while the driver was loading the IOC; the CPU
  columns in them are of that load and should not be read as an idle profile.

---

## 10. Reproducing it

On the box, from `~/rtems-cside/`:

```sh
./boot-cioc.sh ~/rtems-cside/cioc-fd64.exe   # ~45 s to the iocsh prompt
python3 hold5.py                             # 4 busy connections; stackuse, top, showall
python3 hold6.py                             # accept + UDP churn; top with CAS-TCP/CAS-UDP
tail -f cioc.log                             # console; iocsh input goes to the ciocin fifo
```

The drivers are copied into this repository at
[`repro/priority/`](repro/priority) — `hold.py` (first listing, no load),
`hold2.py` (second pass: 4 connections, `rt top` with `A` and `+` so the idle
CAS-* rows are not truncated), `hold3.py` (the unthrottled one from §8),
`hold4.py` (the workaround for hold3's starve: all 4 connections established
before any load is applied), `hold5.py`, `hold6.py`. They are host-side
raw-CA-socket drivers using the same connect method as `ceiling.py`; nothing
in them is compiled into or linked against the target image.

`rt top` is interactive and refreshes until a key arrives; the drivers exit it
by writing a bare newline into the console fifo. It also clears the screen with
ANSI escapes each frame, which is why the transcript contains `ESC[H ESC[J`
sequences — they are in the byte stream the target printed and were left in.

### Checksums (verified on both ends)

SHA-256, computed under `~/rtems-cside/` on `192.168.2.128` and again on the
copy in this repository — and, for the transcript, against the git blob itself
(`git cat-file -p HEAD:<path> | sha256sum`), not only the working copy.

The RTEMS serial console emits `CR LF`, and this repository's root
`.gitattributes` sets `* text=auto eol=lf`, which strips every `CR`. The first
commit of this material did exactly that: the transcript went in as
`d4277d03…f9bd71` and the checksum below did **not** verify against it. It is
fixed by `evidence/.gitattributes` (`*.log -text`), so the log is now stored
byte-for-byte as the target printed it. If a future edit of the attributes file
re-normalizes it, the row below stops matching — that is the intended alarm, not
a stale checksum.

| file in this directory | source on the box | sha256 |
|---|---|---|
| `evidence/c-thread-priority-boot-console-2026-07-22.log` | `cioc.log` (also kept as `log/cioc-priority-2026-07-22.log`) | `b1dfd048fbb66908002cb3b45dc186170aef632739701fb3c3efa95b7e949e50` |
| `repro/priority/hold.py` | `hold.py` | `b1c4c95241c6fe98e60cba161fd021da7ec951e8feb7509588282c595c4575cc` |
| `repro/priority/hold2.py` | `hold2.py` | `f0528844deb487f035f698bc5792ff21f77d0175d8d9cc7a2ddc33df53c0b690` |
| `repro/priority/hold3.py` | `hold3.py` | `e517aabaa707d128d40742df27d26656d1e652c0fbbf77c7a3162ebee04ac5ab` |
| `repro/priority/hold4.py` | `hold4.py` | `658f32d12d4323465d9c4db5e347a32f295ee2714615ce360bebd4952f115154` |
| `repro/priority/hold5.py` | `hold5.py` | `bd932c91d3b85e33692143175bbad46dc64293c36d68cae26cd6a5a7a0b35aed` |
| `repro/priority/hold6.py` | `hold6.py` | `fede38cb191eac8bdd67b257f4a655c45e0dff79df742a0370f564a489bed575` |
| `evidence/DEVIATIONS.md` (now includes "Session 3 additions") | `DEVIATIONS.md` | `bcd3a3ede844cda8ff8296d6e424e57b3f804d4d71f463340328f8769ab00b44` |

`evidence/DEVIATIONS.md` replaces the copy recovered earlier the same day
(`8c3d8109…fbb440`); the earlier content is unchanged and the session-3 section
is appended after it.
