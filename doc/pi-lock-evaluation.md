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

---

## §8 `epics-pva-rs` lock sweep — closing §7's named gap

Read-only. Appendable to `doc/pi-lock-evaluation.md` as §8; lock IDs continue the
parent table's numbering (parent ends at L15).

**Provenance.** `crates/epics-pva-rs` read in
`/home/stevek/work/epics-rs/.caucus/worktrees/integration` @ **`2ce9bd11`**
(branch `integration/rtems-scope-b`). HEAD unchanged start to end, but the
working tree **went dirty mid-sweep** (as in §0): a concurrent panel's
`PvaServerConfig` extraction modified `server_native/{mod,runtime,tcp}.rs` and
added `server_native/config.rs`. Re-verified against the final state — `tcp.rs`
is unaffected in every respect cited here (still 21,378 lines, test module still
at `:8097`, production still lock-free, imports still `:25`/`:26`/`:30`);
`runtime.rs` lost 494 lines to `config.rs` and **both** are lock-free in
production; only `mod.rs`'s gate line numbers shifted +3, and the numbers below
are the post-move ones. Planned-thread classification is against
`doc/pva-rtems-item7-design.md` in that same worktree. (d) is answered: **pvxs is
present** at `/home/stevek/work/epics-modules/pvxs` @ `9348ebc` — the
CLAUDE.md-listed `/Users/stevek/...` path does not exist on this host, the
documented fallback does.

---

### §8.0 Headline — three findings

**1. The PVA protocol scope is lock-free, and that is the opposite of the base
picture.** `server_native/tcp.rs` production (lines 1-8096; the test module
starts at `:8097`) contains **zero** `Mutex`/`RwLock`/`Condvar` — the only match
in 8096 lines is the words `Arc<Mutex<SrvWrite>>` inside a doc comment at
`:2841` explaining what the design *replaced*. Per-connection state is carried
by `Arc`, `AtomicU32`/`AtomicU64` and `tokio::sync::mpsc` (`tcp.rs:25-30`).
`udp.rs`, `runtime.rs`, `accept.rs`, `monitor_control.rs`, `op_handle.rs` and
`server_info.rs` are likewise lock-free in production — as is the newly
extracted `config.rs`. So the whole 21,378-line
TCP module contributes **no rows to this table**. Item 7's thread-per-connection
split therefore does not multiply lock contention in the protocol path — a
materially better starting position than `epics-base-rs`, where 25 shared locks
sit under the record path.

**2. Where the port does hold locks, they are almost all `parking_lot` — so
unlike the parent sweep, PI is actually reachable here.** Of **21 distinct
server-side locks**, 17 are `parking_lot`, 3 are `std::sync`, and exactly
**one** is `tokio::sync` (L23, the ACF cell). The parent table's dominant class —
park_on-PI-invisible, 14 of 25 — is here a **single row**. `epics-pva-rs` never
re-exports `runtime::sync`, so it never fell into the naming trap that made
`database/mod.rs:19` and `pv.rs:4` silently async.

**3. The item-7 design assigns no thread priorities, so it would re-open the
exact asymmetry §6 steps 0a/0b just closed for CA.** `rg -i "priorit|ThreadPriority|apply_to_current_thread"`
over `doc/pva-rtems-item7-design.md` returns **nothing**. The three planned
threads — `PVAS-client {peer}` (`§2` of that doc, replacing `tcp.rs:2539`),
`PVAS-UDP` (replacing the `:962-975` spawn), `PVAS-beacon` (replacing
`udp.rs:500`) — would run SCHED_OTHER while the callback bands that *post* to
them run 59-71. Every "shared" row below is shared precisely across that line.

---

### §8.1 (a)+(b)+(c) The table

**Kind** as in §2: `PL` = `parking_lot`, `STD` = `std::sync` (both kernel-visible,
PI-able by replacement), `TOK` = `tokio::sync` (awaited; park_on-invisible).

**Shared?** — the blocking PVA driver does not exist yet, so per the brief every
row is classified against the *planned* item-7 threads, not live ones. `now` =
already shared today on the hosted tokio server; `item 7` = becomes shared when
the planned threads land; `gated` = module is `#[cfg(not(target_os = "rtems"))]`
today (`server_native/mod.rs:24,34,36,41,43` — `accept`, `peers`, `runtime`,
`tcp`, `udp`) so it has no RTEMS presence until item 7 un-gates it.

| # | lock | decl / evidence | Kind | shared? | class | pvxs parity |
|---|---|---|---|---|---|---|
| **L16** | `SharedPV::inner` | `shared_pv.rs:468` `Arc<Mutex<Inner>>` (`parking_lot`, import `:31`); ~30 acquisitions `:495`-`:975`; write side `try_post_checked` `:650`, `put_delta` `:798`; read side `:602`,`:610`,`:640` | **PL** | **now** — cb band posts, connection thread reads | **replace-with-PI** — the top row of this sweep | `sharedpv.cpp:37` `mutable epicsMutex lock` — **PI on RTOS** |
| **L17** | `MonitorQueue::inner` | `shared_pv.rs:159` `Mutex<MonitorQueueInner<T>>` (PL); producer `post` `:235`,`:239`,`:282`,`:305`; consumer `try_recv` `:347`,`:348` | **PL** | **now** — producer is the posting band, consumer is the per-connection thread | **replace-with-PI** — the cleanest high/low split in the crate | `servermon.cpp:57` `mutable epicsMutex lock` — **PI on RTOS** |
| **L18** | `PvRegistry::pvs` | `shared_pv.rs:1218` `Mutex<HashMap<String, SharedPV>>` (PL); `:1287`,`:1332`,`:1348`,`:1391`,`:1399`,`:1404`,`:1412`,`:1417`,`:1426`,`:1448` | **PL** | **item 7** — `PVAS-UDP` search + `PVAS-beacon` `list_pvs` vs. registration | **accept-with-bounded-CS** (§8.2) | no counterpart — pvxs `StaticSource` is reactor-confined |
| **L19** | `PvRegistry::invalidator` | `shared_pv.rs:1245` `Mutex<Option<ChannelInvalidator>>` (PL); `:1295`, `:1392` — **both taken while `pvs` is held**, ordering pvs→invalidator | **PL** | **item 7** | **accept-with-bounded-CS**; one `Option` clone | — |
| **L20** | `CompositeSource::entries` | `composite.rs:38`,`:65` `Arc<parking_lot::RwLock<BTreeMap<(i32, String), DynSource>>>`; writes `:108`,`:128`, read `:160` | **PL** | **item 7** — every channel lookup from every connection thread | **accept-with-bounded-CS** — writes are registration-time only | — (reactor-confined) |
| **L21** | `RemoteLogQueue::queued` | `source.rs:51` `Arc<Mutex<Vec<RemoteLogMessage>>>` (`std::sync`, import `:7`); `:66` push, `:76` drain via `mem::take` | **STD** | **item 7** | **accept-with-bounded-CS** — push/`mem::take`, no allocation held | `log.cpp` uses `epicsMutex` |
| **L22** | `ChannelInvalidator::subscribers` | `source.rs:440` `Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<Arc<[String]>>>>>`; `:457`, `:470` | **STD** | **item 7** | **accept-with-bounded-CS** — `Vec` retain over subscriber senders | — |
| **L23** | PVA `AcfCell` | `server/native_source.rs:30` `Arc<RwLock<Option<AccessSecurityConfig>>>` — **tokio** (import `:21`); written `pva_server.rs:145`,`:162` `.write().await`; read in base at `access_security.rs:317` `.read().await` | **TOK** | **item 7** — read on every access check from every connection thread | **(c) park_on — PI-invisible**; also **accept** (read-mostly, written only by `reload_acf_from`/`clear_acf`) | `asLock` family; pvxs delegates to base |
| **L24** | `PeerEntry` ×3: `report_info` `peers.rs:47`, `credentials` `:130`, `channels_by_sid` `:137` | all `parking_lot::Mutex`; `:68`,`:168`,`:179`,`:186`,`:247`,`:254`,`:320`,`:323`,`:329` | **PL** ×3 | **gated** → item 7 | **accept-with-bounded-CS** — per-peer, and the introspection reader is the only cross-thread acquirer | **none** — pvxs `serverconn.h`/`server.h` declare **no mutex at all** |
| **L25** | `PeerRegistry::inner` | `peers.rs:201` `parking_lot::RwLock<HashMap<SocketAddr, Arc<PeerEntry>>>`; `:210` insert, `:214` remove, `:229`/`:265` read — `:229-254` holds the registry read **across** `channels_by_sid.lock()` and `report_info.lock()`, ordering registry→entry→channel | **PL** | **gated** → item 7 | **accept-with-bounded-CS**, with the nesting named (§8.2) | **none** — same as L24 |
| **L26** | `client_native` ×11: `channel.rs:77` `state` (PL RwLock, import `:26`), `:80` `transition_lock` (**tokio**, import `:27`), `:100`,`:112`,`:128`,`:143`,`:155`,`:186`,`:195`,`:196`,`:203` (PL) | see decls | mixed **PL**/**TOK** | **never** — `#[cfg(feature = "client")]` (`lib.rs:22`,`:24`; `Cargo.toml:97`), and the client is gate-out territory on RTEMS by explicit scope decision (`Cargo.toml:93-95`) | **out of scope** — one-line note, not classified | `clientimpl.h` uses `epicsMutex` |
| **L27** | `client_native` rest: `server_conn.rs:111`,`:923` (PL), `context.rs:376` (PL RwLock), `beacon_throttle.rs:72` (PL RwLock), `udp.rs:86`,`:87`,`:211` (`std::sync`), `ops_v2.rs:1908`,`:1917` (PL) | see decls | mixed | **never** — same gate as L26 | **out of scope** | `udp_collector.cpp:597` `epicsMutex lock` |

**Server-side totals (L16-L25): 21 distinct locks** — 17 `PL`, 3 `STD`, 1 `TOK`.
Client-side (L26-L27): 20 further locks, all behind `feature = "client"`, not
classified.

**(c) The park_on set is one row: L23.** It is the only `tokio::sync` primitive
a planned blocking PVA thread would wait on, and it reaches
`access_security.rs:317`'s `.read().await` — under a `PVAS-client` thread that
becomes `block_on_sync` → `park_on` → `std::thread::park()`
(`epics-base-rs/src/runtime/task.rs:80`), invisible to the kernel PI chain for
the reasons given in §3. Consequence for L23 specifically is mild: it is a
read-mostly cell whose only writers are `reload_acf_from`/`clear_acf`
(`pva_server.rs:145`,`:162`), so the inversion window is an operator action, not
a steady-state path. **No other PVA lock is park_on-reachable** — which is why
the parent sweep's dominant class is nearly empty here.

---

### §8.2 (b) Rationale for the non-obvious rows

**replace-with-PI (L16, L17).** Both are `parking_lot` mutexes with no `.await`
in the critical section (guaranteed by type — `parking_lot::MutexGuard` is
`!Send`). Both sit exactly on the band/connection boundary: the poster is
whatever thread drives record processing (a callback band in an IOC), the reader
is the per-connection thread that item 7 will create. **L17 is the higher-value
of the two**: it is the narrowest lock with the clearest producer/consumer split
(`post` `:235` vs `try_recv` `:347`), so a PI swap there is a contained change.
L16 is the broader one (~30 acquisition sites) and should follow L17.

Note the nesting: `try_post_checked` (`:650`) and `put_delta` (`:798`) hold
`SharedPV::inner` **while** calling `tx.post(...)` (`:664`, `:837`), which takes
`MonitorQueue::inner`. Ordering is uniformly L16→L17. Any PI conversion must
convert them together or the donated priority stops at the outer lock.

**accept-with-bounded-CS (L18-L22, L24, L25).** Bounds, stated:
* **L18** — one hash probe plus a `SharedPV` clone (itself an `Arc`); the two
  cross-lock sites (`:1391-1392`, `:1287-1295`) additionally clone one `Option`.
* **L19** — a single `Option<ChannelInvalidator>` clone.
* **L20** — writes only at source registration (`:108`, `:128`); the hot path is
  the `:160` read, a `Vec` iteration over registered sources (small N).
* **L21** — `Vec::push` and `mem::take` (`:76`); the drain moves the buffer out
  rather than iterating under the lock.
* **L22** — `Vec` retain over subscriber senders.
* **L24/L25** — per-peer fields; the only cross-thread reader is the
  introspection/`snapshot` path (`:229-254`, `:320-329`). Bound is O(peers ×
  channels) **and it is held across two nested locks**, so it is the one
  "accept" row with a real tail: a `snapshot` during a large channel population
  blocks peer insert/remove at `:210`/`:214`. Acceptable because `snapshot` is
  an operator/introspection call, not a per-datagram path — but that is the
  invalidating condition, and it should be named in the code rather than left to
  a reader to infer.

**No remove-the-sharing rows.** The parent table had two (L8 subset, L14) because
base holds init-time-write state behind runtime locks. The PVA server does not:
L20's writes are registration-time but the lock is still needed for the hot read
path (sources can be added after start), and L18/L19 are genuinely dynamic.

---

### §8.3 (d) pvxs parity — and why it is mostly "no counterpart"

pvxs @ `9348ebc`. Where pvxs holds a lock, it is an `epicsMutex` — i.e. the same
`PTHREAD_PRIO_INHERIT` / `RTEMS_INHERIT_PRIORITY` family established in §5:

* `sharedpv.cpp:37` `mutable epicsMutex lock` (guards typedef'd at `:22-23`,
  taken at `:86`,`:167`,`:197`,`:214`,`:239`,`:259`,`:267`,`:283`) ↔ **L16**.
* `servermon.cpp:57` `mutable epicsMutex lock` (`:111`,`:201`,`:267`,`:305`,
  `:331`) ↔ **L17**.
* `udp_collector.cpp:597` `epicsMutex lock` ↔ **L27** (client side).

But for the rows that came out of *our* threading model, pvxs has nothing to
compare against: `rg "epicsMutex|Guard G\("` over `server.cpp`, `serverconn.cpp`,
`server.h`, `serverconn.h`, `serverimpl.h` returns **zero matches**. pvxs runs
one libevent reactor thread and confines connection and peer state to it, so
there is no peer-registry lock to be PI or not (**L24, L25**), and no
source-registry lock on the hot path (**L20**).

**The parity verdict is therefore two-sided, and the second half is the
important one:**

1. Both locks pvxs *does* have are PI on an RTOS; neither of our counterparts
   (L16, L17) is. That is a straight parity gap, and it is fixable by type
   replacement because both are `parking_lot`.
2. Four of our rows exist **only because item 7 replaces one reactor with N
   threads** (L20, L24, L25, and L18's cross-thread reader). Item 7 does not
   inherit these from pvxs — it creates them. The §6 execution order should
   treat them as new debt introduced by the port's threading model, not as
   parity work.

This also retires the caveat in the brief ("pvxs single-reactor has no
lock-per-lock analogue anyway"): it has an analogue for exactly the two rows
that matter most, and its *absence* elsewhere is itself the finding.

---

### §8.4 Where this fits in §6's execution order

These do not displace §6 steps 1-2; they slot in as a PVA track that only
becomes live when item 7 lands.

| slot | step | rationale |
|---|---|---|
| **before item 7 stage C** | **Assign priorities to `PVAS-client`, `PVAS-UDP`, `PVAS-beacon` in the item-7 design.** The design currently names none (§8.0 finding 3). C has no PVA precedent, but `ThreadPriority::CaServerLow`/`CaServerHigh` (`task.rs:370-371`) is the obvious mapping, matching what `blocking.rs:200`/`:669` just did for CA. Cheapest possible moment is before the threads are written. | prevents re-opening the asymmetry §6 0a/0b closed |
| **with §6 step 3-4** | **L17 → PI, then L16 → PI** (in that order; convert together per the L16→L17 nesting). Both `parking_lot`, both with a pvxs `epicsMutex` precedent. | closes the only genuine C++-parity lock gap in the crate |
| **with §6 step 7** | **L23** — inherits whatever §6 step 1 decides for tokio-locked shared cells; low urgency (operator-action writer only). | shares L1's constraint, not its severity |
| **item 7 review gate** | **Name L25's `snapshot` bound in code**, and re-check L18/L20 if sources or PVs become registerable from a datagram path. | the two "accept" rows with a stated invalidating condition |

---

### §8.5 What this sweep does not establish

* **Nothing measured** — static analysis only, same caveat as §7.
* **The producer identity for L16/L17 is inferred from the API, not traced to a
  band.** `SharedPV::post`'s only production callers inside `epics-pva-rs` are
  its own internals; the cross-crate producer lives in `epics-bridge-rs`
  (`qsrv/pva_adapter.rs:85` `PvaPvHandle::post`, whose own
  `subscribers: Arc<parking_lot::Mutex<...>>` at `:50`/`:67`/`:93` is a **22nd
  lock in a crate this brief did not scope**). Whether that call arrives on a
  callback band or a tokio worker depends on the bridge's emission path, which I
  did not trace.
* **`epics-bridge-rs` was not swept** — `pva_adapter.rs:50` is one confirmed
  shared lock there; the population is unknown. That is the next gap, exactly as
  `epics-pva-rs` was this one.
* **Client-side (L26, L27, 20 locks) is enumerated but not classified**, per the
  RTEMS scope decision at `Cargo.toml:93-95`. If a PVA gateway on RTEMS ever
  comes back into scope, that classification is owed.

---

## §9 `epics-bridge-rs` lock sweep + the L16/L17 producer trace

Read-only. Appendable to `doc/pi-lock-evaluation.md` as §9; lock IDs continue
after §8's L27.

**Provenance.** `crates/epics-bridge-rs` read in
`/home/stevek/work/epics-rs/.caucus/worktrees/integration` @ **`42a42abf`**
(branch `integration/rtems-scope-b`) — the concurrent panel's `PvaServerConfig`
extraction, in flight during §8, has landed; HEAD moved `2ce9bd11` → `42a42abf`
before this sweep began. HEAD `42a42abf` and a **clean** working tree at start,
mid-sweep and end — no file cited here went dirty, so no re-verification was
owed.

**(d) column skipped.** qsrv's upstream is `pva2pva`/`qsrv`, which is not on this
host: `/home/stevek/work/epics-modules` holds 30 modules but no `pva2pva` or
`qsrv` (nearest neighbours are `pvxs` and `ca-gateway`), and `/home/stevek/codes`
holds four unrelated projects. Per the brief this is a skip, not a stop — and
the bridge is largely port-original anyway, so most rows would have no
counterpart even with the source present.

---

### §9.0 The L16/L17 producer trace — §8.5's deferred question, answered

**The answer overturns §8's classification of L16/L17.** I traced the emission
path hop by hop and it does not lead where §8 assumed.

**Hop chain, starting where the brief said to start:**

| hop | site | what it does |
|---|---|---|
| 1 | `qsrv/pva_adapter.rs:85` `PvaPvHandle::post` | validates against the canonical descriptor, then `*self.latest.lock() = Some(value.clone())` (`:92`) and `let mut subs = self.subscribers.lock()` (`:93`) — **L28/L29 below** |
| 2 | `ad-plugins-rs/src/pva.rs:70` `self.handle.post(payload)` | the only production caller anywhere in the workspace |
| 3 | `ad-plugins-rs/src/pva.rs:62` `NDPluginProcess::process_array` | the plugin trait method containing hop 2 |
| 4 | `ad-core-rs/src/plugin/runtime.rs:598` `self.processor.process_array(array, &self.pool)` | inside `process_and_publish` (`:581`) |
| 5 | `ad-core-rs/src/plugin/runtime.rs:1913` `guard.process_and_publish(&msg.array)` | inside `plugin_data_loop` |
| 6 | `ad-core-rs/src/plugin/runtime.rs:1751-1753` `thread::Builder::new().name(format!("plugin-data-{port_name}")).spawn(...)` | **the driving thread** |

**The driving thread is `plugin-data-{port}`, a plain `std::thread` at default
priority.** `rg "apply_to_current_thread|ThreadPriority"` over `crates/ad-core-rs/src`
and `crates/ad-plugins-rs/src` returns **nothing** — it is SCHED_OTHER, not a
callback band, not a tokio worker, not iocsh.

**And this path never touches L16/L17 at all.** `PvaPvHandle` (`pva_adapter.rs:48-102`)
is an *independent reimplementation* of the pvxs `SharedPV::post` contract, not a
wrapper over `epics_pva_rs::SharedPV`: it owns its own `latest` (`:49`) and
`subscribers` (`:50`) `parking_lot` mutexes and posts to `mpsc::Sender`s. Every
mention of `SharedPV` in `pva_adapter.rs` (`:45`, `:77`, `:100`, `:113`, `:340`)
is a doc comment citing pvxs as the *model*.

**So who does drive L16/L17? Nothing in production.** `rg` for `SharedPV` /
`try_post_checked` / `.try_post(` across the workspace excluding `epics-pva-rs`
itself returns hits in exactly one file — `crates/epics-bridge-rs/tests/pva_gateway.rs`
(`:61`, `:237`, `:344`, `:901`, `:1060`, `:1126`, `:1244`, `:1353`, `:1484`) —
plus the doc comments above. **`SharedPV`/`MonitorQueue` are a public API surface
for external embedders, exercised in-tree only by tests.**

**The real IOC monitor path bypasses them entirely.** A records-backed PVA
monitor is served by `MonitorStream::Upstream` (`server_native/source.rs:1739`)
wrapping `UpstreamMonitor::from_db` (`:1912-1921`), which holds a
`epics_base_rs::server::database::db_access::DbSubscription` (`:1913`) — i.e. it
rides the **base** subscription machinery (§2's **L7**, `ProcessVariable::subscribers`,
a tokio Mutex and already classified park_on-PI-invisible), never
`shared_pv.rs`'s locks.

**Consequences — three corrections to §8:**

1. **L16/L17's "replace-with-PI" has no high-priority acquirer today, and item 7
   does not create one.** Item 7 changes the *consumer* side (per-connection
   threads); the *producer* side only exists when an external embedder posts.
   The classification should be downgraded from "the top row of this sweep" to
   **replace-with-PI, but unranked — no in-tree producer**. The pvxs parity gap
   (`sharedpv.cpp:37`/`servermon.cpp:57` are `epicsMutex`) is real and still
   worth closing for embedders, but it is not on any hot path this workspace
   runs.
2. **The row that *does* have a live producer is L29** (`PvaPvHandle::subscribers`),
   and its producer is `plugin-data-{port}` at default priority — so it is a
   low↔low pair today, not a band↔connection pair.
3. **§8's L7 cross-reference was the right lock all along.** The IOC monitor path
   the bridge actually uses lands on base's L7, which the parent sweep already
   classified as park_on-PI-invisible.

---

### §9.1 (a)+(b)+(c) The table

**Kind** as in §2/§8: `PL` = `parking_lot`, `STD` = `std::sync`, `TOK` =
`tokio::sync` (park_on-invisible when reached from a blocking thread).

**Shared?** — `hosted` = live today on the hosted tokio server; `item 7` =
becomes relevant only if the bridge is ported to the RTEMS blocking driver;
`host-only` = gateway subsystems with no RTEMS story at all.

| # | lock | decl / evidence | Kind | shared? | class |
|---|---|---|---|---|---|
| **L28** | `PvaPvHandle::latest` | `qsrv/pva_adapter.rs:49` `Arc<parking_lot::Mutex<Option<PvField>>>`; `:92` write, `:117` read | **PL** | **hosted** — `plugin-data-{port}` writes, connection task reads | **accept-with-bounded-CS** — one `Option<PvField>` swap/clone |
| **L29** | `PvaPvHandle::subscribers` | `qsrv/pva_adapter.rs:50` `Arc<parking_lot::Mutex<Vec<mpsc::Sender<PvField>>>>`; `:93` post-retain, `:125` subscribe | **PL** | **hosted** — see §9.0; producer is SCHED_OTHER today | **replace-with-PI** *conditionally* — only once `plugin-data-*` gets a priority (§9.3) |
| **L30** | `PVA_PV_REGISTRY` | `qsrv/pva_adapter.rs:143` `std::sync::Mutex<HashMap<String, PvaPvHandle>>`; `:155` insert, `:163` `mem::take` drain | **STD** | **hosted**, init-time | **remove-the-sharing** — a global drained once by `take_registered_pva_pvs`; an init-time handoff, not shared state |
| **L31** | `BridgeSource::pva_pvs` | `qsrv/pva_adapter.rs:194` `Arc<RwLock<HashMap<String, PvaPvHandle>>>` — **tokio** (import `:13`); ~10 `.read().await`: `:291`,`:337`,`:489`,`:526`,`:787`,`:1001`,`:1016`,`:1034`,`:1054` | **TOK** | **item 7** — read on every channel op | **(c) park_on — PI-invisible**; also **accept** (read-mostly after init) |
| **L32** | `BridgeProvider` ×5: `groups` `qsrv/provider.rs:628` (PL RwLock), `record_cache` `:641` (**tokio** RwLock), `access_cell` `:647` (PL RwLock), `base_group_defs` `:656` (PL), `group_files` `:668` (PL) | mixed | **item 7** | 4× **accept-with-bounded-CS** (map/vec lookups, init-time writes); `record_cache` is **(c) PI-invisible** |
| **L33** | qsrv group `atomic_write_lock` | `qsrv/group_config.rs:64` `Arc<tokio::sync::Mutex<()>>` | **TOK** | **item 7** — the group-atomic-put gate | **(c) park_on — PI-invisible**; the bridge's `dbScanLockMany` analogue, and it inherits **L1**'s whole problem (§3) |
| **L34** | `PvaLinkResolver` ×6: `link_options` `pvalink/integration.rs:59`, `out_link_options` `:66`, `db` `:71`, `scan_targets` `:78` (PL RwLock), `forwarders` `:84` (PL Mutex), `qsrv` `:95` (PL RwLock) | **PL** ×6 | **hosted, and genuinely high↔low** — `impl LinkSet for PvaLinkResolver` (`:1115`), registered at `:837` `db.register_link_set("pva", …)`, so these are acquired during **record processing** | **replace-with-PI** — the highest-value rows in this crate (§9.2) |
| **L35** | `PvaLink` ×5: `latest` `pvalink/link.rs:153`, `notify_rx` `:178`, `out_scratch` `:205`, `disconnect_time` `:222`,`:300`, `snap_alarm` `:248` | **PL** ×5 (import `:8`) | **hosted**, same record-processing path as L34 | **replace-with-PI** (`latest`, `disconnect_time`); **accept-with-bounded-CS** (`notify_rx`, `out_scratch`, `snap_alarm`) |
| **L36** | `LinkRegistry` ×3: `map` `pvalink/registry.rs:81`, `pending` `:91`, `channel_records` `:98` | **PL** ×3 RwLock (import `:9`) | **hosted**, link open/close | **accept-with-bounded-CS** — hash probes; writes at link attach/detach |
| **L37** | `ChannelEntry` ×3: `state` `pva_gateway/channel_cache.rs:81`, `latest_raw` `:99` (PL RwLock), `drop_poke` `:106` (PL Mutex) | **PL** ×3 | **host-only** | **accept-with-bounded-CS** |
| **L38** | `ChannelCache` ×2: `entries` `pva_gateway/channel_cache.rs:597` (**tokio** Mutex, import `:34`), `cleanup_task` `:599` (PL Mutex) | mixed | **host-only** | **(c) PI-invisible** (`entries`); **accept** (`cleanup_task`) |
| **L39** | `GatewaySource` ×3: `upstream_pool` `pva_gateway/source.rs:333` (PL Mutex, import `:18`), `asg_resolver` `:353` (**tokio** RwLock, import `:19`), `upstream_caches` `:375` (PL Mutex) | mixed | **host-only** | **accept-with-bounded-CS**; `asg_resolver` **(c) PI-invisible** |
| **L40** | CA-gateway `PvCache` + per-entry: `entries: HashMap<String, Arc<RwLock<GwPvEntry>>>` `ca_gateway/cache.rs:282` and the cache itself `Arc<RwLock<PvCache>>` (`upstream.rs:227`,`:245`,`:293`; `server.rs:347`,`:829`; `command.rs:86`,`:117`) — **tokio** RwLock (import `cache.rs:34`) | **TOK** ×2 | **host-only** | **(c) PI-invisible**; note `cache.rs:335-341`/`:365` already document collect-then-act to bound the outer guard |
| **L41** | `UpstreamManager` ×2: `subs` `ca_gateway/upstream.rs:303`, `pending` `:307` (PL Mutex) | **PL** ×2 | **host-only** | **accept-with-bounded-CS** |
| **L42** | stats ×2: `per_host` `ca_gateway/stats.rs:161`, `last_refresh` `:169` — **tokio** Mutex (import `:100`) | **TOK** ×2 | **host-only** | **(c) PI-invisible**; also **accept** (introspection path) |
| **L43** | downstream ×3: `log` `ca_gateway/downstream.rs:74` (PL Mutex), `server` `:278`, `replay_state` `:287` (**tokio** Mutex, import `:29`) | mixed | **host-only** | **accept**; the two tokio ones **(c) PI-invisible** |
| **L44** | putlog ×2: `file` `ca_gateway/putlog.rs:135`, `bytes_written` `:139` — **tokio** Mutex (import `:68`) | **TOK** ×2 | **host-only** | **(c) PI-invisible**; held across `tokio::fs` I/O — the longest critical section in this table |
| **L45** | beacon ×2: `last` `ca_gateway/beacon.rs:25`, `pulse` `:34` — **std::sync** Mutex (import `:14`) | **STD** ×2 | **host-only** | **accept-with-bounded-CS** — one `Instant`/`Option<Arc>` |

**Totals: 45 distinct production locks** — 29 `PL`, 13 `TOK`, 3 `STD`. By
subsystem: qsrv 10 (L28-L33), pvalink 14 (L34-L36), pva_gateway 8 (L37-L39),
ca_gateway 13 (L40-L45).

**(c) the park_on-reachable set: L31, L32's `record_cache`, L33, L38's `entries`,
L39's `asg_resolver`, L40 ×2, L42 ×2, L43 ×2, L44 ×2 = 13 locks.** Only three of
those (L31, L32-`record_cache`, L33) are in a subsystem item 7 could ever reach;
the other ten are gateway-only and have no RTEMS story. **L33 is the one that
matters**: it is the group-atomic-put gate, structurally the same problem as
**L1** — a tokio `Mutex` standing in for `dbScanLockMany` — and it inherits §3's
conclusion verbatim.

---

### §9.2 Why L34/L35 are the real find in this crate

Unlike L16/L17, the pvalink rows have a **traced, live, high-priority acquirer**:
`PvaLinkResolver` implements base's `LinkSet` (`pvalink/integration.rs:1115`) and
is registered into the database at `:837` (`db.register_link_set("pva", Arc::new(resolver.clone()))`).
Link resolution therefore runs inside record processing — on a callback band or
`scanOnce` — while the same locks are taken by the monitor-forwarder side
(`scan_once` at `:1003`, reaching `process_record_with_links` at `:1104` and
`process_record_with_links_already_locked` at `:1100`).

That makes L34/L35 the only rows in §8+§9 combined that are simultaneously
(i) `parking_lot`, so PI is reachable by type replacement, (ii) contended across
a genuine high/low boundary, and (iii) on a path this workspace actually runs.
They outrank L16/L17 — which §8 nominated on an assumption this trace has now
disproved.

**One caveat that bounds the claim.** The forwarder side is spawned through a raw
`tokio::runtime::Handle` (`integration.rs:36`, `:156`, `:417` `self.handle.spawn(run_notify_forwarder(...))`,
`:829`), **not** through the `runtime::task` seam. So on the hosted backend the
forwarder is a tokio worker (SCHED_OTHER) and the record-processing side is the
band — a real high↔low pair. There is no exec-backend variant of this pair today,
for the reason in §9.3.

---

### §9.3 Two findings outside the lock table

**1. `epics-bridge-rs` uses the `runtime::task` seam exactly once.**
`rg "runtime::task::spawn|epics_base_rs::runtime::task"` over
`crates/epics-bridge-rs/src` returns a single production hit —
`qsrv/pva_adapter.rs:1436`. Against that, raw `tokio::spawn` /
`tokio::runtime::Handle` appear in 10 production files, `pvalink/integration.rs`
alone holding 42. The crate is **not** RTEMS-gated in its own manifest
(`rg rtems crates/epics-bridge-rs/Cargo.toml crates/epics-bridge-rs/src/lib.rs`
→ nothing), so its RTEMS status is *undeclared* rather than *excluded*. I did not
verify what the RTEMS build actually compiles, so I am not claiming a live seam
violation — the accurate statement is: **if the bridge ever enters the RTEMS
build, those sites are seam bypasses and would need auditing first.** That is a
prerequisite finding for any "port qsrv to RTEMS" item, and it is cheap to close
by declaring the crate host-only now.

**2. The `plugin-data-{port}` thread has no priority.** `ad-core-rs/src/plugin/runtime.rs:1751-1753`
(and a second at `:2296-2298`) spawn named `std::thread`s that drive all NDArray
plugin processing, including hop 2 of §9.0. No `apply_to_current_thread` exists
anywhere in `ad-core-rs` or `ad-plugins-rs`. In C, areaDetector plugin threads are
created with an explicit `epicsThreadPriority` per plugin. This is the same class
of gap as §6 step 2's `CAS-UDP`/periodic-scan/iocsh holes, in a crate neither §6
nor §8 looked at.

---

### §9.4 Where this fits in §6's execution order

| slot | step | rationale |
|---|---|---|
| **revises §6 step 4** | **Demote L16/L17.** They keep the pvxs parity gap but lose their ranking — no in-tree producer (§9.0). Convert for embedder correctness, not for latency. | §8 ranked them on an assumption this trace disproved |
| **new, alongside §6 step 3** | **L34/L35 → PI.** `parking_lot`, live high↔low, on the record-processing path. The best-evidenced PI candidates found in three sweeps. | §9.2 |
| **with §6 step 2** | **Give `plugin-data-{port}` a priority** (`runtime.rs:1751`, `:2296`), matching C's per-plugin `epicsThreadPriority`. Until then L29's producer/consumer are both SCHED_OTHER and its PI classification is inert. | §9.3 finding 2 |
| **before any qsrv-on-RTEMS item** | **Declare `epics-bridge-rs` host-only, or audit its 40+ raw-spawn sites against the seam.** | §9.3 finding 1 |
| **follows §6 step 1** | **L33** — the group-atomic-put gate inherits L1's decision exactly. | §9.1 |

---

### §9.5 What this sweep does not establish

* **Nothing measured** — static analysis, same caveat as §7 and §8.5.
* **I did not verify what the RTEMS build compiles.** §9.3 finding 1 is stated as
  conditional for that reason; confirming it needs a `--target` build, which is
  outside a read-only sweep.
* **`ad-core-rs` / `ad-plugins-rs` were not swept for locks** — I entered them only
  to complete the §9.0 hop chain. `runtime.rs:95` shows at least one
  `Arc<Mutex<Option<Arc<NDArray>>>>` on the plugin path; the population is
  unknown and is the next gap, exactly as `epics-bridge-rs` was this one.
* **Test-only locks were excluded throughout**, using the last `#[cfg(test)]` in
  each file as the boundary. `pva_gateway/source.rs` has an early `#[cfg(test)]`
  at `:25` as well as the module at `:1982`; the `:1982` boundary is the one used.
