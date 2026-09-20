//! Database-level record locking — the Rust counterpart of the
//! C-EPICS `dbScanLock` / `dbScanLockMany` machinery and pvxs's
//! `ioc::DBManyLock` / `ioc::DBManyLocker`.
//!
//! C EPICS / pvxs background
//! -------------------------
//! Every `dbPutField` / `dbProcess` in C EPICS takes the mutex of the
//! target record's *lock set* via `dbScanLock(precord)`: `dbLock.c:187`
//! reads `precord->lset`, and `:192`/`:196` resolve it to that record's
//! current `lockSet` and lock the single `ls->lock` that `makeSet` gave
//! the set (`:86`). There is no `dbCommon::lock` member, and
//! `precord->mlok` is a different mutex — `dbEvent.c:123-124`'s
//! `LOCKREC`/`UNLOCKREC` over the record's event list, created at
//! `iocInit.c:516` (the review's `R7.0.10` pin; this machine's checkout
//! carries one extra line at `:188`, so the same statement reads `:517`
//! against the working tree). A multi-record transaction — a QSRV *atomic group*
//! operation, or a pvalink *atomic* scan-on-update set — must apply,
//! read or scan several records as one indivisible unit, so pvxs
//! builds a `DBManyLock` over every member record and holds a
//! `DBManyLocker` across the whole member loop:
//!
//! * `epics-base/modules/database/src/ioc/db/dbLock.c:349` —
//!   `dbLockerAlloc` builds a locker over a fixed record set.
//! * `epics-base/modules/database/src/ioc/db/dbLock.c:384` —
//!   `dbScanLockMany` sorts the lock sets and acquires every one,
//!   skipping duplicates.
//! * `pvxs/ioc/groupconfigprocessor.cpp:1165` `initialiseDbLocker` /
//!   `pvxs/ioc/groupsource.cpp:492,621` — atomic group GET/PUT.
//! * `pvxs/ioc/pvalink_channel.cpp:409,423` — `DBManyLock` /
//!   `DBManyLocker` over the atomic pvalink scan-target records.
//!
//! `DBManyLock` locks the member records in a deadlock-free canonical
//! order (`dbLock.c` sorts the lock set), and because those are the
//! *same* mutexes a plain `dbPutField` takes, a direct CA/PVA write
//! to a backing member record cannot interleave with the transaction.
//!
//! Rust port
//! ---------
//! `epics-base-rs` stores each record in a `RecordCell`, and the ONLY lock
//! over its data is the record's lock set: `RecordCell::read` / `write` take
//! the set (recursively, as `dbScanLock` does) and hand out a `RefCell`-style
//! borrow that panics on same-thread aliasing. There is no second lock on the
//! data — the `parking_lot::RwLock<RecordInstance>` an earlier port kept
//! under the set cost a second acquisition on every field access and could
//! wedge a thread that re-entered it while the set already serialised the
//! access.
//!
//! This module is C's **lock sets**: one mutex per connected component of
//! the DB-link graph, with the record-to-set map C keeps in
//! `dbCommon::lset`. That cell is [`LockRecord`], and a record is born with
//! one pointing at the **bootstrap set** ([`bootstrap_set`], id 0, never on
//! the active or free list) — C's null `lset` made lockable — until the
//! registry adopts the cell at registration ([`Registry::adopt`]) and moves
//! it onto a set of its own ([`Registry::mint_for`]). After `iocInit` the
//! two happen together: a record the registry adopts once
//! [`PvDatabase::build_lock_sets`] has run is minted a set before it is
//! published, so no registered record is ever on the bootstrap set then and
//! no set-taker after `iocInit` needs to reach for id 0.
//!
//! * A plain CA/PVA write (`put_record_field_from_ca`, `put_pv`,
//!   `process_record`) takes the target record's set for the duration of the
//!   write via [`PvDatabase::lock_record`] — `dbScanLock`.
//! * A multi-record transaction — the QSRV atomic group PUT/GET and the
//!   pvalink atomic scan-on-update epoch — takes every set its members are
//!   behind, up front and in set-id order, via [`PvDatabase::lock_records`] —
//!   `dbScanLockMany`, including its skip of the duplicates that appear when
//!   several members share one set.
//!
//! Because every path resolves through the same registry, a direct
//! backing-record write blocks until the transaction owning that record
//! finishes, a QSRV atomic group PUT and a pvalink atomic scan can never
//! interleave on a shared record, and — this is what the per-record gate could
//! not do — two records a link joins are serialised against each other exactly
//! as C serialises them.
//!
//! **Rule R — a cell moves only under its set.** A [`LockRecord`] is
//! re-pointed at another set only by a thread holding the set it currently
//! points at (see [`Registry::place`]), which is what makes `set(); acquire;
//! re-check set()` in [`LockRecord::acquire`] a stable hold: once the set is
//! held, the record cannot leave it. Movers take the sets first — in id
//! order through [`hold_sorted`] — and the registry mutex under them, and
//! re-verify [`Registry::revision`] before moving anything.
//!
//! A link-field put takes both ends up front — the record and the local
//! target its new text names — as C's `dbPutFieldLink` does through
//! `dbScanLockMany`, and the relink runs inside that window
//! ([`RelinkScope::LinkEdit`]). A change of membership (a record added,
//! removed, or re-aliased after `iocInit`) relinks over every set with
//! nothing held on entry ([`RelinkScope::Membership`]).
//!
//! ### How a set is built, and what still is not
//!
//! One `lockSet` carries a single `epicsMutexId lock` and an `ELLLIST
//! lockRecordList` of the records behind it (`dbLockPvt.h:29-44`, the list at
//! `:31`, the mutex at `:32`), and `dbCommon.LSET` is a `lockRecord *` pointing
//! into that set (`:48`, `:53-70`). Membership is not static: creating a DB
//! link merges the two records' sets (`dbDbLink.c:110` and `:124`, both
//! `dbLockSetMerge`) and removing one splits them again (`:141`
//! `dbLockSetSplit`), so a record and everything its links reach sit behind
//! ONE mutex.
//!
//! [`PvDatabase::build_lock_sets`] reproduces both halves of C's construction
//! in the order C performs them — `dbLockInitRecords` gives every record its
//! own set, then one merge per DB link (`iocInit.c:178-179`) — which is why
//! `dbLockShowLocked` reports `0` and `0` before `iocInit` and why the sets a
//! merge empties show up on the free list rather than vanishing.
//!
//! ### The runtime relink, and who owns it
//!
//! **Invariant.** The partition the registry holds MUST equal the connected
//! components of the DB-link graph over the records the database currently
//! has. No path may leave a link field written and the partition unchanged.
//!
//! **Owner.** [`PvDatabase::relink_lock_sets`] is the only mutator of the
//! partition after [`PvDatabase::build_lock_sets`], and it is private:
//! nothing outside this module can call it. The only way to reach it is to
//! hold a [`LockSetEdit`], whose destructor calls it — so a link-field
//! write that does not relink is not something a caller can forget to do,
//! it is something they cannot express. Every exit path of the write body
//! (`?`, an early `return`, a panic unwind) drops the token and relinks.
//!
//! **Why a re-partition and not C's incremental pair.** C calls
//! `dbLockSetMerge` on link creation and `dbLockSetSplit` on removal
//! (`dbDbLink.c:110`, `:124`, `:141`), and the split is *itself* a
//! breadth-first reachability recomputation over the live graph
//! (`dbLock.c:710-760`) — C only avoids recomputing on the merge side
//! because it already knows the two endpoints. Keeping the old target on
//! the port's side to reproduce that pair would mean storing the edge set a
//! second time, next to the link text that already is the edge set, and a
//! second copy of a fact is what goes stale. The owner therefore re-derives
//! the affected component from the live link text and re-partitions it,
//! which is one rule for creation, removal, retarget, record deletion and
//! alias changes alike instead of a case per verb.
//!
//! Set ids follow C where C fixes them: the component holding the edited
//! record keeps its id (C's `dbLockSetMerge` keeps `pfirst`'s set, and
//! `pfirst` is the record whose link moved), a component that splits off
//! takes a fresh set from the free list as `makeSet` does, and the sets a
//! merge empties go back on the free list.
//!
//! `field_io.rs`'s `NotifyClaim` is unchanged by this: it closes dbNotify's
//! test-then-install window inside the critical section that tested the slot,
//! which is a stronger guarantee than the lock-set region it was standing in
//! for, not a substitute that lock sets now make unnecessary.
//!
//! What the gate *is* — a blocking priority-inheritance mutex
//! ----------------------------------------------------------
//! The gate is a [`crate::runtime::sync::PriorityInheritanceMutex`], the
//! same primitive L46 (`registration_mutex`), L8a (`simple_pvs`) and L8b
//! (one `scan_index` bucket) already use, and [`PvDatabase::lock_record`] /
//! [`PvDatabase::lock_records`] are plain synchronous `fn`s returning RAII
//! guards. This is the parity shape rather than a Rust-side invention: the
//! `ls->lock` C's `dbScanLock` takes is a plain `epicsMutex` (`dbLock.c:86`),
//! and on
//! the RTEMS arm
//! base compiles the POSIX implementation
//! (`configure/toolchain.c:31-35` selects `OS_API = posix` for
//! `__RTEMS_MAJOR__ >= 5`; `os/RTEMS-posix/osdMutex.c:8` is one `#include
//! "../posix/osdMutex.c"`), whose `globalAttrInit`
//! (`os/posix/osdMutex.c:71-88`) builds every `epicsMutex` with
//! `PTHREAD_PRIO_INHERIT` — probing it once and silently degrading to
//! `PTHREAD_PRIO_NONE` if the target refuses. `PriorityInheritanceMutex` is
//! that same construction on that same API, including the probe.
//!
//! ### The band-ordered wait queue is gone, and why that is not a loss
//!
//! Until §5 step 4 this gate was an async lock, and between steps 2 and 5 it
//! was a hand-rolled `PriorityGate` whose waiters were parked in a
//! `BTreeMap` keyed by the waiter's declared EPICS band, highest band first,
//! FIFO among equals. That queue existed for exactly one reason: while the
//! gate was async, both ends of a contention pair were *tasks* parked on a
//! userspace queue the kernel could not see, so nothing but our own code
//! could order them. It was the async bridge, not the target design.
//!
//! With a blocking PI mutex the waiters are real threads blocked in
//! `pthread_mutex_lock`, so the *OS* orders the queue — by thread priority,
//! which on the RTEMS backend is the EPICS band the thread declared through
//! `enter_ioc_thread` — and additionally boosts a preempted low-band holder
//! to the highest waiting band. The band-ordered wake order is therefore
//! replaced by the kernel's PI wait order, which is strictly stronger: it is
//! what closes handoff §8.0 **gap 4** (priority inheritance), which no
//! userspace queue could close at all. `PriorityGate`, its `BTreeMap` wait
//! queue, `GateAcquire` and the `DECLARED_BAND` thread-local that fed it are
//! deleted with this flip.
//!
//! ### Where the ordering actually holds — [`crate::runtime::sync::is_pi_mutex_active`]
//!
//! Priority inheritance is a property of the *build and the target*, and the
//! function above is the single place that answers whether this process got
//! it:
//!
//! * **RTEMS** — PI, and the answer is a *probe* result rather than a `cfg!`,
//!   matching C's own degrade path (`os/posix/osdMutex.c:77-85`, reported by
//!   `epicsMutexShowAll` at `:199-205`).
//! * **Linux with the `linux-rt` Cargo feature** — PI unconditionally.
//! * **every other build, including a default hosted Linux `cargo test`** —
//!   `parking_lot::Mutex`, which has **no** priority inheritance and no
//!   priority ordering. The host suite therefore verifies the *exclusion*
//!   this module provides, never its ordering; ordering is on-target
//!   territory.
//!
//! Read as a claim about *this* gate: on the host, `lock_record` excludes and
//! nothing more, and no host test can be written that would catch a lost
//! inversion. Two further conditions have to hold on target before the
//! ordering is real, and neither is this module's to enforce — the probe must
//! have returned `PTHREAD_PRIO_INHERIT`, and the contending threads must
//! actually carry distinct scheduling priorities, which requires
//! `RtPolicy::AllowRealtime` in [`crate::runtime::task`]. With the RT switch
//! off, every thread is one priority and PI has nothing to inherit.
//!
//! Acquisition order — MUST
//! ------------------------
//! Written down because every lock in the chain is now *blocking* and a
//! cycle would wedge a thread rather than a task. The order below is the
//! one the code actually takes, not an aspiration — it was derived by reading
//! every nesting site, and the bypass audit is in the commit that added it.
//!
//! > **A thread MUST acquire these in this order and MUST NOT acquire any of
//! > them while holding one that appears later:**
//! >
//! > 0. **L33** — `epics-bridge-rs`' `GroupPvDef::atomic_write_lock`, the
//! >    QSRV per-group atomic-PUT gate (`PriorityInheritanceMutex`). Outside
//! >    this crate, and only the atomic group PUT takes it — see the L33
//! >    section below.
//! > 1. **L1** — the per-record advisory gate ([`PvDatabase::lock_record`] /
//! >    [`PvDatabase::lock_records`]), *this* module
//! >    (`PriorityInheritanceMutex`).
//! > 2. **L46** — `PvDatabaseInner::registration_mutex`
//! >    (`PriorityInheritanceMutex`).
//! > 3. the leaves, none of which is ever held while another lock is taken:
//! >    **L8a** `simple_pvs`, **L8b** one `scan_index` bucket and **L7**
//! >    `ProcessVariable::subscribers` (all `PriorityInheritanceMutex`), plus
//! >    the `records` map and `aliases` (each a `RecursiveReadLock`, whose
//! >    readers never queue behind a waiting writer, so a reader under L1
//! >    cannot be wedged by a writer that is itself waiting for L1; and
//! >    whose readers never take L1 fresh under the guard, so a writer
//! >    under L1 — `remove_record_entry` — cannot be wedged by a reader).
//! >    A record's data has no lock of its own: it is behind L1.
//! >
//! > Every rung is a blocking lock. There is no async lock left anywhere on
//! > the put/process path, which is what makes the order a MUST rather than a
//! > preference: a cycle wedges a thread.
//!
//! [`RecordLockRegistry`]'s own mutex (a `std::sync::Mutex`) is *not* a rung of
//! that order: it is a leaf taken UNDER the sets — a mover holds its region
//! first and consults or edits the registry inside — and no lock set is ever
//! acquired while it is held. A guard's release path deliberately does not
//! touch it — everything a release needs lives in the set's own cell.
//!
//! **Owner/Gate:** [`PvDatabase::update_scan_index`] and
//! `PvDatabase::remove_record` are the **only** production functions that
//! take L46 from inside an L1-held window, and each does it the same way:
//! the record is looked up, its set taken, the gate taken, and the map entry
//! re-verified by `Arc::ptr_eq` under the gate, retrying if it changed. Every
//! other L46 holder (`add_pv`, `add_pv_with_hooks_full`, `remove_simple_pv`,
//! `add_loaded_record`, `add_alias`, `add_breaktables`) is a registration
//! entry point reached from `.db` load, iocsh or the gateway, never from
//! inside a put/process cycle, and reads no record data under the gate
//! (`add_breaktables` drops it before installing its tables) — verified with
//! `rg` over those symbols in `field_io.rs`, `processing.rs`, `links.rs`,
//! `qsrv/group.rs` and `pvalink/integration.rs`, where every hit is inside a
//! `#[cfg(test)]` module. `LockSet::acquire` asserts the rule in debug
//! builds: a fresh L1 acquisition on a thread holding L46 panics naming both
//! ends. A third function that nests L46 under L1 follows the same shape, or
//! the order above stops being checkable by reading one function.
//!
//! The table orders the rungs against each other; it does not say what
//! happens when one rung is taken twice. For L46 that matters, because
//! `update_scan_index` takes L46 itself: **no caller may hold L46 when
//! reaching it.** `PriorityInheritanceMutex` is not reentrant, so a caller
//! that does parks on itself, and the symptom is a hung registration rather
//! than an error. Every L46 acquisition therefore goes through
//! [`PvDatabase::lock_registration`], which knows whether this thread already
//! holds the gate and panics naming both ends instead of parking. The rule is
//! C's too: `iterateRecords` (`iocInit.c:562-586`) walks an already-built
//! database in a separate pass, holding no registration lock.
//!
//! **L1 does NOT have that rule: it recurses, as C's does.** That is forced by
//! lock sets rather than chosen. Once a set spans every record a DB link
//! reaches, processing a record and then following its `FLNK` takes ONE mutex
//! under two different record names, so a non-reentrant L1 would wedge the
//! ordinary process path. C is under the same constraint and answers it the
//! same way: `epicsMutex` must be recursive — the header states the contract
//! in prose, not code, at `epicsMutex.h:16` "An epicsMutex may be claimed
//! recursively" and `:38` "MUST implement recursive locking" — and
//! `dbLock.c:224-234` counts the nesting under `LOCKSET_DEBUG`.
//!
//! `PriorityInheritanceMutex` is not reentrant, so the recursion is built here,
//! on top of it: each set records the thread inside it and how deep, the first
//! acquisition takes the mutex and the rest only raise the count, and the mutex
//! is released when the count returns to zero. The owner field is written only
//! by the owning thread, so a non-owner can never match it. What this replaced
//! — a thread-local held-name set that PANICKED on re-entry — was the right
//! guard while a gate was one record, and is exactly wrong once a gate is a
//! component: it would have fired on the first `FLNK`.
//!
//! `dbScanLockMany`'s own refusal (`cantProceed("dbScanLockMany(%p) already
//! locked.  Recursive locking not allowed")`, `dbLock.c:392-395`) is about
//! re-using ONE `dbLocker` object, not about a thread holding two. Every
//! [`PvDatabase::lock_records`] call builds its own, so there is nothing here
//! to refuse.
//!
//! ### The rule's teeth are structural, and now they cover L1 too
//!
//! Every guard in the list above is `!Send`. A `!Send` value held across an
//! `.await` makes the enclosing future `!Send`, which the compiler rejects at
//! every `tokio::spawn` / `runtime::task::spawn` site in this workspace — so
//! "no suspension point inside a gate window" is a build error rather than a
//! review convention. That is the structural guarantee holders H1–H9 were
//! staged to make reachable: each holder
//! was first rewritten so its gate-held region contained zero `.await`s, and
//! only then did the gate become a type that refuses to be held across one.
//!
//! `!Send`ness is deliberate on both arms of [`crate::runtime::sync::PriorityInheritanceMutex`],
//! and on the PI arm it is also a correctness requirement, not only a
//! lint: POSIX requires a mutex to be unlocked by the thread that locked it,
//! so a guard that could migrate between threads would call
//! `pthread_mutex_unlock` from a non-owner.
//!
//! The compiler only *reports* it at a spawn site, though, so the standing
//! check is a direct one — for every binding of a gate guard, read forward to
//! the end of its drop scope and find no `.await`:
//!
//! ```text
//! rg -n 'let (mut )?\w+ = .*\.(lock_record|lock_records|acquire_put_gate)\(' crates/
//! ```
//!
//! ### L33 — the QSRV atomic-PUT group lock, relative to L1
//!
//! `epics-bridge-rs`' `GroupPvDef::atomic_write_lock` (`qsrv/group_config.rs`)
//! is a group-vs-group serialization aid: it lives in a different crate and
//! has no nesting relationship with L46/L8a/L8b, but it *is* held across L1
//! and so occupies rung 0 of the order above. It is acquired in
//! `GroupChannel::put`'s atomic branch **before** [`PvDatabase::lock_records`]
//! — first so a conversion failure in the up-front value-conversion phase
//! aborts the whole atomic PUT before any member-record gate is even
//! requested, second so two atomic PUTs to the *same* group serialize before
//! either reaches L1 at all.
//!
//! It is a `PriorityInheritanceMutex`. It was a `tokio::sync::Mutex` for
//! exactly as long as L1 was async: its window contains `lock_records`, which
//! used to be a genuine suspension point, and a `!Send` guard across that
//! await would not compile at the connection-task spawn site. That window is
//! now the conversion phase, a synchronous `lock_records`, and a synchronous
//! member loop — zero `.await`s — so the reason to keep it async is gone.

// No RTEMS-EXEC-MODEL-ALLOW marker: this file's tests are all plain `#[test]`s
// now that the gate is a blocking lock (a contender has to be a real thread),
// so none of them needs a reactor and there is nothing to account for.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use crate::server::record::RecordCell;

use crate::runtime::sync::{
    MutexInfo, PriorityInheritanceMutex, PriorityInheritanceMutexGuard, mutex_report,
};

use super::PvDatabase;

/// C's `lockSet` (`dbLockPvt.h:29-44`): the one mutex every member record
/// locks through, plus the bookkeeping the guards need without the registry.
///
/// The membership list is NOT here — a merge rewrites it, and it lives in
/// [`Registry`] behind the registry mutex. What a guard needs at release time
/// is here instead, so releasing never takes the registry lock: doing that
/// while holding the set would close a cycle against [`Registry::real_set_of`],
/// which takes the registry lock and then the set.
struct LockSet {
    /// C's `lockSet::id` (`dbLockPvt.h:33`). Assigned once by
    /// [`Registry::make_set`] and kept for the cell's whole life, free-list
    /// round trips included, exactly as C's is.
    id: u64,
    /// C's `lockSet::lock` (`:32`) — the mutex `dbScanLock` takes.
    lock: PriorityInheritanceMutex<()>,
    /// Position of this set's mutex among this file's entries in the process
    /// mutex list, so [`lock_set_mutex_rows`] can hand back the row
    /// `epicsMutexShow` prints for it. See [`SET_MUTEX_SEQ`].
    mutex_seq: u64,
    /// The thread currently inside `lock`, or 0. Written only by that thread.
    owner: AtomicU64,
    /// How deep that thread's recursion is. C's `epicsMutex` is recursive by
    /// contract (`epicsMutex.h:16`, `:38`) and `dbLock.c:224-234` counts the
    /// same nesting under `LOCKSET_DEBUG`.
    depth: AtomicUsize,
    /// References ABOVE the one-per-member baseline. `dbScanLockMany` adds one
    /// per set for its locked list (`dbLock.c:404`) and `dbScanLock` does not
    /// — it drops its transient reference the moment it holds the mutex
    /// (`:220-222`) — which is why an idle set reports exactly as many refs as
    /// it has members, as the oracle capture in `iocsh`'s `dblsr` shows.
    many_holds: AtomicUsize,
    /// The mutex guard of the current outermost hold, kept here rather than
    /// in that hold's [`SetGuard`] so that a frame which does not own the
    /// guard can still give the set up and take it back — [`Self::unheld`],
    /// C's `dbScanUnlock` from inside `dbBkpt` (`dbBkpt.c:795`). Touched only
    /// by the thread named in `owner`, under the mutex itself.
    held: std::cell::UnsafeCell<Option<PriorityInheritanceMutexGuard<'static, ()>>>,
}

// SAFETY: `held` is the one non-`Sync` field, and it is read or written only
// by the thread that holds `lock` (`acquire`'s fresh arm, `SetGuard::drop`'s
// outermost arm, and `unheld`, each of which checks or establishes
// `owner == thread_key()` first). The guard never leaves that thread: the
// same thread that stashes it takes it out, so the `!Send` guard's
// unlock-by-owner rule holds.
unsafe impl Sync for LockSet {}

/// The `'static` handle to a lock set. Sets are leaked: a guard hands out a
/// `'static` borrow of the mutex, and a set outlives every record it holds —
/// C keeps its emptied sets on `lockSetsFree` for the same reason and frees
/// them only in `dbLockCleanupRecords` (`dbLock.c:563-576`).
type Set = &'static LockSet;

/// C's `lockRecord` (`dbLockPvt.h:52-60`) — the cell `dbCommon::lset` points
/// at, and the ONLY way a record reaches its lock set.
///
/// C's `dbScanLock` is `precord->lset` → `lr->plockSet` → `lock`: two pointer
/// derefs, no name anywhere. The port had the same association spelled as a
/// name-keyed map, so taking a record's gate cost an alias lookup plus two
/// hashes of the record name under the registry mutex — 0.85 us of a 11.5 us
/// calc cycle, measured. A record that owns its cell pays neither.
///
/// `plock_set` is the association C guards with the `lockRecord`'s spinlock
/// (`dbLockPvt.h:53-57`): written only by [`Registry`], which holds the
/// registry mutex, and read with no lock at all, exactly as C reads it under
/// either lock. A stale read is not a hazard — it is what the re-check loop in
/// [`Self::acquire_fresh`] exists to catch.
pub(crate) struct LockRecord {
    plock_set: AtomicPtr<LockSet>,
}

impl LockRecord {
    fn new(set: Set) -> Arc<Self> {
        Arc::new(Self {
            plock_set: AtomicPtr::new(Self::as_ptr(set)),
        })
    }

    /// The cell a record is born with — C's null `lset` before
    /// `dbLockInitRecords`, spelled as a pointer at the one shared
    /// [`bootstrap_set`] so that the record's data is guarded from its first
    /// access, before the registry has given it a set of its own.
    pub(crate) fn bootstrap() -> Arc<Self> {
        Self::new(bootstrap_set())
    }

    /// Whether this record still locks through the bootstrap set, which is
    /// what `dblsr` reports as "no lock set" (`dbLock.c:900-901`).
    pub(crate) fn is_bootstrap(&self) -> bool {
        std::ptr::eq(self.set(), bootstrap_set())
    }

    /// C `dbScanLock`'s body (`dbLock.c:184-213`) — take the set the cell
    /// names, then check the cell still names it.
    ///
    /// The re-check is not optional: a merge running concurrently moves the
    /// record behind another mutex, and the one just taken would guard
    /// nothing. C compares `lockSet` pointers under the record's spinlock;
    /// the cell's store is `Release` and the load `Acquire`, which is the same
    /// publication. The mover holds the set it moves records OUT of (see
    /// [`Registry::place`]), so a thread that took the old set and lost the
    /// re-check was never inside the record's data — it merely waited.
    #[inline]
    pub(crate) fn acquire(&self) -> SetGuard {
        let set = self.set();
        if set.owner.load(Ordering::Acquire) == thread_key() {
            // This thread holds the set, so by Rule R the record cannot leave
            // it: no re-check, and nothing but the depth counter to touch.
            // This is every `rec.read()` / `rec.write()` inside a process
            // cycle, which is why it is inlined and the rest is not.
            return set.reenter(false);
        }
        self.acquire_fresh()
    }

    /// The set was not held: take whatever set the record is behind by the
    /// time the mutex is ours — a merge can move the record while this
    /// thread waits, and the set it then holds would be the wrong one.
    #[cold]
    #[inline(never)]
    fn acquire_fresh(&self) -> SetGuard {
        loop {
            let set = self.set();
            let guard = set.acquire(false);
            if std::ptr::eq(self.set(), set) {
                return guard;
            }
            drop(guard);
        }
    }

    /// See [`LockSet::unheld`]: give up the set this record is in for the
    /// duration of `f`. This thread must hold that set exactly once.
    pub(crate) fn unheld<R>(&self, f: impl FnOnce() -> R) -> R {
        self.set().unheld(f)
    }

    fn as_ptr(set: Set) -> *mut LockSet {
        set as *const LockSet as *mut LockSet
    }

    /// C `lr->plockSet`.
    fn set(&self) -> Set {
        let p = self.plock_set.load(Ordering::Acquire);
        // SAFETY: every pointer stored here came from [`Registry::make_set`],
        // which leaks its `LockSet`, so the referent is live for `'static`;
        // a merge only ever moves the set onto `Registry::free`, which keeps
        // the allocation. The cell is constructed with a set and `store` is
        // the only other writer, so it is never null.
        unsafe { &*p }
    }

    fn store(&self, set: Set) {
        self.plock_set.store(Self::as_ptr(set), Ordering::Release);
    }
}

/// C's `next_id` starts at 1 and `makeSet` uses the POST-increment
/// (`dbLock.c:70`, `:87`), so C's first lock set is number 2. Matching it
/// costs nothing and makes an A/B against a C IOC read straight across.
const FIRST_SET_ID: u64 = 2;

/// Serialises lock-set mutex creation with its sequence counter.
///
/// The process mutex list ([`mutex_report`]) has no per-mutex accessor, so a
/// set finds its own row positionally. That is exact only if the order sets
/// are appended to the list is the order they take sequence numbers, which
/// this lock is what guarantees — several `PvDatabase`s in one process each
/// run their own registry mutex and would otherwise interleave. Nothing is
/// acquired while it is held.
static SET_MUTEX_SEQ: std::sync::Mutex<u64> = std::sync::Mutex::new(0);

/// Mint one set. Every `LockSet` in the process comes from here.
fn new_set(id: u64) -> Set {
    let mut seq = SET_MUTEX_SEQ.lock().unwrap_or_else(|e| e.into_inner());
    let mutex_seq = *seq;
    *seq += 1;
    // The mutex is created UNDER `SET_MUTEX_SEQ` so its position in the
    // process mutex list matches `mutex_seq`. This is the only
    // `PriorityInheritanceMutex::new` in this file, which is what makes
    // filtering that list by creating file select exactly these mutexes.
    let set: Set = Box::leak(Box::new(LockSet {
        id,
        lock: PriorityInheritanceMutex::new(()),
        mutex_seq,
        owner: AtomicU64::new(0),
        depth: AtomicUsize::new(0),
        many_holds: AtomicUsize::new(0),
        held: std::cell::UnsafeCell::new(None),
    }));
    drop(seq);
    set
}

/// The set every record locks through until a registry gives it one — C's
/// null `lset` made lockable, so that a record's data is guarded by a lock
/// set from its first access rather than only from `iocInit` on.
///
/// One per process, never in any registry's `active` or `free` list, and
/// its id `0` is never a set id (`FIRST_SET_ID` is 2). It is created in this
/// file under [`SET_MUTEX_SEQ`] like every other set, so it holds a row in
/// the process mutex list and shifts nothing — see [`lock_set_mutex_rows`].
fn bootstrap_set() -> Set {
    static BOOTSTRAP: std::sync::OnceLock<Set> = std::sync::OnceLock::new();
    BOOTSTRAP.get_or_init(|| new_set(0))
}

/// A process-unique non-zero key for the current thread.
///
/// `ThreadId` has no stable integer form on stable Rust, and the value only
/// has to be comparable and never reused while a thread lives.
///
/// The slot is `const`-initialised and holds a `Cell` rather than being
/// initialised from `NEXT` directly, because that is what makes the read a
/// plain TLS offset: a `thread_local!` with a runtime initialiser carries a
/// lazy-init flag and a destructor registration, and goes through
/// `LocalKey::try_with` on every access. `LockSet::acquire` asks for this key
/// on every `dbScanLock`, so that check was the single largest cost in taking
/// a record's lock set. Zero is the "not yet minted" value and never a key,
/// which is why `NEXT` starts at 1.
fn thread_key() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    thread_local! {
        static KEY: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    KEY.with(|k| match k.get() {
        0 => {
            let minted = NEXT.fetch_add(1, Ordering::Relaxed);
            k.set(minted);
            minted
        }
        key => key,
    })
}

impl LockSet {
    /// C `dbScanLock` (`dbLock.c:180-234`) minus the reference dance: take the
    /// set's mutex, or count one more level if this thread is already inside
    /// it.
    ///
    /// Recursion is not a convenience. Once a lock set spans every record a
    /// DB link reaches, processing a record and then its `FLNK` target takes
    /// the SAME mutex twice on one thread, which is precisely why C's
    /// `epicsMutex` is required to be recursive.
    ///
    /// The re-entry is the hot path: every `rec.read()` / `rec.write()` on
    /// the process path lands here with the set already held, so it is one
    /// TLS load, one compare and a plain counter — no read-modify-write.
    /// `depth` is written only by the owning thread, which is what makes a
    /// load-then-store correct.
    fn acquire(&'static self, many: bool) -> SetGuard {
        let me = thread_key();
        if self.owner.load(Ordering::Acquire) == me {
            return self.reenter(many);
        }
        self.lock_fresh(me);
        if many {
            self.many_holds.fetch_add(1, Ordering::Relaxed);
        }
        SetGuard {
            set: self,
            outermost: true,
            many,
        }
    }

    /// One more level on a thread that already holds the set. The caller
    /// has checked `owner`.
    #[inline]
    fn reenter(&'static self, many: bool) -> SetGuard {
        self.depth
            .store(self.depth.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        if many {
            self.many_holds.fetch_add(1, Ordering::Relaxed);
        }
        SetGuard {
            set: self,
            outermost: false,
            many,
        }
    }

    /// Take the mutex on a thread that does not hold it and publish the hold.
    fn lock_fresh(&'static self, me: u64) {
        // A fresh acquisition is rung L1 of the order in the module doc. L46
        // sits below it, so a thread that holds the registration gate and
        // reaches here is closing the cycle `update_scan_index` opens from
        // the other side. Fail here, on the thread that made the mistake,
        // rather than wedge two threads later.
        debug_assert!(
            !super::registration_gate_held(),
            "a lock set (L1) was taken while this thread holds the registration \
             gate (L46); record data is behind L1 and must be read before the \
             gate is taken, or through a set this thread already holds"
        );
        // The records map and the alias table are leaves too, and
        // `remove_record_entry` writes them while holding the removed
        // record's set: a fresh set taken under a read guard on either is
        // the reader that writer can be waiting on. Clone the cell out, drop
        // the guard, then lock (`RecursiveReadLock`, `mod.rs`).
        debug_assert!(
            !super::map_read_held(),
            "a lock set (L1) was taken while this thread holds a read guard on \
             the records map or the alias table; clone the record handle out \
             under the guard and drop it before locking the record"
        );
        let guard = self.lock.lock();
        // SAFETY: this thread holds `lock`, so it is the only one allowed at
        // `held` (see the `Sync` impl).
        unsafe { *self.held.get() = Some(guard) };
        self.owner.store(me, Ordering::Release);
        self.depth.store(1, Ordering::Relaxed);
    }

    /// Release the mutex this thread holds and clear the hold. Only the
    /// owner, at depth 1, may call this.
    fn unlock_outermost(&'static self) {
        // Clear ownership BEFORE the mutex is released, or the next owner
        // could publish itself and be overwritten by this store.
        self.depth.store(0, Ordering::Relaxed);
        self.owner.store(0, Ordering::Release);
        // SAFETY: as in `lock_fresh` — the mutex is still held here.
        let guard = unsafe { (*self.held.get()).take() };
        drop(guard);
    }

    /// Run `f` with this set given up, then take it back — C's
    /// `dbScanUnlock(precord); epicsThreadSuspendSelf(); dbScanLock(precord);`
    /// (`dbBkpt.c:794-796`), the breakpoint park.
    ///
    /// The set is released from a frame that does not own the outermost
    /// [`SetGuard`] (a `FLNK` target's hook parks under the entry record's
    /// gate), which is why the guard lives in [`Self::held`] and not in that
    /// `SetGuard`. The same set is retaken so the outer guard's release
    /// stays paired with its acquisition, whatever moved meanwhile: a record
    /// re-linked out of this set while it was given up is guarded by its new
    /// set on every later access, because every access goes through
    /// [`LockRecord::acquire`] and not through the outer gate.
    ///
    /// # Panics
    ///
    /// If this thread does not hold the set, or holds it more than once —
    /// a nested hold means a record borrow is live, and nothing may borrow
    /// record data across a park.
    fn unheld<R>(&'static self, f: impl FnOnce() -> R) -> R {
        let me = thread_key();
        assert_eq!(
            self.owner.load(Ordering::Acquire),
            me,
            "a lock set was given up by a thread that does not hold it"
        );
        assert_eq!(
            self.depth.load(Ordering::Relaxed),
            1,
            "a lock set was given up with a nested hold live"
        );
        assert_eq!(
            self.many_holds.load(Ordering::Relaxed),
            0,
            "a lock set was given up from inside a many-lock transaction"
        );
        self.unlock_outermost();
        let r = f();
        self.lock_fresh(me);
        r
    }

    /// C's `epicsMutexTryLock` probe in `dbLockShowLocked` (`dbLock.c:963-965`).
    fn is_locked(&self) -> bool {
        self.lock.try_lock().is_none()
    }
}

/// One acquisition of one lock set — C's `dbScanLock`/`dbScanUnlock` pair.
///
/// `!Send` on both backends, because the inner guard is: an `Option` is `Send`
/// only when its payload is, so the recursive re-entry that carries `None` is
/// `!Send` too.
pub(crate) struct SetGuard {
    set: Set,
    /// `false` for a recursive re-entry, which acquired no new mutex. The
    /// outermost hold's mutex guard is in [`LockSet::held`].
    outermost: bool,
    many: bool,
}

impl SetGuard {
    /// Whether `record` is in the set this guard holds — C `dbLockGetLockId`
    /// equality. Rule R makes the answer stable for the guard's lifetime:
    /// a record cannot leave a set while a thread holds it.
    #[inline]
    pub(crate) fn holds(&self, record: &LockRecord) -> bool {
        std::ptr::eq(record.set(), self.set)
    }
}

impl Drop for SetGuard {
    fn drop(&mut self) {
        if self.many {
            self.set.many_holds.fetch_sub(1, Ordering::Relaxed);
        }
        if self.outermost {
            self.set.unlock_outermost();
        } else {
            self.set.depth.store(
                self.set.depth.load(Ordering::Relaxed) - 1,
                Ordering::Relaxed,
            );
        }
    }
}

/// Hold every set in `sets`, each once, in id order — the acquisition
/// discipline of `dbScanLockMany` (`dbLock.c:384-440`), used by every path
/// that takes more than one set so that two such paths cannot deadlock.
fn hold_sorted(sets: &[Set]) -> Vec<SetGuard> {
    let mut sets: Vec<Set> = sets.to_vec();
    sets.sort_unstable_by_key(|set| set.id);
    sets.dedup_by_key(|set| set.id);
    sets.into_iter().map(|set| set.acquire(false)).collect()
}

/// C's `lockSetsActive` / `lockSetsFree` / `next_id` (`dbLock.c:42-70`) plus
/// the record-to-set mapping C keeps in `dbCommon::lset`.
struct Registry {
    /// C `lockSetsActive`, keyed by set id — which is also C's list ORDER,
    /// because ids ascend with creation and a merge only ever removes an
    /// entry. `dblsr` and `dbLockShowLocked` walk this list in that order.
    active: BTreeMap<u64, SetState>,
    /// C `lockSetsFree` (`:44`): sets a merge emptied. Their id and mutex
    /// survive and are handed back by the next [`Registry::make_set`], which
    /// is why C's free count is not simply "sets that ever existed minus live
    /// ones". `ellGet` takes the head, so this is a queue.
    free: VecDeque<Set>,
    /// C's `dbCommon::lset` by canonical record name — the port's answer for
    /// the callers that have only a name: [`PvDatabase::lock_records`] accepts
    /// names that were never records, and the reports below print by name.
    ///
    /// The [`LockRecord`] here is the SAME cell the record instance holds, so
    /// there is one association and not two: everything that moves a record
    /// between sets moves it by storing into this cell.
    of_record: HashMap<String, Arc<LockRecord>>,
    next_id: u64,
    /// Bumped by every change to the partition — a set minted, a cell moved,
    /// an entry dropped. A mover snapshots it, takes the sets it means to
    /// move records out of, and re-reads it under the registry lock: equal
    /// means the sets it holds are the sets those records are still behind.
    revision: u64,
    /// Whether `dbLockInitRecords` has run — [`PvDatabase::build_lock_sets`].
    /// C reaches `dbLockSetMerge` only from links opened after it
    /// (`dbDbInitLink`/`dbDbAddLink`), so a link written before it merges
    /// nothing, even though this port's write gate has already minted the
    /// two records their sets. The flag is that moment, kept apart from
    /// "some set exists", which the gate makes true earlier.
    built: bool,
}

/// One entry of [`Registry::active`]: the shared cell plus C's
/// `lockSet::lockRecordList` (`dbLockPvt.h:31`).
struct SetState {
    set: Set,
    members: BTreeSet<String>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            active: BTreeMap::new(),
            free: VecDeque::new(),
            of_record: HashMap::new(),
            next_id: FIRST_SET_ID - 1,
            revision: 0,
            built: false,
        }
    }
}

impl Registry {
    /// C `makeSet` (`dbLock.c:72-101`): reuse a freed set, keeping its id and
    /// its mutex, or mint one.
    fn make_set(&mut self) -> Set {
        if let Some(set) = self.free.pop_front() {
            debug_assert_eq!(
                set.many_holds.load(Ordering::Relaxed),
                0,
                "C asserts refcount==0 for a set on the free list (dbLock.c:571)"
            );
            return set;
        }
        self.next_id += 1;
        self.revision += 1;
        new_set(self.next_id)
    }

    /// The cell `name` locks through, or `None` when the name has none —
    /// including a registered record that is still on the bootstrap set,
    /// which is C's null `lset` and what `dblsr` reports as no set at all
    /// (`dbLock.c:900-901`).
    fn real_set_of(&self, name: &str) -> Option<Set> {
        self.of_record
            .get(name)
            .map(|lr| lr.set())
            .filter(|set| !std::ptr::eq(*set, bootstrap_set()))
    }

    /// [`Self::real_set_of`] for a relink after `iocInit`, where a name the
    /// link graph reaches either has a real set or is no longer registered
    /// at all — it was removed between the graph read and this call, and
    /// its cell went with it. A registered name still on the bootstrap set
    /// is the invariant [`Self::adopt`] keeps, broken.
    fn set_of_registered(&self, name: &str) -> Option<Set> {
        let set = self.real_set_of(name);
        debug_assert!(
            set.is_some() || !self.of_record.contains_key(name),
            "registered record {name} is on the bootstrap set after iocInit"
        );
        set
    }

    /// The cell a NAME reaches, minting a one-member set for a name that has
    /// none — C `createLockRecord` (`dbLock.c:505-527`) for
    /// [`PvDatabase::lock_records`]' callers, which may name a record that
    /// never existed (`dbLockerAlloc` accepts the pointers it is given).
    ///
    /// Never mints for a registered record: its cell was adopted from the
    /// record itself ([`Self::adopt`]) and points at the bootstrap set until
    /// [`Self::mint_for`] moves it, under the hold that move requires.
    fn lock_record_of(&mut self, name: &str) -> Arc<LockRecord> {
        if let Some(lr) = self.of_record.get(name) {
            return lr.clone();
        }
        let set = self.make_set();
        let lr = LockRecord::new(set);
        self.of_record.insert(name.to_string(), lr.clone());
        self.active.insert(
            set.id,
            SetState {
                set,
                members: BTreeSet::from([name.to_string()]),
            },
        );
        lr
    }

    /// Register the cell a record was born with as the one its name reaches
    /// — C's `dbCommon::lset`, which `createLockRecord` allocates INTO the
    /// record so that the record and the registry can never hold two answers
    /// to "which set". Called before the record is published, so nothing can
    /// hold it yet and the cell may be re-pointed without a hold.
    ///
    /// A name that was many-locked before its record existed already has a
    /// registry-only cell with a real set; the record's cell takes that set
    /// over, so the epoch that holds it goes on excluding the record.
    ///
    /// After [`PvDatabase::build_lock_sets`] the cell is minted a set here as
    /// well — `createLockRecord` at the only moment C could not reach it —
    /// so that a registered record is never on the bootstrap set once the
    /// IOC is initialised. That is what lets every set-taker after `iocInit`
    /// ([`Registry::every_set`], [`PvDatabase::relink_lock_sets`]) leave the
    /// bootstrap set alone: a `LinkEdit` relink runs under a put that already
    /// holds the seed's set, and reaching for id 0 from there would invert
    /// the id order a `Membership` relink takes the sets in.
    fn adopt(&mut self, name: &str, lr: &Arc<LockRecord>) {
        match self.of_record.get(name) {
            Some(existing) if Arc::ptr_eq(existing, lr) => {}
            Some(existing) => {
                lr.store(existing.set());
                self.of_record.insert(name.to_string(), lr.clone());
                self.revision += 1;
            }
            None => {
                self.of_record.insert(name.to_string(), lr.clone());
                if self.built {
                    self.mint_for(name, lr);
                }
            }
        }
    }

    /// Give a bootstrap cell a set of its own — `dbLockInitRecords`'
    /// `createLockRecord` for one record. **The caller holds the bootstrap
    /// set, or nothing can hold the cell yet**: the cell is being moved out
    /// of the bootstrap set, and [`Self::place`]'s rule is that a mover holds
    /// the set it moves a record out of. [`Self::adopt`] is the one caller
    /// that holds nothing — it runs before the record is in the records map,
    /// so no thread can be inside the record's data through the cell.
    fn mint_for(&mut self, name: &str, lr: &Arc<LockRecord>) {
        if !lr.is_bootstrap() {
            return;
        }
        let set = self.make_set();
        lr.store(set);
        self.active.insert(
            set.id,
            SetState {
                set,
                members: BTreeSet::from([name.to_string()]),
            },
        );
        self.of_record
            .entry(name.to_string())
            .or_insert_with(|| lr.clone());
    }

    /// Move every member of `component` behind `set` — the ONE way a record
    /// changes lock set, so the cell a record holds and the `of_record` entry
    /// a name reaches can never disagree.
    ///
    /// **MUST be called holding the set each moved record is currently
    /// behind.** The set is the record's data lock: a thread inside the
    /// record's data holds that set, and moving the record out from under it
    /// would let a holder of the destination in beside it. Holding the source
    /// is what makes the store safe; the destination needs no hold, because
    /// nothing is inside a record the mover has just excluded everyone from.
    /// Every caller — [`Self::merge`], [`Self::repartition`],
    /// [`Self::mint_for`] — is reached only through a [`PvDatabase`] method
    /// that took those sets first, in id order, and re-read
    /// [`Self::revision`] under the registry lock to prove they are still the
    /// records' sets.
    fn place(&mut self, names: impl IntoIterator<Item = String>, set: Set) {
        self.revision += 1;
        for name in names {
            match self.of_record.get(&name) {
                Some(lr) => lr.store(set),
                None => {
                    self.of_record.insert(name, LockRecord::new(set));
                }
            }
        }
    }

    /// C `dbLockSetMerge` (`dbLock.c:580-666`): every record behind
    /// `second`'s mutex moves behind `first`'s, and the emptied set goes on
    /// the free list with its id and mutex intact.
    ///
    /// The direction matters and is C's: the SOURCE record's set survives
    /// (`dbDbLink.c:110` passes `plink->precord` first), so which id ends up
    /// holding a component depends on link order exactly as in C.
    ///
    /// Both names have real sets — [`PvDatabase::merge_sets`] gave them one
    /// and holds `second`'s, which is the one records move out of.
    fn merge(&mut self, first: &str, second: &str) {
        let (Some(a), Some(b)) = (self.real_set_of(first), self.real_set_of(second)) else {
            return;
        };
        let (a, b) = (a.id, b.id);
        if a == b {
            return;
        }
        let moved = self
            .active
            .remove(&b)
            .expect("every id in of_record names an active set");
        let survivor = self.active[&a].set;
        self.place(moved.members.iter().cloned(), survivor);
        let target = self
            .active
            .get_mut(&a)
            .expect("every id in of_record names an active set");
        target.members.extend(moved.members);
        self.free.push_back(moved.set);
    }

    /// **The partition transition itself** — see [`PvDatabase::relink_lock_sets`],
    /// which is its only caller.
    ///
    /// `seed` is the record whose link text just moved. The affected region is
    /// the closure of `seed` under two relations at once: "is linked to" and
    /// "is currently in the same set as". Closing over both is what makes one
    /// rule cover creation, removal and retarget — a merge widens the region
    /// through the first relation, a split narrows it through the second, and
    /// neither needs to know which one happened.
    ///
    /// `held` is every set the caller holds, and every set a record of the
    /// region is behind MUST be among them — see [`Self::place`]. The seed's
    /// own set is real: [`PvDatabase::relink_lock_sets`] minted it before
    /// taking the region.
    fn repartition(
        &mut self,
        seed: &str,
        adjacency: &HashMap<String, BTreeSet<String>>,
        held: &[Set],
    ) {
        let Some(seed_set) = self.of_record.get(seed).map(|lr| lr.set()) else {
            return;
        };
        let seed_id = seed_set.id;

        let mut affected: BTreeSet<String> = BTreeSet::new();
        let mut work: Vec<String> = vec![seed.to_string()];
        while let Some(name) = work.pop() {
            if !affected.insert(name.clone()) {
                continue;
            }
            if let Some(targets) = adjacency.get(&name) {
                work.extend(targets.iter().cloned());
            }
            if let Some(id) = self.real_set_of(&name).map(|set| set.id) {
                work.extend(self.active[&id].members.iter().cloned());
            }
        }

        // Every set the region covers. Each is emptied below and either
        // re-used for one of the new components or returned to the free list.
        let touched: BTreeSet<u64> = affected
            .iter()
            .filter_map(|name| self.real_set_of(name).map(|set| set.id))
            .collect();
        debug_assert!(
            affected.iter().all(|name| {
                self.of_record
                    .get(name)
                    .is_none_or(|lr| held.iter().any(|set| std::ptr::eq(*set, lr.set())))
            }),
            "repartition reached a record behind a set the caller does not hold"
        );

        // A record the database no longer has leaves the partition with it —
        // `dbDeleteRecord` frees the `lockRecord` — so it is dropped here
        // rather than being carried into a component of one.
        //
        // An instance still alive behind someone's `Arc` keeps its cell, which
        // goes on pointing at the set it had. C's does too, until
        // `dbLockCleanupRecords`. The set may later be re-minted off the free
        // list for another component, so such a gate can end up sharing a
        // mutex with live records — it over-locks a record nothing processes,
        // and can never under-lock one, because every LIVE record was placed
        // into its component's set above.
        for name in &affected {
            if !adjacency.contains_key(name) {
                self.of_record.remove(name);
            }
        }

        // The components of the region, the seed's first when the seed is
        // still a record. Each is closed inside `affected` by construction:
        // `affected` was built by following the same adjacency.
        let seed_present = adjacency.contains_key(seed);
        let mut components: Vec<BTreeSet<String>> = Vec::new();
        let mut placed: BTreeSet<String> = BTreeSet::new();
        let starts = seed_present
            .then(|| seed.to_string())
            .into_iter()
            .chain(affected.iter().cloned());
        for start in starts {
            if placed.contains(&start) || !adjacency.contains_key(&start) {
                continue;
            }
            let mut component: BTreeSet<String> = BTreeSet::new();
            let mut walk = vec![start];
            while let Some(name) = walk.pop() {
                if !component.insert(name.clone()) {
                    continue;
                }
                if let Some(targets) = adjacency.get(&name) {
                    walk.extend(targets.iter().cloned());
                }
            }
            placed.extend(component.iter().cloned());
            components.push(component);
        }

        // Which id each component keeps. C fixes two of these: the component
        // holding the edited record keeps the set that record was already in
        // (`dbLockSetMerge` keeps `pfirst`'s), and a component that was a
        // whole set already and is untouched keeps its own. A seed that has
        // been deleted reserves nothing — its set is freed with the rest.
        let mut keeps: BTreeSet<u64> = BTreeSet::new();
        if seed_present {
            keeps.insert(seed_id);
        }
        let mut assigned: Vec<Option<u64>> = Vec::with_capacity(components.len());
        for component in &components {
            if seed_present && component.contains(seed) {
                assigned.push(Some(seed_id));
                continue;
            }
            let mut ids = component
                .iter()
                .map(|name| self.real_set_of(name).map(|set| set.id));
            let first = ids.next().flatten();
            let uniform =
                first.filter(|id| !keeps.contains(id) && ids.all(|other| other == Some(*id)));
            if let Some(id) = uniform {
                keeps.insert(id);
            }
            assigned.push(uniform);
        }

        // Mint before freeing, as C does: `dbLockSetSplit` calls `makeSet`
        // while the set it is splitting is still live, so the new set comes
        // off whatever was already on the free list rather than off the id
        // this very edit is about to release.
        let fresh: Vec<Set> = assigned
            .iter()
            .filter(|id| id.is_none())
            .map(|_| self.make_set())
            .collect();
        let mut fresh = fresh.into_iter();

        for (component, id) in components.into_iter().zip(assigned) {
            let set = match id {
                Some(id) => self.active[&id].set,
                None => fresh
                    .next()
                    .expect("one fresh set per unassigned component"),
            };
            self.place(component.iter().cloned(), set);
            self.active.insert(
                set.id,
                SetState {
                    set,
                    members: component,
                },
            );
        }

        for id in touched.difference(&keeps) {
            let dropped = self
                .active
                .remove(id)
                .expect("every touched id named an active set");
            self.free.push_back(dropped.set);
        }
        self.revision += 1;
    }

    /// Every set a registered record can be behind after `iocInit`: the
    /// active ones. Not the bootstrap set — [`Self::adopt`] mints a set for
    /// every record registered after [`PvDatabase::build_lock_sets`], and
    /// [`PvDatabase::init_sets`] for every one registered before it, so no
    /// record a relink can reach is on id 0.
    fn every_set(&self) -> Vec<Set> {
        self.active.values().map(|state| state.set).collect()
    }

    fn info(&self, id: u64, rows: &HashMap<u64, MutexInfo>) -> LockSetInfo {
        let state = &self.active[&id];
        LockSetInfo {
            id,
            members: state.members.iter().cloned().collect(),
            refs: state.members.len() + state.set.many_holds.load(Ordering::Relaxed),
            locked: state.set.is_locked(),
            mutex: rows.get(&state.set.mutex_seq).cloned(),
        }
    }
}

/// The row `epicsMutexShow` prints for each lock-set mutex, keyed by
/// [`LockSet::mutex_seq`].
///
/// Positional because the process mutex list exposes no per-mutex accessor.
/// It is exact: [`Registry::make_set`] is the only `PriorityInheritanceMutex`
/// created in this file, so filtering by creating file selects exactly the
/// lock-set mutexes; creation is serialised by [`SET_MUTEX_SEQ`]; and a set's
/// cell is never dropped, so no entry ever leaves the list and shifts the
/// ones behind it.
fn lock_set_mutex_rows() -> HashMap<u64, MutexInfo> {
    mutex_report(false)
        .shown
        .into_iter()
        .filter(|info| info.file() == file!())
        .enumerate()
        .map(|(seq, info)| (seq as u64, info))
        .collect()
}

/// One active lock set, as `dblsr` and `dbLockShowLocked` report it.
pub struct LockSetInfo {
    /// C's `lockSet::id`.
    pub id: u64,
    /// C's `lockRecordList`, in the order `dblsr` walks it.
    pub members: Vec<String>,
    /// C's `lockSet::refcount`: one per member record, plus one for each
    /// [`PvDatabase::lock_records`] epoch currently holding this set.
    pub refs: usize,
    /// Whether the set's mutex cannot be taken right now — C's
    /// `epicsMutexTryLock` filter in `dbLockShowLocked`.
    pub locked: bool,
    /// The `epicsMutexShow` row for this set's mutex.
    pub mutex: Option<MutexInfo>,
}

/// What one `dblsr` / `dbLockShowLocked` call sees.
pub struct LockSetReport {
    /// C's `lockSetsActive`, in list order.
    pub active: Vec<LockSetInfo>,
    /// `ellCount(&lockSetsFree)`.
    pub free: usize,
}

/// The lock sets of one database — C's `lockSetsActive` and `lockSetsFree`.
///
/// Every record is behind exactly one set, and a DB link puts both of its
/// records behind the same one. Sets are created by
/// [`PvDatabase::build_lock_sets`] at IOC init, and lazily for a name that
/// reaches [`PvDatabase::lock_record`] without one.
///
/// Nothing is ever destroyed: a merged-away set moves to the free list, and
/// its mutex must outlive it because a `'static` guard may still be unwinding
/// through it. That bounds memory by the record count, which is what C's own
/// free list does.
#[derive(Default)]
pub(crate) struct RecordLockRegistry {
    inner: std::sync::Mutex<Registry>,
}

impl RecordLockRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Which set `record` is behind right now, without creating one.
    fn set_id_of(&self, record: &str) -> Option<u64> {
        self.lock().real_set_of(record).map(|set| set.id)
    }

    /// See [`Registry::adopt`].
    pub(crate) fn adopt(&self, name: &str, lr: &Arc<LockRecord>) {
        self.lock().adopt(name, lr);
    }
}

impl PvDatabase {
    /// Build the lock sets — C `dbLockInitRecords` followed by the
    /// `dbLockSetMerge` every DB link performs as it is opened
    /// (`iocInit.c:178-179`, `dbDbLink.c:110`).
    ///
    /// Called once from `ioc_init`, which is why `dbLockShowLocked` on a
    /// loaded-but-not-initialised IOC reports `0` and `0` exactly as C's does:
    /// before this runs no record has a set.
    ///
    /// C merges incrementally because it cannot afford to recompute; the
    /// result is the same either way, because `dbLockSetSplit` is itself a
    /// reachability recomputation (`dbLock.c:710-717`). Doing it as C does —
    /// one set per record, then one merge per link — is what reproduces the
    /// free-list count, which a components-first construction would report as
    /// zero.
    pub fn build_lock_sets(&self) {
        let mut names: Vec<String> = self
            .inner
            .records
            .read()
            .keys()
            .map(|n| n.to_string())
            .collect();
        names.sort();
        // Every edge is collected BEFORE the registry lock is taken:
        // `record_link_fields` reads the record map and each record's own
        // lock, and neither may be acquired underneath the registry.
        let mut edges: Vec<(String, String)> = Vec::new();
        for name in &names {
            for target in self.db_link_targets(name) {
                edges.push((name.clone(), target));
            }
        }
        self.init_sets(&names);
        for (from, to) in edges {
            self.merge_sets(&from, &to);
        }
    }

    /// C `dbLockInitRecords` (`dbLock.c:526-532`) through `createLockRecord`:
    /// one set per record, before any link has merged anything. The
    /// bootstrap set is held across the whole pass, because every record
    /// given a set here is being moved out of it.
    ///
    /// Every cell the registry has adopted is minted, not only `names`: a
    /// record adopted after `names` was read is registered but unnamed, and
    /// `built` promises that no registered record is on the bootstrap set
    /// from here on ([`Registry::every_set`]).
    fn init_sets(&self, names: &[String]) {
        let _out_of = bootstrap_set().acquire(false);
        let mut registry = self.inner.record_locks.lock();
        for name in names {
            let lr = registry.lock_record_of(name);
            registry.mint_for(name, &lr);
        }
        let mut adopted: Vec<(String, Arc<LockRecord>)> = registry
            .of_record
            .iter()
            .filter(|(_, lr)| lr.is_bootstrap())
            .map(|(name, lr)| (name.clone(), lr.clone()))
            .collect();
        adopted.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, lr) in &adopted {
            registry.mint_for(name, lr);
        }
        registry.built = true;
    }

    /// C `dbLockSetMerge` under C's protocol: both sets are taken before
    /// the registry decides anything, in id order, and the merge happens
    /// only if they are still the two records' sets once it is held.
    fn merge_sets(&self, first: &str, second: &str) {
        loop {
            let (a, b) = {
                let registry = self.inner.record_locks.lock();
                match (registry.real_set_of(first), registry.real_set_of(second)) {
                    (Some(a), Some(b)) => (a, b),
                    _ => return,
                }
            };
            if std::ptr::eq(a, b) {
                return;
            }
            let _held = hold_sorted(&[a, b]);
            let mut registry = self.inner.record_locks.lock();
            let unchanged = registry
                .real_set_of(first)
                .is_some_and(|set| std::ptr::eq(set, a))
                && registry
                    .real_set_of(second)
                    .is_some_and(|set| std::ptr::eq(set, b));
            if unchanged {
                registry.merge(first, second);
                return;
            }
        }
    }

    /// The cell `name` locks through, with a real set — minted now if the
    /// name has none. The by-name form of [`Self::ensure_set_for`].
    fn ensure_set(&self, name: &str) -> Arc<LockRecord> {
        let lr = self.inner.record_locks.lock().lock_record_of(name);
        self.ensure_set_for(name, &lr);
        lr
    }

    /// Move `lr` off the bootstrap set onto one of its own, if it is still
    /// there — `createLockRecord` for a record that reached a lock before
    /// `iocInit`; after it [`Registry::adopt`] has already done this. The
    /// bootstrap set is taken first and the registry under it: a mover holds
    /// the set it moves out of, and no set is ever taken under the registry
    /// lock.
    fn ensure_set_for(&self, name: &str, lr: &Arc<LockRecord>) {
        if !lr.is_bootstrap() {
            return;
        }
        let _out_of = bootstrap_set().acquire(false);
        self.inner.record_locks.lock().mint_for(name, lr);
    }

    /// The records `record`'s DB links reach, canonicalised.
    ///
    /// Only a link that resolved to a LOCAL record merges: C reaches
    /// `dbLockSetMerge` from `dbDbInitLink`, and a target this IOC does not
    /// have falls through to `dbCaAddLink` instead (`dbLink.c:118-130`), which
    /// merges nothing. `record_link_fields` has already applied that same
    /// locality rule, so a `ca://` link to a local record is a `Ca` link here
    /// and does not widen the set.
    fn db_link_targets(&self, record: &str) -> Vec<String> {
        use crate::server::record::ParsedLink;
        self.record_link_fields(record)
            .into_iter()
            .filter_map(|(_, _, parsed)| match parsed {
                ParsedLink::Db(link) => {
                    // `DbLink::target` is where the record name stops and the
                    // channel filter begins — C reaches `dbLockSetMerge` with
                    // `dbChannelRecord(chan)` (`dbDbLink.c:94-109`), the record
                    // the whole `pvname` resolved to, so `SRC.[2]` merges into
                    // SRC's set exactly as `SRC` does. Matching on the raw text
                    // instead left every filtered reader in a set of its own,
                    // and a slice read outside the source's set can tear
                    // against the source's own processing.
                    let name = self
                        .resolve_alias(&link.target().record)
                        .unwrap_or_else(|| link.target().record.clone());
                    self.get_record_no_resolve(&name).map(|_| name)
                }
                _ => None,
            })
            .collect()
    }

    /// C's `lockSetsActive` and `lockSetsFree` as `dblsr("*", n)` and
    /// `dbLockShowLocked(n)` read them.
    pub fn lock_set_report(&self) -> LockSetReport {
        let rows = lock_set_mutex_rows();
        let registry = self.inner.record_locks.lock();
        LockSetReport {
            active: registry
                .active
                .keys()
                .map(|id| registry.info(*id, &rows))
                .collect(),
            free: registry.free.len(),
        }
    }

    /// The lock set one record is behind, or `None` when it has none —
    /// `dblsr`'s `if (!plockRecord) return 0;` before `iocInit`
    /// (`dbLock.c:900-901`).
    ///
    /// Does not create a set: asking about a record must not change the
    /// report.
    pub fn lock_set_of(&self, record: &str) -> Option<LockSetInfo> {
        let canonical = self
            .resolve_alias(record)
            .unwrap_or_else(|| record.to_string());
        let rows = lock_set_mutex_rows();
        let registry = self.inner.record_locks.lock();
        let id = registry.real_set_of(&canonical)?.id;
        Some(registry.info(id, &rows))
    }

    /// The obligation to re-derive `record`'s lock set, taken out BEFORE a DB
    /// link field on it is written.
    ///
    /// `None` — no obligation — when the field is not a DBF link field, or
    /// when no lock sets exist yet: before `iocInit` C has no `lockRecord` to
    /// merge, so a `.db` load rewrites link text with nothing to maintain and
    /// [`PvDatabase::build_lock_sets`] does the whole job afterwards.
    ///
    /// Declare it ABOVE the record guard in the put body. Rust drops in
    /// reverse declaration order, so the record lock is down by the time the
    /// relink runs, which is the order the owner needs: it reads the link
    /// text of every record in the affected component.
    pub(crate) fn link_field_write<'a>(
        &'a self,
        record: &str,
        field: &str,
    ) -> Option<LockSetEdit<'a>> {
        let canonical = self
            .resolve_alias(record)
            .unwrap_or_else(|| record.to_string());
        if !self.is_dbf_link_field(&canonical, field) {
            return None;
        }
        if !self.inner.record_locks.lock().built {
            return None;
        }
        Some(LockSetEdit {
            db: self,
            record: canonical,
            scope: RelinkScope::LinkEdit,
        })
    }

    /// The same obligation for a change of MEMBERSHIP rather than of link
    /// text: a record added or removed after `iocInit`, or an alias that
    /// makes a link resolve to a record it did not resolve to before.
    ///
    /// One owner covers all three because the owner re-derives the component
    /// instead of tracking a verb — see the module doc. Declare it above the
    /// mutation, so the relink sees the database as it is afterwards.
    pub(crate) fn lock_set_membership_change<'a>(
        &'a self,
        record: &str,
    ) -> Option<LockSetEdit<'a>> {
        if !self.inner.record_locks.lock().built {
            return None;
        }
        Some(LockSetEdit {
            db: self,
            record: self
                .resolve_alias(record)
                .unwrap_or_else(|| record.to_string()),
            scope: RelinkScope::Membership,
        })
    }

    /// **The single owner of the lock-set partition after `iocInit`.**
    ///
    /// Re-derives the connected component `record` now sits in and
    /// re-partitions every set that component touches. C reaches the same
    /// result through `dbLockSetMerge` on link creation and `dbLockSetSplit`
    /// on removal (`dbDbLink.c:110`, `:124`, `:141`); see the module doc for
    /// why the port re-derives instead of tracking the endpoint pair.
    ///
    /// Private, and reachable only by dropping a [`LockSetEdit`].
    ///
    /// The sets are the records' data locks, so the region is taken before
    /// it is read and held while it is re-partitioned — C's `dbPutFieldLink`
    /// holds both `dbLockSetMerge` operands through `dbScanLockMany`
    /// (`dbAccess.c:1115-1123`, `dbLock.c:587-607`). Which sets make up the
    /// region is the scope's business:
    ///
    /// * [`RelinkScope::LinkEdit`] — the seed's set and the sets of the
    ///   records its links now reach. Both are what the put that landed the
    ///   link already holds ([`PvDatabase::acquire_put_gate`] takes them as
    ///   `dbPutFieldLink` does), so the acquisition here is a re-entry. The
    ///   partition invariant puts every OTHER link of the region inside it;
    ///   a target found outside is an invariant already broken, and the loop
    ///   widens the region to it rather than move a record it does not hold.
    /// * [`RelinkScope::Membership`] — every active set. A record added,
    ///   removed or newly reachable through an alias can join or leave a
    ///   component from anywhere, and the links that name it can be in any
    ///   set. Registration-path only, with no set held by the caller, so
    ///   taking them all in id order is the plain `dbScanLockMany`
    ///   discipline.
    ///
    /// Neither scope takes the bootstrap set: after `iocInit` no registered
    /// record is on it ([`Registry::adopt`] mints the set before the record
    /// is published), and a `LinkEdit` relink, entered under the seed's set,
    /// could not take id 0 in id order anyway — a `Membership` relink that
    /// held it first would be waiting on the seed's set.
    ///
    /// Sets are taken first and the registry lock under them, never the
    /// reverse; [`Registry::revision`] proves, under that lock, that the sets
    /// held are still the ones the region's records are behind.
    fn relink_lock_sets(&self, record: &str, scope: RelinkScope) {
        let mut region: Vec<Set> = Vec::new();
        loop {
            let revision = {
                let registry = self.inner.record_locks.lock();
                match scope {
                    RelinkScope::Membership => region = registry.every_set(),
                    RelinkScope::LinkEdit => {
                        if region.is_empty() {
                            region.extend(registry.real_set_of(record));
                        }
                    }
                }
                registry.revision
            };
            if scope == RelinkScope::LinkEdit {
                // The targets are read under the seed's set, which the put
                // holds; their sets come from the registry, not from a lock.
                let targets = self.db_link_targets(record);
                let registry = self.inner.record_locks.lock();
                for target in &targets {
                    let Some(set) = registry.set_of_registered(target) else {
                        continue;
                    };
                    if !region.iter().any(|held| std::ptr::eq(*held, set)) {
                        region.push(set);
                    }
                }
            }
            let held = hold_sorted(&region);
            let adjacency = match scope {
                RelinkScope::Membership => self.db_link_adjacency(),
                RelinkScope::LinkEdit => {
                    let members: Vec<String> = {
                        let registry = self.inner.record_locks.lock();
                        region
                            .iter()
                            .filter_map(|set| registry.active.get(&set.id))
                            .flat_map(|state| state.members.iter().cloned())
                            .collect()
                    };
                    self.db_link_adjacency_of(&members)
                }
            };
            let mut registry = self.inner.record_locks.lock();
            if registry.revision != revision {
                drop(registry);
                drop(held);
                continue;
            }
            let outside: Vec<Set> = adjacency
                .values()
                .flatten()
                .filter_map(|name| registry.set_of_registered(name))
                .filter(|set| !region.iter().any(|held| std::ptr::eq(*held, *set)))
                .collect();
            if !outside.is_empty() {
                region.extend(outside);
                region.sort_unstable_by_key(|set| set.id);
                region.dedup_by_key(|set| set.id);
                drop(registry);
                drop(held);
                continue;
            }
            registry.repartition(record, &adjacency, &region);
            return;
        }
    }

    /// The DB-link graph as an undirected adjacency map.
    ///
    /// Undirected because C's merge is: `dbLockSetMerge(locker, plink->precord,
    /// target)` puts both endpoints behind one mutex regardless of which way
    /// the link points, and `dbLockSetSplit` walks `bklnk` as well as the
    /// record's own links (`dbLock.c:735-770`) for the same reason. A record
    /// that is only ever pointed AT is as much a member as the one pointing.
    fn db_link_adjacency(&self) -> HashMap<String, BTreeSet<String>> {
        let names: Vec<String> = self
            .inner
            .records
            .read()
            .keys()
            .map(|n| n.to_string())
            .collect();
        self.db_link_adjacency_of(&names)
    }

    /// [`Self::db_link_adjacency`] over `names` only — the members of the
    /// sets a [`RelinkScope::LinkEdit`] relink holds. A target outside
    /// `names` still appears as a neighbour, which is how the relink notices
    /// a set it does not hold.
    fn db_link_adjacency_of(&self, names: &[String]) -> HashMap<String, BTreeSet<String>> {
        let mut adjacency: HashMap<String, BTreeSet<String>> = HashMap::new();
        for name in names {
            adjacency.entry(name.clone()).or_default();
        }
        for name in names {
            for target in self.db_link_targets(name) {
                adjacency
                    .entry(name.clone())
                    .or_default()
                    .insert(target.clone());
                adjacency.entry(target).or_default().insert(name.clone());
            }
        }
        adjacency
    }
}

/// A DB link field write that has landed in the record but not yet in the
/// lock-set graph.
///
/// The token exists so that the illegal state — link text changed, partition
/// stale — cannot be constructed rather than merely being checked for. It is
/// minted by [`PvDatabase::link_field_write`], holds the only reference
/// through which [`PvDatabase::relink_lock_sets`] is reachable, and performs
/// the relink from its destructor, so no exit path of a put body can skip it.
#[must_use = "the lock-set graph is only re-derived when this is dropped;               binding it to `_` drops it immediately and relinks too early"]
pub(crate) struct LockSetEdit<'a> {
    db: &'a PvDatabase,
    record: String,
    scope: RelinkScope,
}

impl Drop for LockSetEdit<'_> {
    fn drop(&mut self) {
        self.db.relink_lock_sets(&self.record, self.scope);
    }
}

/// Which sets a relink must hold — see [`PvDatabase::relink_lock_sets`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RelinkScope {
    /// A DB link field of the seed was written; the put holds the seed's set
    /// and the new target's.
    LinkEdit,
    /// The seed joined or left the database, or an alias changed what a
    /// link naming it resolves to; nothing is held.
    Membership,
}

/// RAII guard for one record's lock set.
///
/// Held for the duration of a plain CA/PVA write — the `dbScanLock` +
/// `dbScanUnlock` pair around one `dbPutField`. `!Send`, so the compiler
/// refuses to let it live across an `.await` in any spawned future; see the
/// module doc.
#[must_use = "the lock set is released as soon as the guard is dropped"]
pub struct RecordWriteGuard {
    _guard: SetGuard,
}

/// RAII guard for the lock sets of a declared record set — the `DBManyLocker`
/// equivalent.
///
/// Acquired by [`PvDatabase::lock_records`] over every member record of a
/// multi-record transaction (QSRV atomic group PUT/GET, pvalink atomic
/// scan-on-update epoch) and held across the whole member loop. While alive,
/// every plain write to any record of any of those sets blocks.
#[must_use = "the locked epoch ends as soon as the guard is dropped"]
pub struct ManyRecordWriteGuard {
    _guards: Vec<SetGuard>,
}

impl PvDatabase {
    /// Acquire the lock set of a single record — the `dbScanLock(precord)`
    /// analogue.
    ///
    /// `record` is alias-resolved here and nowhere else: this is the single
    /// owner of "which lock set does this name name", so an alias and its
    /// target always reach the same set and no caller has to resolve first to
    /// make that true. Resolution borrows — a name that is not an alias, which
    /// is every name in a database that declares none, reaches the lookup
    /// without a copy of itself being made.
    ///
    /// **Blocks the calling thread** when another thread holds the set. A
    /// thread that already holds it recurses, as C's recursive `epicsMutex`
    /// does — which is not optional once a set spans a whole link component,
    /// because processing a record and then its link target takes one mutex
    /// twice.
    pub fn lock_record(&self, record: &str) -> RecordWriteGuard {
        let resolved = self.resolve_alias(record);
        let canonical: &str = resolved.as_deref().unwrap_or(record);
        let lr = self.ensure_set(canonical);
        RecordWriteGuard {
            _guard: lr.acquire(),
        }
    }

    /// C `dbScanLock(precord)` itself — the gate taken by a caller that
    /// already holds the record, which in C is the only form there is.
    ///
    /// Prefer it to [`Self::lock_record`] wherever the record is in hand. It
    /// reads the record's own cell and takes the set's mutex, and does
    /// nothing else; reaching the same set by name costs an alias lookup plus
    /// a hash of the record name under the registry mutex, twice. On the
    /// process path — which always has the record, having just read it out of
    /// the records map — that was 0.85 us of a 11.5 us calc cycle, measured
    /// on 2000 records at 10 Hz.
    pub fn lock_instance(&self, rec: &Arc<RecordCell>) -> RecordWriteGuard {
        let lr = rec.lock_record();
        if lr.is_bootstrap() {
            // First gate this record has ever been given. C mints the set in
            // `dbLockInitRecords` before anything can lock; the port allows a
            // database with no `iocInit` at all, so the set is minted on
            // demand here — once per record, off every later pass. The name
            // is read and the guard dropped BEFORE the move: a guard taken
            // through the bootstrap set must not outlive the record's stay
            // in it.
            let name = rec.read().name.clone();
            self.ensure_set_for(&name, lr);
        }
        RecordWriteGuard {
            _guard: lr.acquire(),
        }
    }

    /// Acquire the lock sets covering a set of records — the `DBManyLock` /
    /// `DBManyLocker` equivalent, C's `dbScanLockMany` (`dbLock.c:384-440`).
    ///
    /// Every name is alias-resolved, mapped to its lock set, then the sets are
    /// sorted by id and de-duplicated before any is taken. Sorting gives two
    /// overlapping transactions the same acquisition order so they cannot
    /// deadlock; de-duplication is C's own — several member records commonly
    /// share one set, and `dbScanLockMany` skips the repeats (`:399-402`).
    ///
    /// The returned [`ManyRecordWriteGuard`] must be held for the whole
    /// transaction. Each set it holds reports one extra ref while it lives,
    /// which is the `+1` C's locked list adds.
    ///
    /// Names that do not resolve to a record still get a set, matching
    /// `dbLockerAlloc`, which accepts the record pointers it is given without
    /// a liveness re-check.
    pub fn lock_records<I, S>(&self, records: I) -> ManyRecordWriteGuard
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let names: Vec<String> = records
            .into_iter()
            .map(|record| {
                let record = record.as_ref();
                self.resolve_alias(record)
                    .unwrap_or_else(|| record.to_string())
            })
            .collect();
        let cells: Vec<Arc<LockRecord>> = names.iter().map(|name| self.ensure_set(name)).collect();

        loop {
            let mut sets: Vec<Set> = cells.iter().map(|lr| lr.set()).collect();
            sets.sort_unstable_by_key(|set| set.id);
            sets.dedup_by_key(|set| set.id);

            let guards: Vec<SetGuard> = sets.iter().map(|set| set.acquire(true)).collect();

            // `dbLockUpdateRefs(locker, 0)` (`dbLock.c:432-436`): if a merge
            // moved any member while the sets were being taken, release
            // everything and start again.
            let held: BTreeSet<u64> = sets.iter().map(|set| set.id).collect();
            if names
                .iter()
                .all(|name| matches!(self.inner.record_locks.set_id_of(name), Some(id) if held.contains(&id)))
            {
                return ManyRecordWriteGuard { _guards: guards };
            }
            drop(guards);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    // These are plain `#[test]`s, not `#[tokio::test]`s, and that is forced
    // rather than stylistic: the gate blocks the calling *thread*, so a
    // contending waiter has to be a real thread. Parking one on a
    // `current_thread` runtime's only worker would wedge the runtime instead
    // of demonstrating exclusion. Being reactor-free they also run on the exec
    // backend and add no site to this file's RTEMS-EXEC-MODEL-ALLOW census.

    /// Long enough that a non-blocking (broken) gate would have let the
    /// contender through, short enough not to dominate the suite.
    const SETTLE: Duration = Duration::from_millis(50);

    /// A single-record gate excludes a concurrent same-record locker.
    #[test]
    fn lock_record_excludes_same_record() {
        let db = PvDatabase::new();
        let order = Arc::new(AtomicUsize::new(0));

        let g = db.lock_record("ai:1");

        let db2 = db.clone();
        let order2 = order.clone();
        let h = std::thread::spawn(move || {
            let _g2 = db2.lock_record("ai:1");
            // This must observe the first holder having released (1).
            order2.fetch_add(10, Ordering::SeqCst);
        });

        // Give the spawned thread time to block on the gate.
        std::thread::sleep(SETTLE);
        // First holder still owns the gate: counter untouched.
        assert_eq!(order.load(Ordering::SeqCst), 0);
        order.fetch_add(1, Ordering::SeqCst);
        drop(g);

        h.join().unwrap();
        assert_eq!(order.load(Ordering::SeqCst), 11);
    }

    /// `lock_records` blocks a plain single-record write to a member.
    #[test]
    fn lock_records_excludes_single_member_write() {
        let db = PvDatabase::new();
        let many = db.lock_records(["g:a", "g:b", "g:c"]);

        let db2 = db.clone();
        let acquired = Arc::new(AtomicUsize::new(0));
        let acquired2 = acquired.clone();
        let h = std::thread::spawn(move || {
            // Plain write to a member must block until `many` drops.
            let _g = db2.lock_record("g:b");
            acquired2.store(1, Ordering::SeqCst);
        });

        std::thread::sleep(SETTLE);
        assert_eq!(
            acquired.load(Ordering::SeqCst),
            0,
            "single-member write must block while ManyRecordWriteGuard is held"
        );

        drop(many);
        h.join().unwrap();
        assert_eq!(acquired.load(Ordering::SeqCst), 1);
    }

    /// Two overlapping `lock_records` sets acquire in canonical order
    /// and therefore cannot deadlock even with reversed input order.
    ///
    /// With blocking gates a violated order wedges both threads outright,
    /// which is why this runs the two sets on real threads and joins them
    /// under a bounded wait rather than trusting a scheduler yield.
    #[test]
    fn lock_records_overlapping_sets_no_deadlock() {
        let db = PvDatabase::new();
        let done = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = [["x", "y", "z"], ["z", "y", "x"]]
            .into_iter()
            .map(|set| {
                let db = db.clone();
                let done = done.clone();
                std::thread::spawn(move || {
                    for _ in 0..500 {
                        // Reversed input order on one side — sort makes the
                        // real acquisition order identical, so no deadlock.
                        let _g = db.lock_records(set);
                        std::thread::yield_now();
                    }
                    done.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect();

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while done.load(Ordering::SeqCst) < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "overlapping lock_records sets must not deadlock"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    /// An epoch over a record set excludes a second epoch that shares
    /// any record until the first guard drops — and overlapping sets
    /// listed in opposite orders never deadlock (sorted acquisition).
    #[test]
    fn overlapping_epochs_are_mutually_exclusive_and_deadlock_free() {
        let db = PvDatabase::new();
        let a = vec!["RECA".to_string(), "RECB".to_string()];
        // Opposite order on purpose — sorted acquisition must still
        // make this safe.
        let b = vec!["RECB".to_string(), "RECC".to_string()];

        let guard_a = db.lock_records(&a);

        // A second epoch sharing RECB must not be acquirable while
        // `guard_a` is alive.
        let db2 = db.clone();
        let entered = Arc::new(AtomicUsize::new(0));
        let entered2 = entered.clone();
        let handle = std::thread::spawn(move || {
            let _guard_b = db2.lock_records(&b);
            entered2.store(1, Ordering::SeqCst);
        });

        std::thread::sleep(SETTLE);
        assert_eq!(
            entered.load(Ordering::SeqCst),
            0,
            "epoch B must block on shared RECB"
        );

        drop(guard_a);
        handle.join().expect("epoch B thread");
        assert_eq!(entered.load(Ordering::SeqCst), 1);
    }

    /// Two non-overlapping epochs run concurrently — no false
    /// serialisation. Taking the second on *this* thread while the first is
    /// still held is the assertion: a gate keyed too coarsely would
    /// self-deadlock here rather than merely being slow.
    #[test]
    fn disjoint_epochs_do_not_block_each_other() {
        let db = PvDatabase::new();
        let _g1 = db.lock_records(&["X1".to_string()]);
        // Disjoint set: must acquire immediately without blocking.
        let _g2 = db.lock_records(&["X2".to_string()]);
    }

    /// One name reaches one set, and two unlinked records reach two — C's
    /// state straight after `dbLockInitRecords`, before any link merges.
    #[test]
    fn a_name_reaches_one_set_and_two_unlinked_records_reach_two() {
        let db = PvDatabase::new();
        assert!(
            std::ptr::eq(db.ensure_set("REC:A").set(), db.ensure_set("REC:A").set()),
            "the same canonical name must map to the same lock set"
        );
        assert!(
            !std::ptr::eq(db.ensure_set("REC:A").set(), db.ensure_set("REC:B").set()),
            "records no link joins must not share a lock set"
        );
    }

    /// A second epoch overlapping the first on one thread recurses rather
    /// than wedging. C's refusal (`cantProceed("dbScanLockMany(%p) already
    /// locked...")`, `dbLock.c:392-395`) is about re-using ONE `dbLocker`,
    /// which has no analogue here — every call builds its own — while the
    /// overlap itself is what C's recursive `epicsMutex` absorbs.
    #[test]
    fn a_second_overlapping_epoch_on_one_thread_recurses() {
        let db = PvDatabase::new();
        let _epoch = db.lock_records(&["RE:A".to_string(), "RE:B".to_string()]);
        let _overlapping = db.lock_records(&["RE:B".to_string(), "RE:C".to_string()]);
    }

    /// `dbScanLock` recurses because `epicsMutex` must (`epicsMutex.h:16`,
    /// `:38`), and once a lock set spans a link component the port has no
    /// choice either: processing a record and then its link target is this
    /// sequence with two different names behind one mutex.
    #[test]
    fn re_taking_one_record_s_set_recurses_like_db_scan_lock() {
        let db = PvDatabase::new();
        let _held = db.lock_record("RE:SELF");
        let _again = db.lock_record("RE:SELF");
    }

    /// The recursion is per THREAD: a second thread still blocks, and the
    /// depth the first one built up does not let it through early.
    #[test]
    fn recursion_does_not_let_a_second_thread_in() {
        let db = PvDatabase::new();
        let outer = db.lock_record("RE:DEPTH");
        let inner = db.lock_record("RE:DEPTH");

        let db2 = db.clone();
        let entered = Arc::new(AtomicUsize::new(0));
        let entered2 = entered.clone();
        let h = std::thread::spawn(move || {
            let _g = db2.lock_record("RE:DEPTH");
            entered2.store(1, Ordering::SeqCst);
        });

        std::thread::sleep(SETTLE);
        assert_eq!(entered.load(Ordering::SeqCst), 0, "outer level still held");
        drop(inner);
        std::thread::sleep(SETTLE);
        assert_eq!(
            entered.load(Ordering::SeqCst),
            0,
            "one release of two must not hand the set over"
        );
        drop(outer);
        h.join().unwrap();
        assert_eq!(entered.load(Ordering::SeqCst), 1);
    }

    /// The positional association between a set and its `epicsMutexShow` row
    /// is only exact while `make_set` is the ONLY `PriorityInheritanceMutex`
    /// created in this file. This is that check: one row for every set ever
    /// made in this process, and not one more.
    #[test]
    fn this_file_creates_no_mutex_but_lock_sets() {
        let db = PvDatabase::new();
        for name in ["MS:1", "MS:2", "MS:3"] {
            drop(db.lock_record(name));
        }
        let made = *SET_MUTEX_SEQ.lock().unwrap();
        assert_eq!(
            lock_set_mutex_rows().len() as u64,
            made,
            "a second mutex created in this file would shift every set's row"
        );
        for set in db.lock_set_report().active {
            assert!(set.mutex.is_some(), "set {} has no row", set.id);
        }
    }

    /// `RecordCell::read_in` rides the set its caller holds when the target
    /// is in that set — C's `dbScanLock` on a member of the held set is a
    /// recursion, which the port skips outright — and takes the target's own
    /// set when it is not.
    #[test]
    fn read_in_rides_the_held_set_and_takes_another() {
        use crate::server::record::{RecordCell, RecordInstance};
        use crate::server::records::calc::CalcRecord;
        let db = PvDatabase::new();
        let cell = |name: &str| {
            let cell = Arc::new(RecordCell::new(RecordInstance::new(
                name.into(),
                CalcRecord::default(),
            )));
            db.inner.record_locks.adopt(name, cell.lock_record());
            db.ensure_set_for(name, cell.lock_record());
            cell
        };
        let reader = cell("RI:A");
        let linked = cell("RI:B");
        let apart = cell("RI:C");
        db.merge_sets("RI:A", "RI:B");

        let mut held = reader.write();
        let (_, set) = held.split();
        assert!(
            linked.read_in(set).rides_held_set(),
            "a record of the held set must be read without a second hold"
        );
        assert!(
            !apart.read_in(set).rides_held_set(),
            "a record of another set must take that set"
        );
    }

    /// The set is released with the guard, so the ordinary sequential
    /// pattern — lock, write, drop, lock again — is untouched.
    #[test]
    fn the_set_is_released_when_the_guard_drops() {
        let db = PvDatabase::new();
        drop(db.lock_record("RE:SEQ"));
        drop(db.lock_record("RE:SEQ"));
        drop(db.lock_records(&["RE:SEQ".to_string()]));
        // And a disjoint pair may be held together on one thread.
        let _a = db.lock_record("RE:ONE");
        let _b = db.lock_record("RE:TWO");
    }

    /// A record cell as `add_loaded_record` hands it to the registry:
    /// adopted, not yet in the records map, no lock taken through it.
    fn adopted_cell(db: &PvDatabase, name: &str) -> Arc<crate::server::record::RecordCell> {
        use crate::server::record::{RecordCell, RecordInstance};
        use crate::server::records::calc::CalcRecord;
        let cell = Arc::new(RecordCell::new(RecordInstance::new(
            name.into(),
            CalcRecord::default(),
        )));
        db.inner.record_locks.adopt(name, cell.lock_record());
        cell
    }

    /// Boundary `built == false`: a record adopted before `iocInit` waits on
    /// the bootstrap set, as C's null `lset` does until `dbLockInitRecords`.
    #[test]
    fn adopt_before_build_leaves_the_record_on_the_bootstrap_set() {
        let db = PvDatabase::new();
        let cell = adopted_cell(&db, "AD:PRE");
        assert!(cell.lock_record().is_bootstrap());
        assert_eq!(db.inner.record_locks.set_id_of("AD:PRE"), None);
    }

    /// Boundary `built == true`: a record adopted after `iocInit` has a real
    /// set before anything can lock it, so no relink has to reach for the
    /// bootstrap set on its behalf.
    #[test]
    fn adopt_after_build_mints_the_record_s_set() {
        let db = PvDatabase::new();
        db.build_lock_sets();
        let cell = adopted_cell(&db, "AD:POST");
        assert!(!cell.lock_record().is_bootstrap());
        assert_eq!(
            db.inner.record_locks.set_id_of("AD:POST"),
            Some(cell.lock_record().set().id)
        );
    }

    /// A cell adopted before `build_lock_sets` but absent from the records
    /// map it reads its names from is still minted by `init_sets`: `built`
    /// promises no registered record is on the bootstrap set.
    #[test]
    fn build_lock_sets_mints_every_adopted_cell() {
        let db = PvDatabase::new();
        let cell = adopted_cell(&db, "AD:STRAGGLER");
        assert!(cell.lock_record().is_bootstrap());
        db.build_lock_sets();
        assert!(!cell.lock_record().is_bootstrap());
        assert!(db.inner.record_locks.set_id_of("AD:STRAGGLER").is_some());
    }

    /// Boundary: a fresh set taken under a records-map read guard is the
    /// reader `remove_record_entry` can be waiting on while it holds that
    /// set. Debug builds fail on the offending thread.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "read guard on the records map")]
    fn a_fresh_set_under_the_map_read_guard_is_refused() {
        let db = PvDatabase::new();
        let _map = db.inner.records.read();
        let _g = db.lock_record("MR:FRESH");
    }

    /// Boundary: the same under the alias table's guard.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "read guard on the records map")]
    fn a_fresh_set_under_the_alias_read_guard_is_refused() {
        let db = PvDatabase::new();
        let _map = db.inner.aliases.read();
        let _g = db.lock_record("MR:ALIAS");
    }

    /// Boundary: a set this thread already holds is re-entered, not taken,
    /// so reading the map inside a held set and reaching the same record
    /// again — every `get_record` on the process path — is untouched. And
    /// once the guard is dropped, a fresh set is ordinary again.
    #[test]
    fn re_entry_under_the_map_read_guard_and_a_fresh_set_after_it_are_fine() {
        let db = PvDatabase::new();
        let _held = db.lock_record("MR:HELD");
        {
            let _map = db.inner.records.read();
            drop(db.lock_record("MR:HELD"));
        }
        drop(db.lock_record("MR:OTHER"));
    }

    /// The set a `Membership` relink takes never includes the bootstrap set,
    /// so no relink can invert id order against a `LinkEdit` relink that
    /// enters already holding a seed's set.
    #[test]
    fn every_set_after_build_excludes_the_bootstrap_set() {
        let db = PvDatabase::new();
        adopted_cell(&db, "AD:ES1");
        db.build_lock_sets();
        adopted_cell(&db, "AD:ES2");
        let registry = db.inner.record_locks.lock();
        let sets = registry.every_set();
        assert_eq!(sets.len(), 2);
        assert!(
            sets.iter().all(|set| !std::ptr::eq(*set, bootstrap_set())),
            "every_set() must not hand a relink the bootstrap set"
        );
    }
}
