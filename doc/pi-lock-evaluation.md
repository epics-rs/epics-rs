# PI-lock evaluation — roadmap 4b, prerequisite for any hard-RT claim

Read-only investigation. No files edited. Every claim carries file:line.

## Provenance — and a mid-investigation tree move

* **Worktree:** `/home/stevek/work/epics-rs/.caucus/worktrees/manual-ca-sans-io`
  @ **`051b7851`** (`test(ca): gate the C-interop suites out of the
  rtems-exec-model build`). HEAD was `051b7851` when I started and is still
  `051b7851` now.
* **But the working tree is NOT clean, and it changed under me.** `git status`
  shows three files with uncommitted edits from the concurrent panel:
  `runtime/task.rs` (+485/-…), `runtime/background/delayed_timer.rs` (+11),
  `epics-ca-rs/src/server/blocking.rs` (+18), plus one untracked new file
  `epics-base-rs/tests/rt_priority_default_off.rs`. Those edits landed
  **between** my first read of those files and my verification pass. Two of them implement
  what my first draft was about to recommend. Everything below is re-verified
  against the **current working tree**, and the affected rows are marked
  ⚠ **moved**.
* **C reference:** `/home/stevek/work/epics-base` @ `669a25697`. Present, so the
  reference-source rule is satisfied — no STOP.

---

## §0 Headline — four findings

**1. Real-time priority is opt-in and off by default, so today no priority
inversion is possible at all — by construction.**
⚠ **moved.** `RtPolicy` (`task.rs:469-505`) resolves
`EPICS_RS_ALLOW_RT_PRIORITY` (`task.rs:459`) once per process; absent or unset
⟹ `RtPolicy::Disabled` (`task.rs:480`). `apply_to_current_thread_under`
short-circuits on it: `RtPolicy::Disabled => PriorityApplied::Disabled`
(`task.rs:547`) — **no scheduler call is made** (`task.rs:529-530`). With the
switch off every thread in the process is SCHED_OTHER at the same nominal
priority, and "priority inversion" is not a well-defined condition.

This reframes roadmap 4b: it is not a live-bug hunt. It is *pre-work for the
configuration that does not ship yet* — the state of the tree when
`EPICS_RS_ALLOW_RT_PRIORITY=1` becomes the supported RT mode. Everything below
should be read as "what breaks when the switch is turned on", not "what is
broken now".

**2. The PI mutex already exists and is used by nothing.**
`runtime::sync::PriorityInheritanceMutex<T>` (`sync.rs:27`) is a real
`pthread_mutex_t` + `PTHREAD_PRIO_INHERIT` (`sync.rs:47-83`) behind the
`linux-rt` feature; `sync.rs:33` is the `parking_lot::Mutex` fallback and
`is_pi_mutex_active()` (`sync.rs:36`) the diagnostic. Workspace-wide `rg` for
`PriorityInheritanceMutex` outside `sync.rs` returns **two hits, both prose** —
a comment at `epics-base-rs/Cargo.toml:94` and `docs/epics_base_missing_features.md:238`
(which marks the item ✅ DONE). **Zero production call sites**, and
`linux-rt = []` (`Cargo.toml:98`) is enabled by no target, profile or default.
So 4b is not "add PI mutexes"; it is "decide which existing locks should have
been one, and wire it".

**3. The most safety-critical lock — the `dbScanLock` analogue — is a tokio
async mutex, which no PI mechanism can ever cover.**
`RecordLockRegistry` holds `Arc<Mutex<()>>` per record where `Mutex` is
`tokio::sync::Mutex` (`record_lock.rs:70`, field at `:83`, `gate_for` at `:93`,
`lock_record` at `:140`). Its own doc calls it the direct analogue of
`dbCommon::lock`/`dbScanLock` (`record_lock.rs:8`, `:42`). In C that lock is
`epicsMutexMustCreate()` (`dbLock.c:86`) ⟹ `PTHREAD_PRIO_INHERIT`
(`posix/osdMutex.c:71-85`) / `RTEMS_INHERIT_PRIORITY`
(`RTEMS-score/osdMutex.c:72`). A blocking thread waiting on it in Rust sits in
`std::thread::park()` (`task.rs:80`) holding no kernel-visible lock — see §3.

**4. The thread-priority asymmetry I found has just been closed for the CA
server and the timer — but not for periodic scan or iocsh.**
⚠ **moved.** `apply_to_current_thread` now has **six** non-test call sites:
callback bands (`callback_executor.rs:301`), scanOnce (`scan_once.rs:181`),
`spawn_blocking_with_priority` (`task.rs:755`), and — new in the uncommitted
diff — `cbTimer` → `ScanHigh` (`delayed_timer.rs:234`), `CAS-client-blocking`
→ `CaServerLow` (`blocking.rs:200`), `CAS-event-blocking` →
`Custom(CaServerLow-1)` (`blocking.rs:669-671`). Still **none**: periodic scan
tasks (`scan_event.rs:87-92`), both iocsh threads (`ioc_app.rs:695`, `:1030`),
the `CAS-UDP` thread (`bin/rtems-ca-ioc.rs:159`), and all hosted tokio workers.

---

## §1 Thread inventory: who runs at what priority

Priorities apply **only** when `EPICS_RS_ALLOW_RT_PRIORITY` is on (finding 1).

| thread | created at | EPICS priority | applied? |
|---|---|---|---|
| `cbLow` | `callback_executor.rs:293`,`:301` | `ScanLow-1` = 59 (`:97-103`) | yes |
| `cbMedium` | same | `ScanLow+4` = 64 | yes |
| `cbHigh` | same | `ScanHigh+1` = 71 | yes |
| `scanOnce` | `scan_once.rs:172`,`:181` | `ScanLow` = 60 | yes |
| `cbTimer` | `delayed_timer.rs:225`,`:234` | `ScanHigh` = 70 | ⚠ **yes, new** |
| `CAS-client-blocking <peer>` | `blocking.rs:192`,`:200` | `CaServerLow` = 20 (`task.rs:370`) | ⚠ **yes, new** |
| `CAS-event-blocking <peer>` | `blocking.rs:659`,`:669` | `CaServerLow-1` = 19 | ⚠ **yes, new** |
| `CAS-UDP` | `bin/rtems-ca-ioc.rs:159` | — | **no** |
| periodic scan | `scan_event.rs:87`,`:92` (`tokio::task::JoinSet`) | — | **no** |
| `iocsh-startup` / `iocsh-after-ioc-running` | `ioc_app.rs:695`, `:1030` | — | **no** |
| tokio workers (hosted) | tokio runtime | — | **no** |

Two structural notes:

* The band mapping is C-faithful (`callback_executor.rs:98` cites
  `epicsThread.h:84`), and the new CA values cite `caservertask.c:109` and
  `:560`/`:1508`. So the *intended* ordering is now CA server (19/20) **below**
  scan/callback (59-71), matching C.
* **On the RTEMS/exec backend the classes collapse anyway.**
  `runtime::task::spawn` routes every async tail to `spawn_future(…,
  DEFAULT_SPAWN_PRIORITY, …)` and `DEFAULT_SPAWN_PRIORITY =
  CallbackPriority::Medium` (`future_exec.rs:105`) — so CA async tails and
  record-processing callbacks share one cbMedium worker there, regardless of the
  `CaServerLow` values above. And on RTEMS `apply_priority_impl` is the
  `#[cfg(not(target_os = "linux"))]` arm returning `PriorityApplied::Unsupported`
  (`task.rs:715-719`), so **no priority is applied on the actual RTEMS target at
  all**. The band structure is advisory-only there.

---

## §2 (a)+(b) The shared-lock table

"Shared" = acquired by at least one thread from the higher-priority set
{cb bands, scanOnce, cbTimer} and at least one from the lower set {CA threads,
iocsh, periodic scan, tokio workers}.

**Kind:** `PL` = `parking_lot`, `STD` = `std::sync` — both kernel-visible
futexes, PI-able by type replacement. `TOK` = `tokio::sync` — awaited; reached
from a blocking thread only via `park_on`, **invisible to the kernel PI chain**
(question (c), §3).

| # | lock | decl / evidence | Kind | class | C counterpart |
|---|---|---|---|---|---|
| **L1** | per-record advisory write gate `Arc<Mutex<()>>` | `record_lock.rs:70`,`:83`,`:110`,`:126`; taken by `lock_record` `:140` / `lock_records` `:171` | **TOK** | **(c) park_on — PI-invisible** §3 | `dbCommon::lock`, `epicsMutexMustCreate` `dbLock.c:86`, PI on |
| **L2** | `RecordLockRegistry::gates` | `record_lock.rs:83` `std::sync::Mutex<HashMap<..>>`, entered in `gate_for` `:93` | **STD** | **replace-with-PI** — cheapest win | `lockSetsGuard`, `dbLock.c:48`,`:62` |
| **L3** | per-record data `Arc<parking_lot::RwLock<RecordInstance>>` | `database/mod.rs:107`,`:230`,`:591`,`:1368`,`:1682`,`:1696`; 96 acquisitions in `database/processing.rs`, 42 in `links.rs`, 38 in `field_io.rs`, 65 in `database/mod.rs` | **PL** | **replace-with-PI** | the same `dbCommon::lock` (C has one lock; the port has two) |
| **L4** | `PvDatabase::records` registry | `database/mod.rs:230` `parking_lot::RwLock<HashMap<..>>` | **PL** | **accept-with-bounded-CS** (§4) | `dbBase` GPHENTRY lookup, no per-op mutex in C |
| **L5** | `PvDatabase::aliases` | `database/mod.rs:270`,`:597`; read via `resolve_alias` inside `lock_record` (`record_lock.rs:142`) | **PL** | **accept-with-bounded-CS** | — |
| **L6** | `ProcessVariable::value` | `pv.rs:322`,`:370` `parking_lot::RwLock<EpicsValue>` | **PL** | **replace-with-PI** | record field storage under `dbCommon::lock` |
| **L7** | `ProcessVariable::subscribers` | `pv.rs:323`,`:371` — `crate::runtime::sync::Mutex` (`pv.rs:4`) = tokio (`sync.rs:3`); CA takes it as `block_on_sync(pv.subscribers.lock())` (`blocking.rs:2008`), also `:852`, `:970` | **TOK** | **(c) park_on — PI-invisible** §3 | monitor list under the record lock |
| **L8** | 11 `PvDatabase` fields: `simple_pvs` `:229`, `scan_index` `:244`, `load_order` `:250`, `cp_links` `:256`, `external_cp_links` `:265`, `external_resolver` `:297`, `search_resolver` `:299`, `existence_gate` `:303`, `link_sets` `:308`, `subroutine_registry` `:333`, `breaktable_registry` `:340` — all bare `RwLock` = tokio (`database/mod.rs:19`) | **TOK** ×11 | **(c) PI-invisible**; 6 are **remove-the-sharing** (§4) | `dbScan.c:122`,`:139` `event_lock`/`ioscan_lock` are epicsMutex, PI on |
| **L9** | ACF config cell `Arc<tokio::sync::RwLock<Option<AccessSecurityConfig>>>` | `access_security.rs:169`,`:185`,`:201`; CA's `SharedAcf` alias `blocking.rs:132`,`:142`,`:153`,`:617` | **TOK** | **(c) PI-invisible** + **accept** (read-mostly) | `asLock` (epicsMutex family) |
| **L10** | `TRAP_WRITE_REGISTRY` | `access_security.rs:1000-1005` `OnceLock<std::sync::RwLock<Vec<..>>>`; read at `:993`,`:1025`,`:1055` | **STD** | **accept-with-bounded-CS** | `asTrapWriteListener` list |
| **L11** | CA event ring `EvQue::inner`, `ques` | `event_queue.rs:82`,`:258`,`:458`,`:472` `std::sync::Mutex` | **STD** | **replace-with-PI** | ring buffer under `client->eventLock` |
| **L12** | callback band queue `PriorityQueue::state` + `wake` | `callback_executor.rs:32`,`:137`,`:139`,`:146`,`:152` `std::sync::Mutex`/`Condvar` | **STD** | **replace-with-PI** — highest leverage | `callbackQueue[].lock` (`epicsRingBytes` + `epicsEventId`) |
| **L13** | `RecordInstance::metadata_cache`, async-completion `tx` | `record_instance.rs:61`,`:729` `StdMutex` | **STD** | **accept-with-bounded-CS** | — |
| **L14** | autosave per-set gate | `autosave/manager.rs:67`,`:82`,`:309`,`:317` `Arc<tokio::sync::Mutex<()>>` | **TOK** | **remove-the-sharing** (§4) | `save_restore` task-local |
| **L15** | `IfaceMap` cache | `net/iface_map.rs:15`,`:51`,`:63` `parking_lot::Mutex` | **PL** | **not shared** — CA/PVA UDP only, no high-prio acquirer; listed for completeness | `osiSockDiscoverBroadcastAddresses` |

Totals: **15 rows / 25 distinct locks** (L8 is 11). **14 are `tokio::sync`**
(L1, L7, L8×11, L9, L14), **5 `std::sync`** (L2, L10, L11, L12, L13),
**5 `parking_lot`** (L3, L4, L5, L6, L15).

---

## §3 (c) The park_on set — why PI mutexes cannot reach them

`block_on_sync` (`task.rs:112`) picks one of three mechanisms: `Err(NotBlockable)`
on a current-thread runtime (`:115`), `tokio::task::block_in_place` on a
multi-thread worker (`:116`), and otherwise `park_on`. On a plain `std::thread`
with no runtime entered — *every* thread in the blocking CA driver, both iocsh
threads, `CAS-UDP` — it takes the `park_on` arm, which polls and calls
**`std::thread::park()`** (`task.rs:66`, `:80`).

The consequence, stated precisely:

> A thread blocked in `std::thread::park()` waiting for a `tokio::sync::Mutex`
> is, to the kernel, a thread sleeping on a futex owned by **nobody**. The
> kernel has no record that it is waiting for a resource another thread holds.
> Priority inheritance is a kernel mechanism that walks owner pointers; there is
> no owner pointer. **Adding `PriorityInheritanceMutex` anywhere in the
> workspace changes nothing for L1, L7, L8, L9 or L14 — 14 of the 25 locks.**

Two further properties, both worse than the C behaviour:

* **Wake order is FIFO, not priority.** `tokio::sync::Mutex` is explicitly fair:
  waiters are served in arrival order. C's `epicsMutex` on RTEMS is created
  `RTEMS_PRIORITY|RTEMS_BINARY_SEMAPHORE|RTEMS_INHERIT_PRIORITY|…`
  (`RTEMS-score/osdMutex.c:72`) — `RTEMS_PRIORITY` makes the wait queue
  **priority-ordered**. So even setting inheritance aside, a cbHigh waiter on L1
  queues behind however many CA client threads arrived first.
* **`OwnedMutexGuard` means L1 holders *can* await while holding.**
  `lock_record` returns a `'static` `OwnedMutexGuard` (`record_lock.rs:110`,
  `:121`, `:126`) precisely so the guard can be held across await points. That is
  what makes L1 unconvertible to a blocking lock without a design change — it is
  not an oversight, it is the current contract.

**The (c) class is the dominant one and its fix is not a lock swap.** It is
either (i) move the state onto a blocking lock — sound only if no holder ever
awaits while holding, which L1 currently violates by design — or (ii) remove the
sharing. L1 must be answered before any hard-RT claim; §6 orders it first.

---

## §4 (b) Rationale for the non-obvious classifications

**replace-with-PI (L2, L3, L6, L11, L12).** All five are `parking_lot`/`std`
locks whose critical sections contain no await — guaranteed by type, since
`parking_lot::RwLockWriteGuard` and `std::sync::MutexGuard` are `!Send` and a
holder cannot cross an await point and still compile. Each is genuinely
contended between a high- and a low-priority thread. These are exactly the rows
`PriorityInheritanceMutex` was written for. **L12 is the highest-value of the
five**: every callback submission from every thread in the process passes
through it (`callback_executor.rs:137`), so it is the one lock where a
SCHED_OTHER submitter can delay a SCHED_FIFO-71 band worker's own dequeue.

**accept-with-bounded-CS (L4, L5, L10, L13).** Bounds, stated rather than
asserted:

* **L4** — the bound rests on a lock discipline the code already documents and
  I verified at all 10 map-read sites in `database/processing.rs`:
  *"Never hold `records.read()` across `rec.write()`"* (`processing.rs:681-683`).
  Seven sites are collect-then-act — take the map read in a scope block, clone
  the `Arc`, drop it (`:685-687`, `:704-706`, `:981-983`, `:1092-1094`,
  `:1270-1272`, `:4427-4430`, `:4920-4922`). The remaining three hold the map
  read across a per-record **read**, never a write (`:802-804`, `:819-822`,
  `:869-872`). Lock ordering is uniformly records→instance. Bound: one hash
  probe plus an `Arc` clone; no allocation beyond the clone, no I/O, no nested
  write.
* **L5** — same shape, one alias hash probe (`record_lock.rs:142`).
* **L10** — iteration over registered trap listeners, typically 0 or 1
  (`access_security.rs:1000-1005`).
* **L13** — one `Option` take/replace (`record_instance.rs:61`, `:729`).

These stay as they are **provided** the write-set stays init-time. The
invalidating change is dynamic record creation after `iocInit`; that condition
should be named in the code rather than left implicit.

**remove-the-sharing (L8 subset, L14).** L14's autosave gates are per-save-set
and touched by the RT band only because save-on-change is driven from record
processing; routing that through the existing callback queue (a submission, not
a lock acquisition) removes the contention entirely. Within **L8**,
`external_resolver` (`:297`), `search_resolver` (`:299`), `existence_gate`
(`:303`), `subroutine_registry` (`:333`), `breaktable_registry` (`:340`) and
`load_order` (`:250`) are written at IOC init and read-only afterwards; the
structural fix is `ArcSwap`/`OnceLock`, which **removes six of the eleven L8
locks from the table** rather than reclassifying them.

---

## §5 (d) C parity — what is an `epicsMutex` in C base

Confirmed against `/home/stevek/work/epics-base` @ `669a25697`:

* POSIX: `epicsMutex` sets `PTHREAD_PRIO_INHERIT` on both the default and
  recursive attribute (`modules/libcom/src/osi/os/posix/osdMutex.c:71-75`), with
  a **runtime probe** — it constructs a temporary mutex and on failure falls
  back to `PTHREAD_PRIO_NONE` (`:77-86`). `epicsMutexShowAll` reports
  "PI is/is not enabled" (`:199-205`). `DONT_USE_POSIX_THREAD_PRIORITY_SCHEDULING`
  `#undef`s the whole thing (`:50-52`) — so PI is build-configurable in C too,
  not an unconditional guarantee.
* RTEMS: `RTEMS_PRIORITY|RTEMS_BINARY_SEMAPHORE|RTEMS_INHERIT_PRIORITY|RTEMS_NO_PRIORITY_CEILING|RTEMS_LOCAL`
  (`os/RTEMS-score/osdMutex.c:72`) — inheritance **and** a priority-ordered wait
  queue.
* Every lock this evaluation cares about is in that family: `dbLock.c:86`
  `ls->lock = epicsMutexMustCreate()` (what `dbScanLock` takes, `dbLock.c:184`;
  `dbScanLockMany` at `:384`); `dbLock.c:48`,`:62` `lockSetsGuard`;
  `dbScan.c:75` per-scan-list `lock`; `dbScan.c:122` `event_lock`;
  `dbScan.c:139` `ioscan_lock`.

**Parity verdict.** C protects the record lock, the lock-set registry, the event
list and the scan lists with PI + priority-ordered mutexes. The port protects
the same four with, respectively, a tokio async mutex (L1), a std mutex (L2), a
std mutex (L11) and tokio RwLocks (L8). **Zero of the four are PI today**, and
the first and fourth cannot become PI without changing their type.

One thing the port already has right: C's probe-and-fall-back
(`osdMutex.c:77-86`) is the same posture as `PriorityApplied::BestEffortFailed`
(`task.rs:428`, `:693`, `:711`) and the new `RtPolicy` switch — C also has an
opt-out. The Rust side does not need to invent a policy; it needs to apply the
one it has.

---

## §6 Recommended execution order

Ordered by (blocker for a hard-RT claim) × (cost). Steps 1 and 2 of my first
draft are struck through — the concurrent panel implemented them while this
investigation was running.

| # | step | why here | size |
|---|---|---|---|
| ~~0a~~ | ~~give the CA blocking threads a priority~~ | ⚠ **DONE, uncommitted** — `blocking.rs:200`, `:669` | — |
| ~~0b~~ | ~~give `cbTimer` a priority~~ | ⚠ **DONE, uncommitted** — `delayed_timer.rs:234` | — |
| **1** | **Decide L1.** Either make the record gate a blocking PI mutex — which requires removing the hold-across-await contract that `OwnedMutexGuard` exists to provide (`record_lock.rs:110`,`:121`,`:126`), a real design change — or accept it and **withdraw the hard-RT claim for the write path**. Nothing else in this list matters until this is answered: L1 is `dbScanLock`, and every write and every process pass takes it. | it is the one lock C most clearly protects and the port most clearly does not | design first, then **L** |
| **2** | **Finish the priority sweep.** `CAS-UDP` (`bin/rtems-ca-ioc.rs:159`), periodic scan (`scan_event.rs:87-92`), both iocsh threads (`ioc_app.rs:695`,`:1030`) still apply none. Periodic scan is the one that matters — it is a `JoinSet` tokio task, so giving it a priority means moving it off the tokio pool, not adding a line. | completes what 0a/0b started; without it the RT set still has holes | **S** for CAS-UDP/iocsh, **M** for scan |
| **3** | **L12 → PI.** The callback-queue mutex is on every submission path from every thread; highest contention-per-line in the table. | one lock, whole-process effect | **S** |
| **4** | **L2, L11 → PI**, then **L6, L3 → PI**, ascending by call-site count (L2 one site, L11 three, L6 two, L3 ~240). L3 and L6 are `RwLock`s and the PI type is mutex-only (`sync.rs:27`) — this step needs either a PI rwlock or an explicit decision to demote them, with the read-concurrency cost stated. | the rows PI was written for | **M**, and L3 alone is **L** |
| **5** | **Remove six of the eleven L8 locks** — init-time-write state to `ArcSwap`/`OnceLock`. Structural: shrinks the table instead of reclassifying it. | reduces the surface steps 6-7 apply to | **M** |
| **6** | **L14, then L7 and L9** — the remaining park_on set. L14 by routing save-on-change through the callback queue. L7 and L9 follow whatever step 1 decides; they share L1's constraint. | dependent on step 1 | **M** |
| **7** | **Make it reachable and measurable.** `linux-rt` is declared (`Cargo.toml:98`) and enabled nowhere; `is_pi_mutex_active()` (`sync.rs:36`) has no caller. Wire the feature into the RT profile alongside `EPICS_RS_ALLOW_RT_PRIORITY`, and report it at startup mirroring C's "PI is enabled" line (`osdMutex.c:205`). Add a latency regression that fails when a low-priority holder delays a high-priority waiter beyond a stated bound. | without this every step above is unverifiable, and 4b cannot be closed | **M** |

Step 2's cheap half (CAS-UDP, iocsh) is three lines and is worth doing in the
same change as 0a/0b, while that context is open.

---

## §7 What this evaluation does not establish

* **Nothing was measured.** Every claim is static analysis. Whether any of these
  inversions bites depends on `EPICS_RS_ALLOW_RT_PRIORITY` being on at all
  (`task.rs:459`), on `CAP_SYS_NICE` (`task.rs:678-679` warns when the switch is
  on but the process may not have permission), on core count and on load. The
  order in §6 is a correctness order, not a measured-impact order.
* **No RTEMS-target verification.** All priority reasoning is the Linux
  SCHED_FIFO path (`task.rs:559-711`, `#[cfg(target_os = "linux")]`). On RTEMS
  `apply_priority_impl` returns `PriorityApplied::Unsupported`
  (`task.rs:715-719`), so no priority is applied on the real target today. That
  is a separate gap from PI and belongs with RTEMS bring-up.
* **`epics-pva-rs` was not swept.** The brief named the CA blocking threads and
  the base background threads; the PVA server's locks are a further population.
* **The three modified files are uncommitted.** If the concurrent panel amends
  or drops those edits, rows marked ⚠ **moved** revert and §6 steps 0a/0b return
  to the list.
