# RTEMS priority locks — design for handoff §8.0 gaps 3 and 4

Design only. **No production code is changed by this document.** It answers the
two gaps `doc/rtems-scope-b-session-handoff.md` §8.0 left open and marked
"needs the user's sign-off":

* **gap 3** — priority-ordered wait, not tokio FIFO.
* **gap 4** — priority inheritance on **L1**, the `dbScanLock` analogue.

The base analysis is `doc/pi-lock-evaluation.md` on `main` (§0/§3/§5/§6). This
document does not restate it; it **re-verifies** the parts it depends on
against the tree named below, corrects four of them, and turns the result into
a staged plan.

## Provenance

* **Worktree** — `.caucus/worktrees/VVK0HDQKKZ-pi-lock-design-4ec993ff-1`
  @ **`6852ea13`** (`fix(scan): run periodic scans on dedicated banded threads,
  as dbScan.c does`), branch `caucus/VVK0HDQKKZ/pi-lock-design-4ec993ff-1`.
  Working tree clean at start and at end; no file cited here moved under me.
* **Every `file:line` in this document was re-read in that worktree.** The
  evaluation on `main` was written against an older tree and several of its
  line numbers and one of its conclusions no longer hold; those are marked
  **⚠ moved** or **⚠ corrected** where they appear.
* **C reference** — `/home/stevek/work/epics-base` @ `669a25697`, present, so
  the reference-source rule is satisfied.
* **libc fork** — the `[patch.crates-io]` pin (`Cargo.toml:71-72`), rev
  `6f64e70d6a2a03b989380aaa719207236343f093`, read at
  `~/.cargo/git/checkouts/libc-ce7c688b00a5ff12/6f64e70`.
* **Not available on this host** — no RTEMS toolchain or sysroot
  (`find / -maxdepth 4 -name pthread.h -path '*rtems*'` → nothing). Target
  header facts in §4 come from the box measurement supplied to this panel and
  are attributed there, not re-derived here.

---

## §0 Headline — five findings, four of them corrections

**1. The L1 hold-across-await contract is used by 7 of the 9 production
holders, not by all of them — and the blocker is not the `.await` count.**
The exhaustive audit is §1. Two holders (`field_io.rs:1165`, `field_io.rs:2198`)
take the gate and never await while holding; they convert for free. The other
seven do. But the thing that actually stops the conversion is that a
`parking_lot`/`pthread` guard is **`!Send`** while `tokio::sync::OwnedMutexGuard`
is `Send`: any `async fn` that holds a `!Send` guard across an await produces a
`!Send` future, and the hosted CA/PVA servers `tokio::spawn` those futures onto
a multi-thread runtime. So the restructure is not "delete the awaits" — it is
"lift the gate-held region out of the async body", per holder. §2 states it as
such.

**2. ⚠ corrected — C on RTEMS 6 does *not* use
`rtems_semaphore_create(RTEMS_PRIORITY|RTEMS_INHERIT_PRIORITY)`. It uses the
POSIX arm.** `doc/pi-lock-evaluation.md` §5 and the handoff's §8.0 gap-3 row
("lock wait discipline: C = `RTEMS_PRIORITY` ordered") both cite
`os/RTEMS-score/osdMutex.c:72`. That file is not compiled on RTEMS 6.
`configure/toolchain.c:31-35` selects `OS_API = posix` for
`__RTEMS_MAJOR__ >= 5`; `configure/CONFIG_COMMON:141` expands that to
`OS_IMPL_DIRS = os/RTEMS-posix os/RTEMS`; and
`modules/libcom/src/osi/os/RTEMS-posix/osdMutex.c:8` is one line —
`#include "../posix/osdMutex.c"`. So base-on-RTEMS-6's `epicsMutex` is a
`pthread_mutex_t` with `pthread_mutexattr_setprotocol(PTHREAD_PRIO_INHERIT)`
and a **probe-and-fall-back** (`posix/osdMutex.c:71-88`), reported by
`epicsMutexShowAll` as "PI is/is not enabled" (`:199-205`).

   Two consequences, and both change what the gaps *are*:

   * **Gap 4 gets easier and better defined.** The parity target is a POSIX PI
     mutex, which is exactly what `PriorityInheritanceMutex` already is on
     Linux (`runtime/sync.rs:26-27`, `:55-81`). It is the same construction on
     the same API, not an RTEMS classic-API semaphore we would have to mirror.
   * **Gap 4 is also *conditional in C*.** C's own PI is a probe, not a
     guarantee: if `pthread_mutex_init` with `PTHREAD_PRIO_INHERIT` fails, C
     silently falls back to `PTHREAD_PRIO_NONE` (`posix/osdMutex.c:77-85`), and
     `DONT_USE_POSIX_THREAD_PRIORITY_SCHEDULING` `#undef`s the whole block
     (`:50-51`). Matching C therefore means matching the probe too, not
     asserting PI unconditionally.
   * **Gap 3's parity target is now UNVERIFIED.** With the score arm gone,
     nothing in the C source names `RTEMS_PRIORITY` for this mutex. The wait
     order C actually gets on RTEMS 6 is whatever RTEMS's POSIX-mutex thread
     queue does under `PTHREAD_PRIO_INHERIT`. That is a target-side question
     (§6), not one this worktree can answer.

**3. ⚠ corrected — the park_on-reachable tokio population is 20 in
`epics-base-rs`, not 14.** The evaluation's own row totals give 15
(L1 + L7 + L8×11 + L9 + L14 = 15, printed as "14"), and its sweep missed two
families that exist in this tree: `PvDatabase::registration_mutex`
(`database/mod.rs:288`, a `tokio::sync::Mutex<()>`) and the four
`tokio::sync::RwLock`s in `server/autosave/save_set.rs` (`:67`, `:68`, `:106`,
`:108`, import `:7`). The exhaustive re-sweep is §3; `registration_mutex`
matters because it is taken **inside** the L1-held region on the SCAN-put path
(`scan_index.rs:30`, called from `field_io.rs:731`/`:739`/`:1018`/`:1026`).

**4. ⚠ moved, and it is the best evidence in the whole file — L1 now has a
banded, park-blocking, high-priority waiter on the real target.** The
evaluation recorded periodic scan as an unbanded `JoinSet` tokio task. As of
this worktree's tip it is a dedicated `std::thread` per rate, named and banded
through `enter_ioc_thread` (`server/scan.rs:254-262`, band at `:48-50`,
`scan-10` → EPICS 60 … `scan-0.1` → EPICS 66), and on the exec backend its tick
is driven by `block_on_sync` (`scan.rs:121`) — i.e. `park_on` →
`std::thread::park()` (`runtime/task.rs:120` → `:92` → `:82`). That thread then
calls `process_record_with_links` (`scan.rs:166`), which takes L1
(`processing.rs:1295`).

   So the L1 contention pair is now concrete and both ends are banded:

   | side | thread | EPICS band | reaches L1 via |
   |---|---|---|---|
   | **high** | `scan-1` | 63 (`scan.rs:48-50`) | `scan.rs:121` `block_on_sync` → `scan.rs:166` → `processing.rs:1295` |
   | **low** | `CAS-client <n>` | 20 (`ThreadPriority::CaServerLow`, `task.rs:430`) | `ca .../blocking.rs:1283` `block_on_sync(serve_write_head)` → `ca .../tcp.rs:3792` → `:4305`/`:4316` → `field_io.rs:1392` |

   (`CAS-client <n>` is a **pooled worker**, not a thread created for one
   client: the CA server borrows each client's two threads from
   `CAS_CLIENT_POOL` and the name carries the worker's index, not the peer —
   `epics-ca-rs/src/server/blocking.rs`, `doc/rtems-connection-worker-pool-design.md`
   §9. The band is unchanged; it is now the `client` role's, applied once at
   worker creation. Target measurements taken before that conversion name the
   same thread `CAS-client-blocking <peer>`.)

   Both sit in `std::thread::park()` while waiting. Neither is visible to the
   kernel as waiting on an owned resource. This is no longer a hypothetical
   about a configuration that does not ship: handoff §5.9 records both bands
   **measured live on target** (`CAS-client` posix 76; `cbHigh` posix 128), and
   `scan-1` → posix 119 follows from the same `posix = 56 + epics` map
   (`task.rs:1120-1154`) though the scan threads themselves postdate that boot
   and are **UNVERIFIED on target**.

**5. There are two kinds of L1 waiter on the RTEMS backend, and only one of
them is a parked thread.** Blocking driver threads (`CAS-client`, `PVAS-conn`,
`scan-N`, `scanOnce`) park. Callback-band tails do not: the exec backend's
future executor releases the worker on `Poll::Pending` and re-enqueues on wake
(`runtime/background/future_exec.rs:14`, `:373-385`), so a cbMedium tail
waiting on L1 costs a queue slot, not a thread. Converting L1 to a blocking
lock changes that — see §2's cost row. It is not a regression against C, which
has the same property, but it must be stated rather than discovered.

---

## §1 Exhaustive audit — every `lock_record` / `lock_records` /
`OwnedMutexGuard` holder

**Anchor:** `rg -n "lock_record|lock_records|RecordWriteGuard|ManyRecordWriteGuard|OwnedMutexGuard|lock_owned" -g '*.rs' crates/`

**`OwnedMutexGuard` in production exists in exactly one module.** The only
production declarations are `record_lock.rs:112` (`RecordWriteGuard::_guard`)
and `:128` (`ManyRecordWriteGuard::_guards`), minted at `:148` and `:197`. The
only other `lock_owned()` calls in the workspace are
`epics-bridge-rs/src/qsrv/group.rs:5005` and `:5190`, both **inside** that
file's `#[cfg(test)] mod tests` (boundary at `:2899`) and both on
`atomic_write_lock`, a different lock. There is no `OwnedRwLockReadGuard` /
`OwnedRwLockWriteGuard` anywhere in the workspace.

Test-module boundaries used to separate production from test rows:
`record_lock.rs:203`, `field_io.rs:2240`, `qsrv/group.rs:2899`,
`pvalink/integration.rs:1924`; `processing.rs` has **no** `#[cfg(test)]`.

### §1.1 The nine production holders

"Awaits while holding" means: an `.await` lexically inside the guard's live
range. Line numbers are the guard's binding site and the awaits reached from it.

| # | holder | gate site | awaits while holding? | evidence |
|---|---|---|---|---|
| **H1** | `PvDatabase::put_pv_inner` | `field_io.rs:611` | **YES — 3** | `update_scan_index(…).await` `:731`, `:739`; `run_special_actions(…).await` `:749`. Guard is a function-scope `Option<RecordWriteGuard>`, live to the `return Ok(())` at `:762`. |
| **H2** | `PvDatabase::put_pv_and_post_with_origin` | `field_io.rs:895` | **YES — 3** | `update_scan_index(…).await` `:1018`, `:1026`; `run_special_actions(…).await` `:1032`. Guard live to the `return Ok(())` at `:1043`. Comment at `:892` states "Held until return." |
| **H3** | `PvDatabase::put_alarm_ack_from_ca` | `field_io.rs:1165` | **NO** | Body after the gate is `rec.write()` `:1167`, `check_put_disabled` `:1168`, one `put_ackt`/`put_acks` `:1170-1171`, `Ok(())` `:1173`. Zero `.await`. |
| **H4** | `PvDatabase::put_record_field_from_ca_inner` | `field_io.rs:1392` | **YES — 3+** | `find_subroutine_named(…).await` `:1438`; `put_driven_process_already_locked(…).await` `:1577`, `:1617`. The last of these re-enters the full processing chain (H6's body) under the same held gate. |
| **H5** | `PvDatabase::put_pv_no_process` | `field_io.rs:2198` | **NO** | Body after the gate is `rec.write()` `:2199` through the `return Ok(())` at `:2218`; the only call out is `notify_asg_field_changed()` `:2216`, a sync fn. Zero `.await`. |
| **H6** | `PvDatabase::process_record_with_links_inner` | `processing.rs:1295` | **YES — 37** | Function spans `:1229-3718`; the gate is live from `:1295` to the end. `rg -c '\.await'` over `1296..3718` = **37**. Named examples: `fetch_link` `:1434`, `check_simulation_mode` `:1655`, `schedule_delayed_reprocess` `:1710`, `complete_async_record` `:3347`. **This is the widest hold in the workspace.** |
| **H7** | `pvalink` atomic scan epoch | `epics-bridge-rs/src/pvalink/integration.rs:1060` | **YES — 3** | `scan_target_should_process(…).await` `:1075`; `process_record_with_links_already_locked(…).await` `:1101`; the epoch is explicitly dropped at the atomic→non-atomic boundary `:1069` and again at `:1111`. |
| **H8** | QSRV atomic group **GET** | `epics-bridge-rs/src/qsrv/group.rs:861` | **YES — 1** | `lock_group_records_read(…).await` `:863`. Everything after `:863` is synchronous (`rec.read()` collection `:874`, `read_member_locked` `:894`). |
| **H9** | QSRV atomic group **PUT** | `epics-bridge-rs/src/qsrv/group.rs:1722` | **YES — 4+** | `atomic_write_lock.lock().await` `:1728`; `convert_member_value(…).await` `:1762`; `post_process_member(…).await` `:1787`; `apply_member_value(…).await` `:1807`. |

**Totals: 9 production holders; 7 await while holding, 2 do not.**

### §1.2 The `_already_locked` family — holders by delegation

These take no gate themselves; they exist *because* the gate is not reentrant,
and they are the callee half of H4/H6/H7/H9. Any change to L1's type changes
their contract too, so they are named here rather than left implicit.

| entry | decl | called by |
|---|---|---|
| `put_record_field_from_ca_already_locked` | `field_io.rs:1098` | QSRV atomic PUT (H9) via `apply_member_value` |
| `put_record_field_from_ca_no_notify_already_locked` | `field_io.rs:1179` | same |
| `process_record_with_links_already_locked` — a `pub fn` returning a future, not an `async fn` | `processing.rs:724` | pvalink atomic scan (H7) `integration.rs:1101`; `field_io.rs:1316`; `processing.rs:594` |
| `put_driven_process_already_locked` | reached at `field_io.rs:1577`, `:1617` | H4, under its own held gate |

The reentrancy note is stated in three places already —
`field_io.rs:1094-1097`, `processing.rs:1288-1294`,
`qsrv/group.rs:1717-1720` — so the non-reentrancy of the gate is a documented,
load-bearing invariant, not an accident.

### §1.3 What the audit means for the conversion

`RecordWriteGuard` and `ManyRecordWriteGuard` are `Send` today because
`tokio::sync::OwnedMutexGuard<()>` is `Send`. Every one of H1, H2, H4, H6, H7,
H8, H9 is an `async fn` whose future is, directly or transitively, handed to
`tokio::spawn` on the hosted backend (the CA connection task,
`ca .../tcp.rs`; the PVA operation tasks). Making the guard wrap a
`parking_lot::MutexGuard` or a `pthread_mutex_t` guard makes it **`!Send`**,
which makes each of those futures `!Send`, which fails to compile at the spawn
site — *before* any question of deadlock arises.

That is the actual shape of the blocker, and it is why "count the awaits" is
the wrong measure. The seven holders do not need their awaits removed; they
need the gate-held region to stop being *inside* an async body.

---

## §2 The L1 decision — options, cost, recommendation

### Option A — convert the record gate to a blocking PI lock

`RecordLockRegistry::gates` maps names to `Arc<tokio::sync::Mutex<()>>`
(`record_lock.rs:85`, `:101`). Option A makes that
`Arc<PriorityInheritanceMutex<()>>` and makes `lock_record`/`lock_records`
synchronous (`fn`, not `async fn`).

**What must change, per holder.** The rule is: the gate-held region becomes a
synchronous function, and everything that must await moves outside it.

| holder | required restructure | size |
|---|---|---|
| **H3** `put_alarm_ack_from_ca` `field_io.rs:1165` | none — drop the `.await` on the gate. The body is already synchronous. | trivial |
| **H5** `put_pv_no_process` `field_io.rs:2198` | none — same. | trivial |
| **H1** `put_pv_inner` `field_io.rs:611` | Split at the existing seam. The gate-held work is already a scoped block (`:619-719`); the three awaits (`:731`, `:739`, `:749`) are *after* the record `RwLock` is released and are commented as such (`:615-618`, `:720-722`). Take the gate, run the block, **drop the gate**, then run the scan-index and special-action tails. **Semantic change:** the tails would no longer be inside the exclusion window. That must be signed off — see "the semantic question" below. | **M** |
| **H2** `put_pv_and_post_with_origin` `field_io.rs:895` | Identical shape to H1, same seam (`:901-1005` block, awaits at `:1018`/`:1026`/`:1032`). Its own comment at `:897-900` says the gate "still holds the processing-exclusion window across the whole helper" — that sentence is what Option A retracts. | **M** |
| **H4** `put_record_field_from_ca_inner` `field_io.rs:1392` | Hardest of the six. `find_subroutine_named` `:1438` is a lookup that can be hoisted **above** the gate (its result is only consumed inside). `put_driven_process_already_locked` `:1577`/`:1617` cannot: it is the process cycle, and it re-enters H6. Requires H6 first. | **L**, blocked on H6 |
| **H6** `process_record_with_links_inner` `processing.rs:1295` | The real work. 37 awaits over a 2,489-line body. **Not all 37 are suspensions**: most are nested acquisitions of the very tokio locks §3 also converts (`fetch_link` → L8/L3, `read_link_with_alarm` → L8). Once those are blocking, those call sites stop being `.await` at all. The genuinely suspending ones are the async-device paths — `schedule_delayed_reprocess` `:1710`, `complete_async_record` `:3347` — and **C does not hold `dbScanLock` across those either**: C's `dbProcess` sets `PACT` and *returns*, releasing the lock, and the device callback re-takes it. So the target shape is C's shape, not a novel one. | **XL** |
| **H7** pvalink atomic scan `integration.rs:1060` | `scan_target_should_process` `:1075` and the process call `:1101` both go under the epoch. Follows H6: once `process_record_with_links_already_locked` is sync-entered, the epoch loop is a sync loop. | **M**, blocked on H6 |
| **H8** QSRV atomic GET `group.rs:861` | One await (`lock_group_records_read` `:863`), and it does not need the gate held: `lock_group_records_read` (`:608-634`) only reads the records map and clones `Arc`s — it takes no per-record lock. Hoist it **above** the gate. Everything after `:863` is already synchronous. | **S** |
| **H9** QSRV atomic PUT `group.rs:1722` | `convert_member_value` `:1762` is the up-front conversion phase and is already documented (`:1724-1727`) as happening before the writes; hoist it above the gate together with `atomic_write_lock` `:1728`. `post_process_member` `:1787` / `apply_member_value` `:1807` are the member writes and must stay inside — they reach H4/H6 through the `_already_locked` entries, so they follow H6. | **L**, blocked on H6 |

**The semantic question Option A forces, and it needs sign-off.** For H1/H2,
today's gate covers the value write **and** the scan-index update **and** the
`special()` link writes. C's `dbScanLock` covers `dbPut` including
`dbPutSpecial(paddr, 1)` (`dbAccess.c`, cited at `field_io.rs:745-748`), so C
*does* hold the lock across the special-action link writes. Dropping the gate
before them is therefore a **deviation from C**, not merely a refactor. The
alternative — make `run_special_actions` and `update_scan_index` synchronous
too — is a larger change that reaches `registration_mutex` (§3) and the link
machinery. Recommendation: make them synchronous rather than shrink the window,
because shrinking it re-opens exactly the interleaving `lock_records` was added
to close (`record_lock.rs:54-58`).

**Costs Option A carries, stated rather than discovered:**

* **Band-worker occupancy.** §0 finding 5: a callback-band tail that blocks on
  a blocking L1 blocks its band worker thread and everything queued behind it,
  where today it only yields a queue slot. This is **parity with C** (a C
  callback thread blocking on `dbScanLock` blocks that band) and PI bounds it
  by the holder's critical section — but the band's effective concurrency drops
  from N-tails-per-worker to 1.
* **Deadlock surface.** A blocking lock cannot be released by yielding. The
  non-reentrancy invariant (`field_io.rs:1094-1097` et al.) stops being
  "deadlock the task" and starts being "deadlock the thread". The
  `_already_locked` family (§1.2) becomes safety-critical rather than
  merely correct.
* **Hosted behaviour changes too.** This is not an RTEMS-only edit;
  `record_lock.rs` is not `cfg`-gated. Every hosted CA/PVA write path takes the
  new lock.

### Option B — keep the async gate, withdraw the hard-RT write-path claim

Change nothing in `record_lock.rs`. Amend handoff §8.0's table so the "PI on
the scan lock" row reads *deliberate deviation, hard-RT write path not
claimed*, and say so in `record_lock.rs`'s module doc (which today presents the
gate as the `dbScanLock` analogue, `:42`, without qualification).

Cost: zero engineering, and the §0-finding-4 inversion stays. A `CAS-client` at
EPICS 20 holding L1 through a slow record write delays a `scan-1` at EPICS 63
for the full duration, with no bound and no kernel visibility, and any thread
between them preempts the holder freely.

### Option C — priority-ordered async wait, no PI

Replace `tokio::sync::Mutex` with a hand-written async gate whose waiter queue
is ordered by the waiter's declared EPICS band (carried in a thread-local set
by `enter_ioc_thread`). Fixes **gap 3** without touching the hold-across-await
contract. Does **not** fix gap 4: there is still no kernel-visible owner, so a
preempted low-priority holder is still not boosted.

Cost: a new synchronisation primitive to own and test. Value: it is the only
option that improves anything before H6 lands, and it composes with Option A
(the ordering logic is discarded when the lock becomes a PI pthread mutex,
which orders by priority in the kernel).

### Recommendation

**Option A, staged, with Option C as the bridge.** Two reasons, and only the
second is mine:

1. The user's standing directive for this work is that **PI must reach a
   C-equivalent implementation**. That settles A over B directly; B is recorded
   here only so the decision is legible, not as a live alternative.
2. Independently, §0 finding 2 makes A cheaper than the evaluation assumed: the
   parity target is a POSIX PI mutex, which `PriorityInheritanceMutex` already
   is on Linux (`sync.rs:26-27`, `:55-81`), so gap 4 is a cfg arm plus a type
   swap plus the restructure — not a new primitive.

Option C lands first because H6 is **XL** and everything else in Option A is
blocked on it. Without C, gap 3 stays fully open for however long H6 takes.

---

## §3 Gap 3 — per-lock disposition for the park_on-reachable tokio set

**Anchor:** `rg -n "tokio::sync::(Mutex|RwLock)<|runtime::sync::(Mutex|RwLock)" crates/epics-base-rs/src`
plus a per-file import resolution of every bare `Mutex<`/`RwLock<` field
declaration (the naming trap: `runtime/sync.rs:3` re-exports tokio's types, so
a bare `Mutex` means tokio in `pv.rs` and `database/mod.rs` but `std::sync` in
`scan.rs`, `event_queue.rs`, `callback_executor.rs`, `future_exec.rs`,
`scan_once.rs`, `delayed_timer.rs`, and `parking_lot` in every
`database/filters/*.rs`).

**Result: 20 tokio locks in `epics-base-rs`, not 14** (§0 finding 3). Every row
below was read in this worktree.

**Disposition vocabulary.** `PI` = becomes `PriorityInheritanceMutex` (or the
PI RwLock §5 step 3 must decide on). `ArcSwap` = remove the sharing; the state
is written at init and read forever. `async, bounded` = stays a tokio lock,
with the invalidating condition named in code. `async, off-target` = not
reachable from any banded RTEMS thread.

| # | lock | decl | reached from park_on by | disposition | note |
|---|---|---|---|---|---|
| **L1** | per-record advisory write gate | `record_lock.rs:85` map → `Arc<tokio::sync::Mutex<()>>` (`:72`, `:101`) | `scan-N` `scan.rs:121`→`:166`; `scanOnce`; `CAS-client` `ca blocking.rs:1244`; `PVAS-conn` `pva blocking.rs:976` | **PI** | §2. The whole design. |
| **L7** | `ProcessVariable::subscribers` | `pv.rs:325` `Mutex<Vec<Subscriber>>`, import `pv.rs:6` = tokio | `CAS-client` `ca blocking.rs:1216`, `:1334` (`block_on_sync(pv.remove_subscriber(…))`) | **PI** | Critical sections at `pv.rs:629`, `:776`, `:834`, `:842` are `Vec` push/retain; none contains an `.await`. Converts by type swap once the callers stop being `async fn` — same `!Send` constraint as §1.3. |
| **L8a** | `simple_pvs` | `database/mod.rs:231` | every put/get path, e.g. `field_io.rs:595`, `:877`, `:2182` | **PI** (mutex) | Read on the CA write hot path. **§5.3 addendum: decided — single `PriorityInheritanceMutex`; PI RwLock rejected (no POSIX protocol, no C analogue). Converts in step 4.** |
| **L8b** | `scan_index` | `database/mod.rs:246` | `scan-N` via `records_for_scan` (`scan.rs:163`); written by `update_scan_index` (`scan_index.rs:35`) | **PI** (mutex) | Both ends banded; the highest-contention L8 row. **§5.3 addendum: decided — one `PriorityInheritanceMutex` per `ScanList` bucket (C `scan_list.lock` shape). Converts in step 4.** |
| **L8c** | `load_order` | `database/mod.rs:252` | record add/remove | **ArcSwap** | Written at `mod.rs:1528` (`remove_record`) — **not** init-only, so the invalidating condition is runtime record removal. |
| **L8d** | `cp_links` | `database/mod.rs:258` | processing, every CP fan-out | **ArcSwap** | Writes at `links.rs:2596` (link registration, init) and `mod.rs:1533` (`remove_record`, runtime). Condition as L8c. |
| **L8e** | `external_cp_links` | `database/mod.rs:267` | same | **ArcSwap** | Write at `links.rs:2647`. |
| **L8f** | `external_resolver` | `database/mod.rs:305` | channel search | **ArcSwap** | Init-time write. |
| **L8g** | `search_resolver` | `database/mod.rs:307` | channel search (`CAS-UDP`, `PVAS-UDP`) | **ArcSwap** | Init-time write. |
| **L8h** | `existence_gate` | `database/mod.rs:311` | channel search | **ArcSwap** | Init-time write. |
| **L8i** | `link_sets` | `database/mod.rs:316` | processing — every pvalink resolution | **ArcSwap** | Write at `mod.rs:823` (`register_link_set`), init-time. |
| **L8j** | `subroutine_registry` | `database/mod.rs:341` | `field_io.rs:1438` (inside the L1 window, H4) | **ArcSwap** | Write at `mod.rs:688`, whole-registry replace — already `ArcSwap`-shaped. |
| **L8k** | `breaktable_registry` | `database/mod.rs:348` | conversion during processing | **ArcSwap** | Write at `mod.rs:652`; the value is already `Arc<…>`. |
| **L46** | `registration_mutex` ⚠ **new** | `database/mod.rs:288` `tokio::sync::Mutex<()>` | **inside the L1 window** — `scan_index.rs:30`, reached from `field_io.rs:731`/`:739`/`:1018`/`:1026`; also `mod.rs:650`, `:1082`, `:1147`, `:1175` | **PI** | Missed by the evaluation entirely. L1 → L46 is a nested tokio-on-tokio hold on the SCAN-put path; converting L1 without it leaves the inner hold async and the outer blocking, which is the worst of both. **Convert with L1, not after.** |
| **L9** | ACF config cell | `access_security.rs:171`, `:187`, `:203` `Arc<tokio::sync::RwLock<Option<AccessSecurityConfig>>>` | every access check from `CAS-client`/`PVAS-conn` | **ArcSwap** | Writers are `reload_acf_from`/`clear_acf` — operator actions. Read-mostly by construction, so removing the lock is cheaper and more honest than making it PI. |
| **L14** | autosave per-set gate | `autosave/manager.rs:82`, also `:309`, `:317` `Arc<tokio::sync::Mutex<()>>` | save-on-change driven from record processing | **remove-the-sharing** | Route save-on-change through the callback queue (a submission, not an acquisition), as the evaluation's §4 proposed. Unchanged by this design. |
| **L47a–d** | `SaveSet` status/backup ⚠ **new** | `autosave/save_set.rs:67`, `:68`, `:106`, `:108`, import `:7` = `tokio::sync::RwLock` | autosave worker only | **async, off-target** | Four locks the evaluation missed. **UNVERIFIED** whether autosave runs on the RTEMS target at all — neither RTEMS binary was traced for it in this panel. If it does, they inherit L14's disposition. |

**Cross-check on the L1 nesting.** With L1 blocking and L46 blocking, the hold
order on the SCAN-put path is L1 → L46 → L8b, all three PI. That is a
three-deep chain; PI walks it, but the chain must be **acquired in one order
everywhere** or it deadlocks a thread rather than a task. That ordering
invariant does not exist in the code today because none of the three can
deadlock a thread today. §5 step 4 owns writing it down.

**Not in this table, and why.** `epics-pva-rs` contributes exactly one tokio
lock (evaluation §8.1 L23, the ACF cell — the same cell as L9 here, reached
through `native_source.rs`), and `epics-bridge-rs` contributes 13 (evaluation
§9.1), of which only three (L31, L32-`record_cache`, L33) are in a subsystem
the RTEMS backend could reach. **L33** (`qsrv/group_config.rs:64`
`Arc<tokio::sync::Mutex<()>>`, the group-atomic-put gate, taken at
`group.rs:1728` inside H9's window) inherits L1's decision exactly and is
listed in §5 step 6 rather than re-derived here.

---

## §4 The `PriorityInheritanceMutex` RTEMS arm

### §4.1 What exists today

`runtime/sync.rs:26` gates the real implementation on
`all(target_os = "linux", feature = "linux-rt")`; `:32-33` is the
`parking_lot::Mutex` fallback for everything else; `:36-38`
`is_pi_mutex_active()` reports which. The Linux implementation
(`:41-115`) is a `pthread_mutex_t` in an `UnsafeCell` initialised with
`pthread_mutexattr_setprotocol(&attr, PTHREAD_PRIO_INHERIT)` (`:62`) — the
same construction as C's `posix/osdMutex.c:72`, minus the probe.

The feature is declared at `epics-base-rs/Cargo.toml:117` (`linux-rt = []`) and
enabled by no target, profile or default. Workspace-wide, the type has **zero
production call sites** (re-verified in this worktree: the only non-`sync.rs`
mentions are prose).

**So on the RTEMS target `PriorityInheritanceMutex<T>` is `parking_lot::Mutex<T>`
and `is_pi_mutex_active()` returns `false`.** Gap 4 cannot be closed without an
RTEMS arm regardless of what §2 decides about L1.

### §4.2 What an RTEMS PI pthread mutex needs

**VERIFIED — supplied to this panel from the target toolchain headers on the
RTEMS/QEMU box (2026-07-22); authority is
`doc/upstream-libc-rtems/pthread-prio-protocol.md` in the `main` checkout.**
That file is not committed on `main` at the time this document was written, so
the citation is forward-looking; the facts below are as supplied, not
re-derived here (this host has no RTEMS sysroot — see Provenance).

| what | status | source |
|---|---|---|
| `pthread_mutex_init` / `_destroy` / `_lock` / `_unlock`, `pthread_mutexattr_init` / `_destroy` | **present in `libc` for RTEMS** | patched libc `src/unix/mod.rs:1335-1349` — declared for all `unix`, so `armv7-rtems-eabihf` gets them. Verified in this panel. |
| `pthread_mutex_t` / `pthread_mutexattr_t`, at RTEMS widths | **present in `libc` for RTEMS** | patched libc `src/unix/newlib/mod.rs:283`, `:329`; sizes at `:392-393` — `__SIZEOF_PTHREAD_MUTEX_T = 64`, `__SIZEOF_PTHREAD_MUTEXATTR_T = 24`. Verified in this panel. |
| `pthread_mutexattr_setprotocol` | **ABSENT from `libc` for RTEMS** | patched libc declares it only for cygwin, qurt, teeos, aix and the `new/` musl/glibc/uclibc/posix modules — no newlib/rtems arm. Verified in this panel. |
| `PTHREAD_PRIO_NONE` / `_INHERIT` / `_PROTECT` constants | **ABSENT from `libc` for RTEMS** | patched libc defines them only for aix, vxworks, l4re, qurt, apple, hurd (+ linux). Verified in this panel. |
| the prototype **on the target** | **present** — target newlib `pthread.h:189-206` | box measurement |
| the constant **values on the target** | **`PTHREAD_PRIO_NONE = 0`, `PTHREAD_PRIO_INHERIT = 1`, `PTHREAD_PRIO_PROTECT = 2`** — `sys/_pthreadtypes.h:81-83` | box measurement |
| the feature macros | **unconditionally on for `__rtems__`** — `features.h:394-395` | box measurement |

**Conclusion: the symbols exist at link time; only the Rust declarations are
missing.** This is the identical situation `runtime::task` already solved for
`pthread_setschedparam`, which `libc`'s `newlib/rtems` module also does not
declare — the code declares its own `extern "C"` block locally
(`task.rs:999-1052`, with the struct-width `const _` assertion at `:1036-1039`
and the "absent from `libc`'s `newlib/rtems` module" note at `:1048-1049`).

### §4.3 Two ways to get the declarations, and the recommendation

**(i) Local `extern "C"` block in `epics-rs`** — a `rtems_pi` module in
`runtime/sync.rs` mirroring `rtems_sched`: declare
`pthread_mutexattr_setprotocol(attr: *mut libc::pthread_mutexattr_t, protocol: c_int) -> c_int`
and `const PTHREAD_PRIO_INHERIT: c_int = 1;`, reusing `libc`'s
`pthread_mutexattr_t` (whose RTEMS width is already right, §4.2 row 2) and
`libc`'s `pthread_mutex_*` functions (already declared, row 1).

**(ii) Add the arm to the libc fork** — extend
`src/unix/newlib/rtems/mod.rs` with the prototype and the three constants, then
upstream it alongside the two PRs the fork already carries
(`Cargo.toml:66-70`).

**Recommendation: (i) now, (ii) as the upstream follow-up.** Reasons:

* (i) needs **no** change to the pinned libc rev, so it does not disturb the
  `-Zbuild-std` invariant that `std` and our code compile against the same
  `libc` (`Cargo.toml:56-65`) — the risk the two build-refusing asserts in
  `epics-rtems-boot` exist to catch.
* (i) is precedented in this exact file family and its precedent is documented
  as deliberate (`task.rs:1048-1049`).
* It only declares a function and a constant. There is no struct-layout
  exposure: `pthread_mutexattr_t` comes from `libc` and its RTEMS width is
  already asserted by the fork, so (i) cannot reintroduce the class of defect
  the `timespec`/`sockaddr` patches exist for.
* (ii) is the right *end state* — RTEMS supports the protocol and every other
  POSIX target in `libc` declares it — but it gates our work on a fork edit and
  an upstream cycle for a two-line win. File it, do not wait on it.

### §4.4 The cfg arm, stated

The gate at `sync.rs:26` and `:32` becomes a three-way split, and the RTEMS arm
must **not** be behind `feature = "linux-rt"` (which is Linux-named and
Linux-defaulted):

* `all(target_os = "linux", feature = "linux-rt")` → the existing `pi_mutex`.
* `target_os = "rtems"` → the RTEMS `pi_mutex`, **unconditional**, matching the
  precedent that `DEFAULT_POLICY` is `AllowRealtime` on RTEMS
  (`task.rs:591-603`) because the target has no way to set an env var and no
  privilege gate to fail.
* everything else → `parking_lot::Mutex`.

`is_pi_mutex_active()` (`sync.rs:36-38`) must be updated in the same edit or it
reports `false` on the one target where PI is now live. Its call-site count
today is zero; §5 step 7 gives it one.

**Two things the RTEMS arm must do that the Linux arm does not:**

1. **Probe and fall back, as C does.** `posix/osdMutex.c:77-85` builds a
   temporary mutex with the PI attribute and, on failure, downgrades both
   global attributes to `PTHREAD_PRIO_NONE`. Our Linux arm instead
   `assert_eq!`s (`sync.rs:63`, `:65`) — i.e. it panics where C degrades. On a
   target with no console, no iocsh and no `tracing` subscriber for most of
   boot, a panic in a lock constructor is the worst available failure mode.
   The RTEMS arm should probe once and fall back, and report which it got.
2. **Report it at startup, as C does.** `epicsMutexShowAll` prints
   "PI is/is not enabled" (`posix/osdMutex.c:199-205`). We have no iocsh on
   target (memory: the RTEMS IOC installs no tracing subscriber for the
   closure), so this must be an `eprintln!`-class line from the boot path or it
   is unobservable.

---

## §5 Staged execution plan

Ordered as `doc/pi-lock-evaluation.md` §6 is: by (blocker for a hard-RT claim)
× (cost). Each step is scoped to be brief-able as a single panel task and
carries a **done-check** that is a command or an observation, not a judgement.

| # | step | why here | size | done-check |
|---|---|---|---|---|
| **1** | **RTEMS arm for `PriorityInheritanceMutex`.** §4: local `extern "C"` `rtems_pi` module in `runtime/sync.rs`, three-way cfg (§4.4), probe-and-fall-back (§4.3 note 1), `is_pi_mutex_active()` updated. No call sites added. | Nothing below can be verified on target until the type is real there. Independent of the L1 decision, so it can start immediately. | **S** | `./scripts/rtems-check.sh` exit 0 **and** a target boot prints the PI line. On host: `cargo nextest run -p epics-base-rs runtime::sync` green with the existing two tests, plus a new test asserting `is_pi_mutex_active()` matches the cfg on each arm. |
| **2** | **Option C — priority-ordered async gate for L1.** Replace `tokio::sync::Mutex<()>` in `RecordLockRegistry` (`record_lock.rs:85`, `:101`) with a gate whose waiter queue orders by the waiter's EPICS band, read from a thread-local set by `enter_ioc_thread`. **API unchanged** — still `async fn lock_record`, still hands out a `Send` guard, so **zero callers change**. | Closes **gap 3** for the one lock that matters, without waiting on the XL step 5. It is the only step that improves anything before H6 lands. | **M** | A test in `record_lock.rs` that parks three waiters at bands 20/63/70 on a held gate, releases it, and asserts wake order 70, 63, 20 — failing on `main` (which is FIFO), passing after. The five existing tests at `:203-332` stay green unchanged. |
| **3** | **Decide the PI RwLock question, and remove the six ArcSwap rows.** `PriorityInheritanceMutex` is mutex-only (`sync.rs:27`); L8a/L8b are `RwLock`s. Either add a PI RwLock or demote them, with the read-concurrency cost stated. In the same change convert L8c–L8k (`ArcSwap`/`OnceLock`, §3) — that removes **nine** rows from the table rather than reclassifying them. **DECIDED — see §5.3 addendum:** demote both to `PriorityInheritanceMutex` (L8b sharded per `ScanList`); the PI-RwLock option is rejected with evidence; the *conversion* of L8a/L8b moves to step 4 because it cannot land before step 1 and must land with L46. | Shrinks the surface steps 4–6 apply to, and answers a question step 4 would otherwise be blocked on. Structural: it deletes locks. | **M** | `rg -n "runtime::sync::RwLock" crates/epics-base-rs/src/server/database/mod.rs` returns **2** rows (L8a, L8b), down from 11. `cargo nextest run -p epics-base-rs` green. |
| **4** | **L1 + L46 → PI, and write down the acquisition order.** Convert `RecordLockRegistry::gates` (`record_lock.rs:85`), `registration_mutex` (`database/mod.rs:288`) **and — per the §5.3 addendum — L8a `simple_pvs` (`:231`) and L8b `scan_index` (`:246`)** **together** (§3 cross-check), make `lock_record`/`lock_records` synchronous, and convert the two free holders **H3** (`field_io.rs:1165`) and **H5** (`field_io.rs:2198`). State the L1 → L46 → L8b order as a MUST rule in `record_lock.rs`'s module doc, with the single owner named. | The gate type change is the point of the whole exercise; H3/H5 prove the new API before the hard holders touch it. | **M** | `cargo nextest run -p epics-base-rs -p epics-bridge-rs` green. `is_pi_mutex_active()` true on the RTEMS build. A test that a second `lock_record` on a held gate from another thread does not return until release (the blocking analogue of `lock_record_excludes_same_record`, `record_lock.rs:211`). |
| **5** | **H6 — `process_record_with_links_inner`.** The XL step. Lift the L1-held region out of the async body: after step 3 most of the 37 awaits are no longer awaits, and the genuinely suspending ones (`:1710`, `:3347`) must move outside the window — which is what C does, `dbProcess` releasing at `PACT`. | Every remaining holder is blocked on this one. It is also the only step where the port can end up *wrong* rather than merely unconverted. | **XL** | `process_record_with_links` compiles as `fn`, not `async fn`. `cargo nextest run --workspace` green — in particular `epics-base-rs/tests/database_tests.rs:8793-8865` (the two `lock_records`-epoch exclusion tests) unchanged and passing. `./scripts/rtems-check.sh` exit 0. |
| **6** | **The remaining holders, in dependency order.** **H8** (`group.rs:861`, hoist `lock_group_records_read` above the gate — **S**, independent of H6, can be done any time after step 4). Then **H1**/**H2** (`field_io.rs:611`/`:895`, with the §2 semantic decision signed off). Then **H4** (`field_io.rs:1392`), **H7** (`integration.rs:1060`), **H9** (`group.rs:1722`) — all blocked on step 5. Then **L33** (`qsrv/group_config.rs:64`), which inherits L1's decision verbatim. | Mechanical once step 5 lands; splitting them keeps each panel task reviewable. | **L** total | `rg -n "OwnedMutexGuard" crates/` returns **zero** production hits. `cargo nextest run --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` green. |
| **7** | **Make it reachable and measurable.** Wire the startup report (§4.4 note 2) — `is_pi_mutex_active()` gets its first caller. Add an on-target latency regression: hold L1 from a `CAS-client`-band thread, contend from a `scan-N`-band thread, run a mid-band thread in between, and fail when the high-priority waiter is delayed beyond a stated bound. Re-measure the thread census on target (the §0-finding-4 `scan-N` bands are UNVERIFIED). | Without it, steps 1–6 are unfalsifiable and §8.0 gaps 3/4 cannot be *closed*, only claimed. | **M** | The regression **fails** on a build with the PI arm disabled and **passes** with it enabled — both runs on target, both recorded, as `doc/rtems-priority-on-target-measurement.md` records the band readback. |

### §5.3 addendum — the PI RwLock decision (step-3 panel, worktree `VVK0HDQKKZ-pi-step3-arcswap-2013e606-1` @ `d58a31b3`)

Written by the panel that executed step 3. Everything below was read in that
worktree; the C citations are `/home/stevek/work/epics-base` @ `669a25697`, the
same reference this document's Provenance names.

**Decision.** L8a `simple_pvs` and L8b `scan_index` become
`PriorityInheritanceMutex`, **not** a reader-writer lock and **not** `ArcSwap`.
L8b becomes **one PI mutex per `ScanList` bucket**, not one lock over the map.
The conversion itself is **deferred to step 4** and is listed there, not done in
step 3 — see "why not now" below. The step-3 panel converted only L8c–L8k and
L9.

#### Option 1 — add a PI RwLock. **REJECTED.**

*There is no priority protocol for a pthread rwlock, on any POSIX platform.*

* glibc `/usr/include/pthread.h` declares the entire `pthread_rwlockattr_*`
  family and it is six functions: `_init` `:1078`, `_destroy` `:1082`,
  `_getpshared` `:1086`, `_setpshared` `:1092`, `_getkind_np` `:1097`,
  `_setkind_np` `:1103`. There is no `_setprotocol`. By contrast
  `pthread_mutexattr_setprotocol` is declared at `:913` and
  `PTHREAD_PRIO_NONE`/`_INHERIT`/`_PROTECT` at `:84`.
  `rg -n 'rwlockattr_setprotocol' /usr/include/` returns **zero hits**.
* The pinned libc fork declares `pthread_rwlockattr_init` /
  `pthread_rwlockattr_destroy` and nothing else, for all `unix`
  (`src/unix/mod.rs:1422-1423`). So §4.3's escape hatch (i) — "declare it
  locally, the symbol exists at link time" — has nothing to declare: the
  function does not exist in any libc, it is not merely undeclared in Rust.
* This is structural, not an API oversight. PI works by boosting **the owner**.
  A read-held rwlock has N owners and the kernel tracks none of them, so there
  is no thread to boost. That is why POSIX defines the protocol on mutexes
  only.
* **UNVERIFIED on the RTEMS target.** This host has no RTEMS sysroot
  (Provenance), so the target's newlib `pthread.h` was not read; the claim
  asserted here is the POSIX-level and libc-level one. It does not change the
  decision, because option 1 also fails the parity test below independently.

#### The parity test — C has no reader-writer lock anywhere

* `rg -n 'pthread_rwlock' /home/stevek/work/epics-base/modules/` → **zero
  hits**. `modules/libcom/src/osi/` ships `epicsMutex`, `epicsEvent` and
  `epicsSpin`; there is no reader-writer primitive in the C IOC at all.
* Both of our two rows have a named C analogue and **both are `epicsMutex`**:

  | our lock | C analogue | C lock | taken for read at | taken for write at |
  |---|---|---|---|---|
  | **L8a** `simple_pvs` | process-variable directory, `dbPvdLib.c` | `epicsMutexId lock` per hash bucket, `:30`, created `:119` | `dbPvdFind` `:123-136` | `dbPvdAdd` `:150-162` |
  | **L8b** `scan_index` | `scan_list.lock`, `dbScan.c:75`, created `:527`, `:604`, `:908` | `epicsMutexId` | `scanList` `:1007-1051` | `addToList` / `deleteFromList` `:1082-1123` |

  On RTEMS 6 both are POSIX PI mutexes (§0 finding 2). So "demote to a PI
  mutex" is not a downgrade against C — it **is** C's construction.

#### Read-concurrency cost, quantified

**L8a `simple_pvs` — 14 production read sites**, all reached from banded
threads: `field_io.rs:493`, `:595`, `:790`, `:804`, `:826`, `:851`, `:877`,
`:2182`; `mod.rs:801`, `:1473`, `:1565`, `:1645`, `:1739`, `:1853`.

* *Who reads concurrently:* one `CAS-client <n>` pooled worker per live CA TCP
  connection, one `PVAS-conn` per PVA connection, the UDP search responders via
  `has_name_from` (`mod.rs:1645`), and the scan/callback bands through the put
  paths. On the RTEMS target that population is bounded at **142** — the
  measured fd wall, not a stack estimate, and not the lock.

  *(This bullet used to say "single digits … ~5 CA / ~3 PVA connections",
  derived from a 2 MiB-per-thread stack ceiling. Both halves were wrong.
  **2 MiB is the 64-bit host's `Big` class**: `StackSizeClass::bytes` is
  `f × 0x10000 × size_of::<usize>()` (`task.rs:695-703`), so on
  `armv7-rtems-eabihf`, where `usize` is 4 bytes, the classes are
  **256 KiB / 512 KiB / 1 MiB** — a CA connection's `Big` + `Medium` and a PVA
  connection's `Big` + `Small` + `Small` are each 1,572,864 B, not ~4–6 MiB.
  And the binding ceiling is not stacks at all: 150 configured descriptors
  minus the 8 the IOC holds at idle gives **142** concurrent connections, with
  client #143 refused by `accept` with `ENFILE`, while the memory wall sits at
  151 and was only reachable on an image with the fd cap raised to 400 —
  measured, `doc/rtems-fd-ceiling-deviation.md` §2–§3, and the derivation the
  CA worker pool's capacity was set from,
  `doc/rtems-connection-worker-pool-design.md` §9.)*
* *Critical section:* one `HashMap::get` plus an `Arc::clone` at 13 of the 14
  sites. The exception is `all_simple_pv_names` (`mod.rs:1853`), which clones
  every key and is reached only from iocsh dumps.
* *Cost of demotion:* ≤ 142 threads serialise on an O(1) section — the
  corrected bound above, not the ~10 this line carried while it inherited the
  stack estimate. C serialises
  the same lookup at a **finer** grain (per hash bucket) than our single
  map-wide lock, so the port is already coarser than C here and the demotion
  does not make it coarser still. **Accepted.**

**L8b `scan_index` — read through `records_for_scan` (`scan_index.rs:93`).**

* *Who reads concurrently:* one periodic thread per rate, at most 7 (`scan-10`
  … `scan-0.1`, bands 60–66, `scan.rs:44-50`), calling from `scan.rs:163`; plus
  `scan_event.rs:123`, the I/O-Intr sweep (`ioc_app.rs:1419`) and iocsh
  (`commands.rs:633`, `:643`, `:986`, `:989`). Each reads **once per tick**
  (10 s … 0.1 s).
* *Critical section:* `get(&list)` plus a clone of that bucket's names into a
  `Vec<String>` — O(n) in one scan list, once per tick, released **before** the
  processing loop. C's `scanList` (`dbScan.c:1007-1051`) does the same thing
  differently: it holds `psl->lock` only for the cursor step and releases it
  around every `dbProcess`. Neither holds its lock across processing.
* *The real cost, and it is not a PI question:* C has **one lock per scan
  list**; we have **one lock over the whole index map**. Demoting to a single
  map-wide mutex would serialise 7 periodic threads that C never serialises
  against each other — a structural mismatch we would be introducing, on the
  one path where both ends are banded (§3 calls L8b "the highest-contention L8
  row"). **Therefore L8b converts to one `PriorityInheritanceMutex` per
  `ScanList` bucket**, which is C's shape and removes the cross-rate contention
  entirely. L8a stays a single lock because its section is O(1); sharding it
  the way `dbPvdLib` shards is a measured follow-up, not a prerequisite.

#### Why not `ArcSwap`, the disposition the other nine rows get

Both are read-mostly, but both have runtime writers whose cost under a
whole-snapshot rebuild is quadratic:

* `simple_pvs` writers are `add_pv` (`mod.rs:1082`), `add_pv_with_hooks_full`
  (`:1147`) and `remove_simple_pv` (`:1175`). The CA gateway registers a shadow
  simple PV **per client search**, at runtime — so this is not an init-only
  cell, and a full-map rebuild per registration is O(n²) over gateway warm-up.
* `scan_index` writers are `update_scan_index` (`scan_index.rs:30`) on every
  SCAN/PHAS put, plus `add_loaded_record` (`mod.rs:1455`) and `remove_record`
  (`:1523`). `add_loaded_record` runs once per record at load, so a rebuild per
  add is O(n²) over database load.

That write cost is exactly why §3 puts these two rows in the PI column and the
other nine in the ArcSwap column. **Rejected.**

#### Why the conversion is not done in step 3 — three blockers, all in step 4's scope

1. **`PriorityInheritanceMutex` has no RTEMS arm until step 1** (§4.1: on
   RTEMS it is `parking_lot::Mutex` and `is_pi_mutex_active()` is `false`).
   Converting first spends the read concurrency on the target and buys no PI.
2. **Two read sites hold the `simple_pvs` guard across an `.await`** —
   `field_io.rs:595` and `:2182`, both `if let Some(pv) = …read().await.get(name)
   { pv.set(value).await; … }`, where `pv` borrows out of the guard so the guard
   is live across the await. A `parking_lot`/pthread guard there is `!Send`,
   which makes those `async fn`s `!Send` futures and fails at the `tokio::spawn`
   sites — §1.3's blocker, verbatim. Fixable with `.cloned()` and an explicit
   drop, but that is a call-site edit, not a type swap.
3. **The nesting rule is L46's.** Every `scan_index` write is taken *inside*
   `registration_mutex` (`scan_index.rs:30`, `mod.rs:1365`, `:1503`), and
   `simple_pvs`' writes likewise (`mod.rs:1082`, `:1147`, `:1175`). Converting
   L8a/L8b while L46 stays tokio nests a blocking lock inside an async one on
   the SCAN-put path — the combination §3's cross-check already names "the worst
   of both". They must convert **with** L1 + L46, under the one written
   acquisition order step 4 owns.

So the target type is fixed here and the edit lands in step 4. Step 4's row and
done-check should be read as covering L8a and L8b as well.

#### Step-3 done-check, as executed

`rg -n "runtime::sync::RwLock" crates/epics-base-rs/src/server/database/mod.rs`
returns **four** rows, and every one of them is L8a or L8b: the two field
declarations (`simple_pvs` `:233`, `scan_index` `:248`) and their two
constructor calls (`:645`, `:651`). It returned **one** row before — the bare
`use crate::runtime::sync::RwLock;` import — which is §3's own naming trap and
told you nothing about how many tokio locks the file declares. The import is
gone and both survivors are spelled in full, so the check now reads the code
rather than the import list. Nine rows (L8c–L8k) and L9 are converted; the
remaining eleven `RwLock`/`Mutex` occurrences in the file are
`parking_lot`/`std`, which were never in scope.

### §5.4 addendum — what the L1 flip cost on the hosted backend (measured)

Written by the panel that executed steps 5–6's type flip (L1, L33, L7). It is
the first *measured* number anywhere in this document; §6's "nothing was
measured" still stands for every claim about the target.

**Host.** 96-core Linux, shared, load average 2–7 during the runs. Default
`cargo` build, so `PriorityInheritanceMutex` is its `parking_lot::Mutex` arm:
this measures blocking-vs-async, **not** PI-vs-no-PI, and the box has no RT
policy to invert.

**Load.** `PvDatabase::put_record_field_from_ca` — one `dbScanLock`/`dbPut`/
`dbScanUnlock` bracket per call — driven from N tokio tasks on an 8-worker
multi-threaded runtime, 160 000 puts per measurement, three runs per point,
medians below. `contended` = every task writes one `ai` record; `disjoint` =
one record per task. The harness compiles unchanged against both commits
(`put_record_field_from_ca` is an `async fn` on both), so the only difference
is the gate's internals.

| writers | contended, `a00d90ba` (async gate) | contended, after the flip | Δ |
|---|---|---|---|
| 1 | 356 k put/s | 373 k put/s | **+4.8 %** |
| 2 | 168 k put/s | 239 k put/s | **+42 %** |
| 4 | 168 k put/s | 71 k put/s | **−58 %** |
| 8 | 189 k put/s | 68 k put/s | **−64 %** |

`disjoint` shows no attributable change at any width (medians 355–580 k before,
374–440 k after; run-to-run spread on this host exceeds the difference).
`epics-ca-rs`' `e2e_caput_warm` — the heaviest existing end-to-end put
benchmark — cannot resolve a 10 % effect here (251–325 µs before, 252–517 µs
after, overlapping) and in any case its IOC serves *simple PVs*, which take no
record gate at all, so it is reported only as evidence that nothing gross
happened end to end.

**The −64 % is real and is the cost of the design, not a defect.** The shape
says where it comes from: the flip is *faster* uncontended and at two writers,
and only turns over once the number of blocked waiters exceeds a couple. A
blocked waiter is now an OS thread parked in the mutex — one futex sleep/wake
and one context switch per handoff — where the async gate could hand ownership
to another task on a worker's own run queue and never enter the kernel. This is
precisely the trade §2 Option A was chosen on: only a blocking lock has a
kernel-visible owner to inherit priority from, and C's `dbScanLock` is that
blocking lock.

Three bounds on how much it should worry a reader:

* It needs ≥4 writers on **one** record with no think time between puts. Spread
  the same load across records and it disappears.
* It is a *hosted-executor* pathology. The RTEMS backend has no tokio worker
  pool on the CA/PVA paths — a blocked CA thread there is just a blocked CA
  thread, which is what C does.
* The harness's per-put work is a bare `VAL` write, so lock overhead is ~100 %
  of it. A put that converts, posts monitors and drives links dilutes it.

**Not established:** whether the same shape holds on target, where the waiters
are banded threads and the mutex is a real PI mutex. That belongs to step 7
alongside the latency regression, and until it is run, the target-side cost of
this flip is unmeasured in both directions.

---

**Ordering note.** Steps 1, 2 and 3 are mutually independent and can run as
three parallel panels. Step 4 needs 1 and 3. Step 5 needs 4. Step 6's H8 needs
only 4; the rest need 5. Step 7 needs 4 at minimum and is only meaningful after
6.

**What is deliberately *not* in this plan.** The evaluation's §6 steps 2, 3 and
4 (finish the priority sweep; L12 → PI; L2/L11/L6/L3 → PI) and §9.4's L34/L35
are all still owed and are not superseded by anything here. They are omitted
because this document's scope is gaps 3 and 4, and folding them in would hide
the dependency structure above. Step 2 of the evaluation's list is now
**partly** closed by this worktree's tip — periodic scan is banded
(`scan.rs:254-262`) — with `CAS-UDP` and iocsh still per handoff §5.9's census.

---

## §6 What this design does not establish

* **Nothing was measured *on target*.** Static analysis plus the two supplied
  target-header facts. Every latency claim is structural. §5.4 adds one hosted
  throughput measurement of the L1 flip; it says nothing about the target,
  where the lock is a real PI mutex and the waiters are banded threads.
* **The `scan-N` bands are UNVERIFIED on target.** They postdate the
  2026-07-22 boot recorded in `doc/rtems-priority-on-target-measurement.md`.
  The predicted values (`scan-1` → posix 119) follow from `task.rs:1120-1154`
  but no boot has read them back.
* **The wait-order parity target is UNVERIFIED.** §0 finding 2 removes
  `RTEMS_PRIORITY` as the answer; what RTEMS's POSIX mutex queue actually does
  under `PTHREAD_PRIO_INHERIT` was not determined. Until it is, "priority-ordered
  wait" is a goal stated against C's *intent*, not against a measured C
  behaviour. **This is the single most load-bearing unverified fact in this
  document** and it belongs in step 7.
* **Whether `epics-base-rs` autosave runs on the RTEMS target was not traced**,
  so L47a–d's disposition (§3) is conditional.
* **Whether every H1–H9 future is genuinely `tokio::spawn`ed on the hosted
  backend was verified for the CA connection path only** (`ca .../tcp.rs`
  through `blocking.rs:1244`); the PVA operation paths were read for
  `block_on_sync` sites (`pva .../blocking.rs:976`, `:1476`) but not traced to
  a spawn. §1.3's `!Send` argument holds for the CA path with certainty and for
  the PVA path by strong inference.
* **No RTEMS build was run.** `./scripts/rtems-check.sh` is red on the
  committed tree for the reason handoff §8.1 item 3 records (stock `libc`
  cannot build a bootable image), and this document changes no code, so it was
  not run.
