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
//! **A link put is C's pair.** `dbPutFieldLink` splits the record's set from
//! the field's old target and then merges it with the new one
//! (`dbAccess.c:1170`, `:1208`; `dbDbLink.c:124`, `:141`), and that pair
//! decides more than the grouping: the split side's breadth-first order
//! (`dbLock.c:717-790`), which set `makeSet` hands it, and where the set a
//! merge empties lands on the free list are all in `dblsr`'s output. So a
//! [`RelinkScope::LinkEdit`] replays it ([`Registry::relink_field`]). The old
//! target is read from the field before the write, and the split walks the
//! live link text without the written field — the graph between C's two
//! calls.
//!
//! A change C has no verb for — a record added, removed or re-aliased after
//! `iocInit`, or a device's lock partners — re-derives the affected
//! component from the live link text and re-partitions it
//! ([`Registry::repartition`]). Its set ids follow C's rules: the component
//! holding the seed keeps its id, a component that splits off takes a set
//! from the free list as `makeSet` does, and the sets it empties go back on
//! the free list.
//!
//! `field_io.rs`'s `NotifyClaim` is unchanged by this: it closes dbNotify's
//! test-then-install window inside the critical section that tested the slot,
//! which is a stronger guarantee than the lock-set region it was standing in
//! for, not a substitute that lock sets now make unnecessary.
//!
//! What the gate *is* — a blocking priority-inheritance mutex
//! ----------------------------------------------------------
//! The gate is a [`crate::runtime::sync::PriorityInheritanceMutex`], the
//! same primitive L8a (`simple_pvs`) and L8b (one `scan_index` bucket)
//! already use, and [`PvDatabase::lock_record`] /
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
//! >    QSRV per-group atomic-PUT gate (`UnlistedPriorityInheritanceMutex`). Outside
//! >    this crate, and only the atomic group PUT takes it — see the L33
//! >    section below.
//! > 1. **L1** — the per-record advisory gate ([`PvDatabase::lock_record`] /
//! >    [`PvDatabase::lock_records`]), *this* module
//! >    (`PriorityInheritanceMutex`).
//! > 2. **L46** — `PvDatabaseInner::registration_mutex`
//! >    (`UnlistedPriorityInheritanceMutex`).
//! > 3. the leaves, none of which is ever held while another lock is taken:
//! >    **L8a** `simple_pvs`, **L8b** one `scan_index` bucket (both
//! >    `PriorityInheritanceMutex`) and **L7** `ProcessVariable::subscribers`
//! >    (`SubscriberList`), plus
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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};

use crate::server::record::RecordCell;
use crate::server::record::record_instance::{HeldMut, RecordRef};

use crate::runtime::sync::{
    CMutex, MutexInfo, PriorityInheritanceMutex, PriorityInheritanceMutexGuard,
    UnlistedPriorityInheritanceMutex, mutex_report,
};

use super::PvDatabase;
use crate::types::RecordName;

/// C's `lockSet` (`dbLockPvt.h:29-44`): the one mutex every member record
/// locks through, plus the bookkeeping the guards need without the registry.
///
/// The membership list is NOT here — a merge rewrites it, and it lives in
/// [`Registry`] behind the registry mutex. What a guard needs at release time
/// is here instead, so releasing never takes the registry lock: doing that
/// while holding the set would close a cycle against [`Registry::real_set_of`],
/// which takes the registry lock and then the set.
///
/// `repr(C)` in the order the compiler had chosen, with [`Self::trace`] last:
/// the scan walk prefetches only the fields before it, which a cycle reads.
#[repr(C)]
struct LockSet {
    /// C's `lockSet::lock` (`dbLockPvt.h:32`) — the mutex `dbScanLock` takes.
    lock: PriorityInheritanceMutex<()>,
    /// C's `lockSet::id` (`:33`). Assigned once by
    /// [`Registry::make_set`] and kept for the cell's whole life, free-list
    /// round trips included, exactly as C's is.
    id: u64,
    /// The address the process mutex list lists `lock` under, so
    /// [`mutex_rows_by_addr`] can hand back the row `epicsMutexShow` prints
    /// for this set and no other.
    mutex_addr: usize,
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
    many_holds: AtomicU32,
    /// [`Self::owed`] is not empty. Read by every fresh hold, so it shares
    /// `many_holds`' word: the fields before [`Self::trace`] stay one line.
    has_owed: AtomicBool,
    /// [`OWED_CALLBACKS`]: [`Self::callbacks`] is not empty; [`OWED_READS`]:
    /// [`Self::reads`] is not. One byte, so every outermost release reads
    /// both with the one load, in `has_owed`'s padding.
    release_owed: AtomicU8,
    /// The mutex guard of the current outermost hold, kept here rather than
    /// in that hold's [`SetGuard`] so that a frame which does not own the
    /// guard can still give the set up and take it back — [`Self::unheld`],
    /// C's `dbScanUnlock` from inside `dbBkpt` (`dbBkpt.c:795`). Touched only
    /// by the thread named in `owner`, under the mutex itself.
    held: std::cell::UnsafeCell<Option<PriorityInheritanceMutexGuard<'static, ()>>>,
    /// C's `lockSet::trace` (`dbLockPvt.h`): the record whose TPRO
    /// `dbProcess` is running under this set, as [`InFrame::trace_enter`]'s
    /// token, or 0. Written only under `lock`.
    trace: AtomicUsize,
    /// Records whose `cvt_dbaddr` a lock-free resolve found busy
    /// ([`owe_cvt`]): the next fresh hold of this set runs it
    /// ([`Self::admit_owed`]) before the holder reads any of them. A leaf
    /// lock, taken with or without the set.
    owed: std::sync::Mutex<Vec<Arc<RecordCell>>>,
    /// Driver callbacks that reached a member record on the holding thread
    /// while it had the record borrowed or lent to a dset call
    /// ([`owe_callback`]), in arrival order: the record's frame runs its own
    /// when the dset call returns ([`InFrame::run_owed_callbacks`]), and the
    /// outermost release runs whatever is left before the mutex goes.
    callbacks: std::sync::Mutex<std::collections::VecDeque<(Arc<RecordCell>, OwedCallback)>>,
    /// Reads of member records that found the set held ([`owe_read`]),
    /// served by the outermost release, or by the holder from where it waits
    /// for a read of its own ([`ReadWait`]).
    reads: std::sync::Mutex<Vec<Arc<dyn OwedRead>>>,
}

/// [`LockSet::release_owed`]'s bits.
const OWED_CALLBACKS: u8 = 1;
const OWED_READS: u8 = 2;

/// A driver callback [`owe_callback`] deferred: run once its record is free.
pub(crate) type OwedCallback = Box<dyn FnOnce() + Send>;

/// A read of a member record that found its set held — C's lock-free
/// `dbGet` (`dbAccess.c:909`) — run by a thread that holds the set.
pub(crate) trait OwedRead: Send + Sync {
    /// The record read.
    fn cell(&self) -> &RecordCell;
    /// Read now. `own` is the serving thread's own record, borrowed by its
    /// processing, when it serves from a [`ReadWait`].
    fn serve(&self, own: Option<(&RecordCell, crate::server::record::OwnRecord<'_>)>);
    /// Whether the read has its answer.
    fn answered(&self) -> bool;
    /// Answer the read with nothing, unless it has its answer already.
    fn claim(&self) -> bool;
}

// SAFETY: `held` is the one non-`Sync` field, and it is read or written only
// by the thread that holds `lock` (`acquire`'s fresh arm, `SetGuard::drop`'s
// outermost arm, and `unheld`, each of which checks or establishes
// `owner == thread_key()` first). The guard never leaves that thread: the
// same thread that stashes it takes it out, so the `!Send` guard's
// unlock-by-owner rule holds.
unsafe impl Sync for LockSet {}

// The scan walk prefetches the fields before `trace` as one line.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::offset_of!(LockSet, trace) == 64);

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
    hints: CycleHints,
}

/// What a record's next cycle reads past its cell and lock set, for the scan
/// walk to prefetch records ahead without their hold (rs2001 tail). The body
/// is named once at registration ([`RecordCell::indexed`]), the rest by the
/// record's own frame under its hold ([`InFrame::publish_hints`]).
#[derive(Default)]
pub(crate) struct CycleHints {
    pub(crate) body: crate::server::record::Hint,
    /// The OUT link's parse, then its `DbLink`.
    pub(crate) out: [crate::server::record::Hint; 2],
    /// For each of the first two monitors: its event queue, then its slot.
    pub(crate) posts: [[crate::server::record::Hint; 2]; 2],
}

impl LockRecord {
    /// C `*dbLockSetAddrTrace(precord)`, read by a thread that holds the set.
    #[inline]
    pub(crate) fn tracing(&self) -> bool {
        self.set().trace.load(Ordering::Relaxed) != 0
    }

    fn new(set: Set) -> Arc<Self> {
        Arc::new(Self {
            plock_set: AtomicPtr::new(Self::as_ptr(set)),
            hints: CycleHints::default(),
        })
    }

    /// Hint the lock set this record names and what its next cycle reads.
    #[inline(always)]
    pub(crate) fn prefetch_targets(&self) {
        let set = self.plock_set.load(Ordering::Relaxed);
        crate::server::record::prefetch_span(
            set.cast_const().cast(),
            std::mem::offset_of!(LockSet, trace),
            align_of::<LockSet>(),
        );
        let h = &self.hints;
        h.body.prefetch();
        h.out[0].prefetch();
        h.out[1].prefetch();
        h.posts[0][0].prefetch();
        h.posts[0][1].prefetch();
        h.posts[1][0].prefetch();
        h.posts[1][1].prefetch();
    }

    pub(crate) fn hints(&self) -> &CycleHints {
        &self.hints
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
        // The bootstrap set is the one set whose id is 0 (`FIRST_SET_ID` is 2),
        // so the question is one load, not a `OnceLock` probe.
        self.set().id == 0
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
        let me = thread_key();
        if set.owner.load(Ordering::Acquire) == me {
            // This thread holds the set, so by Rule R the record cannot leave
            // it: no re-check, and nothing but the depth counter to touch.
            // This is every `rec.read()` / `rec.write()` inside a process
            // cycle, which is why it is inlined and the rest is not.
            return set.reenter();
        }
        self.acquire_fresh(me)
    }

    /// [`Self::acquire`] without waiting: `None` while another thread holds
    /// the set, or when a merge moved the record off the set just taken.
    pub(crate) fn try_acquire(&self) -> Option<SetGuard> {
        let set = self.set();
        let me = thread_key();
        if set.owner.load(Ordering::Acquire) == me {
            return Some(set.reenter());
        }
        let guard = set.try_lock_fresh(me)?;
        if !std::ptr::eq(self.set(), set) {
            return None;
        }
        set.admit_owed();
        Some(guard)
    }

    /// The set was not held. The common case is a free mutex and a record
    /// that has not moved: one `try_lock`, the publication, one re-check of
    /// the cell, and none of the loop's saved registers.
    #[inline(never)]
    fn acquire_fresh(&self, me: u64) -> SetGuard {
        let set = self.set();
        if let Some(guard) = set.try_lock_fresh(me) {
            if std::ptr::eq(self.set(), set) {
                // After the re-check, where only the set is live, so the
                // test costs no saved register.
                set.admit_owed();
                return guard;
            }
        }
        self.acquire_contended(me)
    }

    /// Take whatever set the record is behind by the time the mutex is ours
    /// — a merge can move the record while this thread waits, and the set it
    /// then holds would be the wrong one. A hold [`Self::acquire_fresh`]
    /// took and lost the re-check on is given back by dropping it first.
    #[cold]
    #[inline(never)]
    fn acquire_contended(&self, me: u64) -> SetGuard {
        loop {
            let set = self.set();
            let guard = set.acquire_as(me);
            if std::ptr::eq(self.set(), set) {
                return guard;
            }
            drop(guard);
        }
    }

    /// Whether this thread holds the set the record is in. Stable while it
    /// does: a record cannot leave a held set.
    pub(crate) fn held_by_this_thread(&self) -> bool {
        self.set().owner.load(Ordering::Acquire) == thread_key()
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

/// Owe `job`, a driver callback for `cell`, to the set this thread holds it
/// under: C's recursive `dbScanLock` would run it inside the dset call, which
/// here has the record borrowed or its device out.
pub(crate) fn owe_callback(cell: &Arc<RecordCell>, job: OwedCallback) {
    let set = cell.lock_record().set();
    debug_assert_eq!(set.owner.load(Ordering::Acquire), thread_key());
    let mut callbacks = set.callbacks.lock().unwrap_or_else(|e| e.into_inner());
    callbacks.push_back((Arc::clone(cell), job));
    set.release_owed.fetch_or(OWED_CALLBACKS, Ordering::Relaxed);
}

/// Owe `read` to the set its record is in, held by another thread: its
/// outermost release serves it, and a holder waiting in a [`ReadWait`] is
/// handed the set to serve it from there. A record a merge moves meanwhile
/// is left to the reader's own retry.
pub(crate) fn owe_read(read: Arc<dyn OwedRead>) {
    let set = read.cell().lock_record().set();
    set.reads
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(read);
    set.release_owed.fetch_or(OWED_READS, Ordering::Release);
    let owner = set.owner.load(Ordering::Acquire);
    let waits = WAITS.lock();
    if let Some(wait) = waits.reading.iter().find(|w| w.key == owner) {
        wait.owed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(set);
        wait.thread.unpark();
    }
}

/// A thread in a [`ReadWait`], and the sets it holds that were owed reads
/// since it last served them.
struct Waiter {
    key: u64,
    thread: std::thread::Thread,
    read: Arc<dyn OwedRead>,
    owed: std::sync::Mutex<Vec<Set>>,
}

/// A thread waiting in [`LockSet::lock_taking`] for the set `taking`, null
/// once it has it. It lives on that thread's stack, linked into
/// [`Waits::taking`] for the wait only, so a wait allocates nothing.
struct Taker {
    key: u64,
    taking: AtomicPtr<LockSet>,
    /// The next linked taker, under [`WAITS`].
    next: AtomicPtr<Taker>,
}

/// A [`Taker`] linked into [`Waits::taking`] while this lives.
struct Linked<'a>(&'a Taker);

impl<'a> Linked<'a> {
    fn new(taker: &'a Taker) -> Self {
        let waits = WAITS.lock();
        taker
            .next
            .store(waits.taking.load(Ordering::Relaxed), Ordering::Relaxed);
        waits
            .taking
            .store(std::ptr::from_ref(taker).cast_mut(), Ordering::Relaxed);
        Self(taker)
    }
}

impl Drop for Linked<'_> {
    fn drop(&mut self) {
        let waits = WAITS.lock();
        let me = std::ptr::from_ref(self.0).cast_mut();
        let mut link = &waits.taking;
        while link.load(Ordering::Relaxed) != me {
            // SAFETY: as in `Waits::takers`; this taker is linked, so the
            // walk meets it before the end.
            link = unsafe { &(*link.load(Ordering::Relaxed)).next };
        }
        link.store(self.0.next.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

/// The threads that wait while they may hold sets.
struct Waits {
    reading: Vec<Arc<Waiter>>,
    /// The first linked [`Taker`].
    taking: AtomicPtr<Taker>,
}

impl Waits {
    fn takers(&self) -> impl Iterator<Item = &Taker> {
        let mut next = self.taking.load(Ordering::Relaxed);
        std::iter::from_fn(move || {
            // SAFETY: a taker unlinks under `WAITS` before its frame ends, and
            // `self` is reached only through that lock.
            let taker = unsafe { next.as_ref() }?;
            next = taker.next.load(Ordering::Relaxed);
            Some(taker)
        })
    }
}

/// Priority-inheriting, since a contended [`LockSet::lock_fresh`] takes it;
/// built by [`new_set`], so no wait builds it.
static WAITS: std::sync::LazyLock<UnlistedPriorityInheritanceMutex<Waits>> =
    std::sync::LazyLock::new(|| {
        UnlistedPriorityInheritanceMutex::new(Waits {
            reading: Vec::new(),
            taking: AtomicPtr::new(std::ptr::null_mut()),
        })
    });

/// One link of the chain [`ReadWait::stuck`] follows.
enum Hop<'a> {
    /// A holder waiting for this set.
    Takes(&'a Taker, Set),
    /// A holder waiting for this read.
    Reads(&'a Waiter),
}

/// This thread waiting for `read`, which it owed: a read owed to any set it
/// holds is served from here ([`Self::serve`]), its own record's or not, and
/// whether owed before the wait began or after. Two holders reading each
/// other's records would otherwise wait for good. Taken before `read` is
/// owed; given up when dropped.
pub(crate) struct ReadWait(Arc<Waiter>);

impl ReadWait {
    pub(crate) fn new(read: Arc<dyn OwedRead>) -> Self {
        let key = thread_key();
        let mut waits = WAITS.lock();
        // Every owed read's reader waits in a `ReadWait`, so the reads owed
        // to this thread's sets before now are among theirs.
        let owed = waits
            .reading
            .iter()
            .map(|w| w.read.cell().lock_record().set())
            .filter(|set| set.owner.load(Ordering::Acquire) == key)
            .collect();
        let wait = Arc::new(Waiter {
            key,
            thread: std::thread::current(),
            read,
            owed: std::sync::Mutex::new(owed),
        });
        waits.reading.push(Arc::clone(&wait));
        Self(wait)
    }

    /// Serve the reads owed to the sets this thread holds. `own` is the
    /// record the thread is inside, read through the view it lends.
    pub(crate) fn serve(&self, own: Option<(&RecordCell, crate::server::record::OwnRecord<'_>)>) {
        let sets = std::mem::take(&mut *self.0.owed.lock().unwrap_or_else(|e| e.into_inner()));
        for set in sets {
            // Released since it was owed: its next holder's release serves it.
            if set.owner.load(Ordering::Acquire) != self.0.key {
                continue;
            }
            for read in set.take_reads() {
                read.serve(own);
            }
        }
    }

    /// Whether the holder of the record read waits in
    /// [`LockSet::lock_fresh`] for a set this thread holds, directly or
    /// through holders that wait in turn: it can neither serve the read nor
    /// let go, and the read is claimed, unanswered, for the caller to fail.
    /// A holder waiting on a read of its own serves this one, so a chain
    /// ends free at one, or at a read this thread serves.
    pub(crate) fn stuck(&self) -> bool {
        let me = self.0.key;
        let waits = WAITS.lock();
        let owner = |set: Set| set.owner.load(Ordering::Acquire);
        let read_owner = |w: &Waiter| owner(w.read.cell().lock_record().set());
        let mut chain = Vec::new();
        let mut holder = read_owner(&self.0);
        for _ in 0..waits.takers().count() {
            let Some(taker) = waits.takers().find(|t| t.key == holder) else {
                return false;
            };
            let set = taker.taking.load(Ordering::Acquire);
            if set.is_null() {
                return false;
            }
            // SAFETY: `lock_fresh` publishes its `&'static self`.
            let set: Set = unsafe { &*set };
            chain.push(Hop::Takes(taker, set));
            holder = owner(set);
            if holder == me {
                break;
            }
            if let Some(reader) = waits.reading.iter().find(|w| w.key == holder) {
                holder = read_owner(reader);
                if reader.read.answered() || holder == me {
                    return false;
                }
                chain.push(Hop::Reads(reader));
            }
        }
        if holder != me {
            return false;
        }
        // Read again from the last hop back: the last taker waits for a set
        // this thread keeps, so it stays put, and so then does each holder
        // it waits behind.
        let mut next = me;
        for hop in chain.iter().rev() {
            next = match *hop {
                Hop::Takes(taker, set) => {
                    if !std::ptr::eq(taker.taking.load(Ordering::Acquire), set)
                        || owner(set) != next
                    {
                        return false;
                    }
                    taker.key
                }
                Hop::Reads(reader) => {
                    if read_owner(reader) != next || reader.read.answered() {
                        return false;
                    }
                    reader.key
                }
            };
        }
        read_owner(&self.0) == next && self.0.read.claim()
    }
}

impl Drop for ReadWait {
    fn drop(&mut self) {
        WAITS.lock().reading.retain(|w| !Arc::ptr_eq(w, &self.0));
    }
}

/// Owe `cell`'s pending `cvt_dbaddr` to the set it is in, for that set's next
/// fresh hold — a lock-free `dbNameToAddr` that found the record busy. The
/// re-check closes a merge that moved the record between the read of its set
/// and the push: the entry is pushed again behind the new set, and the stale
/// one is skipped by the drain.
pub(crate) fn owe_cvt(cell: &Arc<RecordCell>) {
    loop {
        let set = cell.lock_record().set();
        {
            let mut owed = set.owed.lock().unwrap_or_else(|e| e.into_inner());
            owed.push(Arc::clone(cell));
            set.has_owed.store(true, Ordering::Release);
        }
        if std::ptr::eq(cell.lock_record().set(), set) {
            return;
        }
    }
}

/// Test-only: run a closure each time this thread takes a record's set
/// fresh, i.e. after every point the set was free for another thread to
/// take. A window between two holds that no sleep can hit deterministically
/// is reached here by acting as that other thread would.
#[cfg(feature = "test-hooks")]
pub mod fresh_hold_hook {
    use super::{LockSet, RecordCell};
    use std::cell::RefCell;
    use std::sync::Arc;

    type Hook = (Arc<RecordCell>, Box<dyn FnMut()>);

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Disarms the hook when dropped.
    pub struct Armed(());

    impl Drop for Armed {
        fn drop(&mut self) {
            HOOK.with(|h| h.borrow_mut().take());
        }
    }

    /// Run `f` on this thread at every fresh hold of `record`'s set, with
    /// the set held and no member borrowed.
    pub fn arm(record: Arc<RecordCell>, f: impl FnMut() + 'static) -> Armed {
        HOOK.with(|h| *h.borrow_mut() = Some((record, Box::new(f))));
        Armed(())
    }

    pub(super) fn fire(set: &'static LockSet) {
        // Taken out for the call, so a fresh hold inside `f` does not re-enter.
        let Some((record, mut f)) = HOOK.with(|h| h.borrow_mut().take()) else {
            return;
        };
        if std::ptr::eq(record.lock_record().set(), set) {
            f();
        }
        HOOK.with(|h| {
            let mut h = h.borrow_mut();
            if h.is_none() {
                *h = Some((record, f));
            }
        });
    }
}

/// C's `next_id` starts at 1 and `makeSet` uses the POST-increment
/// (`dbLock.c:70`, `:87`), so C's first lock set is number 2. Matching it
/// costs nothing and makes an A/B against a C IOC read straight across.
const FIRST_SET_ID: u64 = 2;

/// Mint one set. Every `LockSet` in the process comes from here.
fn new_set(id: u64) -> Set {
    std::sync::LazyLock::force(&WAITS);
    let lock = PriorityInheritanceMutex::new((), CMutex::LockSet);
    // The mutex names its own row, so no two sets can be confused for each
    // other and nothing has to be serialised against the process mutex list.
    let mutex_addr = lock.registered_addr();
    Box::leak(Box::new(LockSet {
        id,
        lock,
        mutex_addr,
        owner: AtomicU64::new(0),
        depth: AtomicUsize::new(0),
        many_holds: AtomicU32::new(0),
        has_owed: AtomicBool::new(false),
        release_owed: AtomicU8::new(0),
        held: std::cell::UnsafeCell::new(None),
        trace: AtomicUsize::new(0),
        owed: std::sync::Mutex::new(Vec::new()),
        callbacks: std::sync::Mutex::new(std::collections::VecDeque::new()),
        reads: std::sync::Mutex::new(Vec::new()),
    }))
}

/// The set every record locks through until a registry gives it one — C's
/// null `lset` made lockable, so that a record's data is guarded by a lock
/// set from its first access rather than only from `iocInit` on.
///
/// One per process, never in any registry's `active` or `free` list, and
/// its id `0` is never a set id (`FIRST_SET_ID` is 2). It comes from
/// [`new_set`] like every other set, so it holds a row in the process mutex
/// list of its own — see [`mutex_rows_by_addr`].
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

#[cfg(debug_assertions)]
thread_local! {
    /// How many lock sets this thread holds — counted where the mutex is
    /// taken and released, so no body run under a set can miss it.
    static SETS_HELD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many [`InitPass`] scopes this thread is inside.
    static INIT_PASS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether this thread is under a record's hold as C `dbScanLock` holds it, in
/// a debug build; `None` in a release build, which does not count. For a
/// caller that must never wait on another thread under the hold. An
/// `InitPass` is not such a hold: the set it takes only guards the data.
#[inline]
pub fn scan_lock_held() -> Option<bool> {
    #[cfg(debug_assertions)]
    return Some(
        SETS_HELD.with(std::cell::Cell::get) > 0 && INIT_PASS.with(std::cell::Cell::get) == 0,
    );
    #[cfg(not(debug_assertions))]
    None
}

/// `iocInit`'s `init_record` walks, which C runs with no `dbScanLock`
/// (`iocInit.c:508-527`, `:545-570`): device support may wait on its port
/// there (devAsyn's SyncIO readback).
pub(crate) struct InitPass(());

impl InitPass {
    pub(crate) fn enter() -> Self {
        #[cfg(debug_assertions)]
        INIT_PASS.with(|n| n.set(n.get() + 1));
        Self(())
    }
}

impl Drop for InitPass {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        INIT_PASS.with(|n| n.set(n.get() - 1));
    }
}

#[cfg(debug_assertions)]
thread_local! {
    /// How many [`sync_driver_wait`] calls this thread is inside.
    static SYNC_DRIVER_WAIT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Run `f`, a driver call C makes inline under `dbScanLock` that may wait on
/// a can-block port: asynRecord's `drvUserCreate` (asynRecord.c:1249) and
/// devScalerAsyn's reset/preset/arm, which a synchronous port (drvScaler974)
/// serves through SyncIO. A caller list is a workaround until the asyn
/// driver-model change.
pub fn sync_driver_wait<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(debug_assertions)]
    struct Scope;
    #[cfg(debug_assertions)]
    impl Drop for Scope {
        fn drop(&mut self) {
            SYNC_DRIVER_WAIT.with(|n| n.set(n.get() - 1));
        }
    }
    #[cfg(debug_assertions)]
    SYNC_DRIVER_WAIT.with(|n| n.set(n.get() + 1));
    #[cfg(debug_assertions)]
    let _scope = Scope;
    f()
}

/// Whether this thread is inside [`sync_driver_wait`]; `false` in a release
/// build, which does not count.
#[inline]
pub fn in_sync_driver_wait() -> bool {
    #[cfg(debug_assertions)]
    return SYNC_DRIVER_WAIT.with(std::cell::Cell::get) > 0;
    #[cfg(not(debug_assertions))]
    false
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
    fn acquire(&'static self) -> SetGuard {
        self.acquire_as(thread_key())
    }

    /// [`Self::acquire`] for a caller that has already asked for the key.
    fn acquire_as(&'static self, me: u64) -> SetGuard {
        if self.owner.load(Ordering::Acquire) == me {
            return self.reenter();
        }
        self.lock_fresh(me);
        SetGuard::of(self)
    }

    /// One more level on a thread that already holds the set. The caller
    /// has checked `owner`.
    #[inline]
    fn reenter(&'static self) -> SetGuard {
        self.depth
            .store(self.depth.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        SetGuard::of(self)
    }

    /// [`Self::lock_fresh`] if the mutex is free, without waiting for it.
    #[inline(always)]
    fn try_lock_fresh(&'static self, me: u64) -> Option<SetGuard> {
        Self::debug_check_fresh();
        let guard = self.lock.try_lock()?;
        // SAFETY: as in `lock_fresh`. `held` is `None` while the mutex is free.
        unsafe { self.held.get().write(Some(guard)) };
        self.owner.store(me, Ordering::Release);
        self.depth.store(1, Ordering::Relaxed);
        #[cfg(debug_assertions)]
        SETS_HELD.with(|n| n.set(n.get() + 1));
        Some(SetGuard::of(self))
    }

    /// The lock-order rules a fresh acquisition is held to — compiled out of
    /// a release build, shared by every way of taking the mutex.
    #[inline(always)]
    fn debug_check_fresh() {
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
    }

    /// Take the mutex on a thread that does not hold it and publish the hold.
    fn lock_fresh(&'static self, me: u64) {
        Self::debug_check_fresh();
        let guard = match self.lock.try_lock() {
            Some(guard) => guard,
            None => self.lock_taking(me),
        };
        // SAFETY: this thread holds `lock`, so it is the only one allowed at
        // `held` (see the `Sync` impl).
        // `held` is `None` whenever the mutex is free (`unlock_outermost` takes it
        // before releasing), so there is no old guard to drop.
        unsafe { self.held.get().write(Some(guard)) };
        self.owner.store(me, Ordering::Release);
        self.depth.store(1, Ordering::Relaxed);
        #[cfg(debug_assertions)]
        SETS_HELD.with(|n| n.set(n.get() + 1));
        self.admit_owed();
    }

    /// Wait for the mutex linked as a [`Taker`] of this set, for
    /// [`ReadWait::stuck`].
    #[cold]
    #[inline(never)]
    fn lock_taking(&'static self, me: u64) -> PriorityInheritanceMutexGuard<'static, ()> {
        let taker = Taker {
            key: me,
            taking: AtomicPtr::new(std::ptr::from_ref(self).cast_mut()),
            next: AtomicPtr::new(std::ptr::null_mut()),
        };
        let _linked = Linked::new(&taker);
        let guard = self.lock.lock();
        taker.taking.store(std::ptr::null_mut(), Ordering::Release);
        guard
    }

    /// Run what [`owe_cvt`] left on this set. Called wherever a hold is taken
    /// fresh — [`Self::lock_fresh`], and the callers of
    /// [`Self::try_lock_fresh`] — where no borrow of any member can be live.
    #[inline(always)]
    fn admit_owed(&'static self) {
        #[cfg(feature = "test-hooks")]
        fresh_hold_hook::fire(self);
        if self.has_owed.load(Ordering::Acquire) {
            self.drain_owed();
        }
    }

    #[cold]
    #[inline(never)]
    fn drain_owed(&'static self) {
        let cells = {
            let mut owed = self.owed.lock().unwrap_or_else(|e| e.into_inner());
            self.has_owed.store(false, Ordering::Relaxed);
            std::mem::take(&mut *owed)
        };
        for cell in cells {
            // An entry a merge moved elsewhere was forwarded by
            // `Registry::place` or re-owed by `owe_cvt`'s re-check.
            if std::ptr::eq(cell.lock_record().set(), self) {
                // SAFETY: this thread took the set fresh, so no borrow of any
                // member record is live.
                unsafe { cell.apply_cvt_pending() };
            }
        }
    }

    /// Move `lr`'s owed cells to `to` — [`Registry::place`] moving the record.
    ///
    /// Always under `owed`, never on a `has_owed` test: [`owe_cvt`] stores the
    /// flag and then loads the record's set, this stores the set and then
    /// would load the flag, and both loads may see the old value. The mutex
    /// orders the two: either this sees the entry, or `owe_cvt`'s re-check
    /// sees the new set and owes it there. Merges are cold.
    fn forward_owed(&self, lr: &LockRecord, to: Set) {
        let mut owed = self.owed.lock().unwrap_or_else(|e| e.into_inner());
        let mut moved = Vec::new();
        owed.retain(|cell| {
            let mine = std::ptr::eq(&**cell.lock_record(), lr);
            if mine {
                moved.push(Arc::clone(cell));
            }
            !mine
        });
        drop(owed);
        if !moved.is_empty() {
            let mut dest = to.owed.lock().unwrap_or_else(|e| e.into_inner());
            dest.extend(moved);
            to.has_owed.store(true, Ordering::Release);
        }
    }

    /// The owed callbacks of `of`, or all of them, in arrival order.
    fn take_callbacks(&self, of: Option<&RecordCell>) -> Vec<OwedCallback> {
        let mut callbacks = self.callbacks.lock().unwrap_or_else(|e| e.into_inner());
        let taken = match of {
            None => callbacks.drain(..).map(|(_, job)| job).collect(),
            Some(cell) => {
                let mut taken = Vec::new();
                let mut kept = std::collections::VecDeque::new();
                for (owner, job) in callbacks.drain(..) {
                    if std::ptr::eq(&*owner, cell) {
                        taken.push(job);
                    } else {
                        kept.push_back((owner, job));
                    }
                }
                *callbacks = kept;
                taken
            }
        };
        if callbacks.is_empty() {
            self.release_owed
                .fetch_and(!OWED_CALLBACKS, Ordering::Relaxed);
        }
        taken
    }

    /// The owed reads, in arrival order.
    fn take_reads(&self) -> Vec<Arc<dyn OwedRead>> {
        // A read owed after this look is the reader's own retry's.
        if self.release_owed.load(Ordering::Acquire) & OWED_READS == 0 {
            return Vec::new();
        }
        let mut reads = self.reads.lock().unwrap_or_else(|e| e.into_inner());
        self.release_owed.fetch_and(!OWED_READS, Ordering::Relaxed);
        std::mem::take(&mut *reads)
    }

    /// The outermost release: what the set was owed runs first, still under
    /// the hold, those callbacks owe in turn included; an unwind leaves it to
    /// the next holder's release.
    #[inline(always)]
    fn release_outermost(&'static self) {
        if self.release_owed.load(Ordering::Relaxed) != 0 {
            self.run_owed_then_unlock();
        } else {
            self.unlock_outermost();
        }
    }

    /// Out of line, so that a release whose set was owed nothing makes no
    /// call it did not make before. A callback that unwinds still gives the
    /// set up: the release it ran in does not run again.
    #[cold]
    #[inline(never)]
    fn run_owed_then_unlock(&'static self) {
        struct Unlock(&'static LockSet);
        impl Drop for Unlock {
            fn drop(&mut self) {
                self.0.unlock_outermost();
            }
        }
        let _unlock = Unlock(self);
        while !std::thread::panicking() {
            let jobs = self.take_callbacks(None);
            let reads = self.take_reads();
            if jobs.is_empty() && reads.is_empty() {
                break;
            }
            for job in jobs {
                job();
            }
            for read in reads {
                read.serve(None);
            }
        }
    }

    /// Release the mutex this thread holds and clear the hold. Only the
    /// owner, at depth 1, may call this.
    fn unlock_outermost(&'static self) {
        // Clear ownership BEFORE the mutex is released, or the next owner
        // could publish itself and be overwritten by this store.
        self.depth.store(0, Ordering::Relaxed);
        self.owner.store(0, Ordering::Release);
        #[cfg(debug_assertions)]
        SETS_HELD.with(|n| n.set(n.get() - 1));
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
        // The set is retaken on every exit of the release and `f`, an unwind
        // included: the frame's outer `SetGuard` releases a hold this thread
        // must own.
        let _retake = Retake { set: self, me };
        self.release_outermost();
        f()
    }

    /// C's `epicsMutexTryLock` probe in `dbLockShowLocked` (`dbLock.c:963-965`).
    fn is_locked(&self) -> bool {
        self.lock.try_lock().is_none()
    }
}

/// Takes back the set [`LockSet::unheld`] gave up, when dropped.
struct Retake {
    set: &'static LockSet,
    me: u64,
}

impl Drop for Retake {
    fn drop(&mut self) {
        self.set.lock_fresh(self.me);
    }
}

/// One acquisition of one lock set — C's `dbScanLock`/`dbScanUnlock` pair.
///
/// `!Send` on both backends, because the inner guard is: an `Option` is `Send`
/// only when its payload is, so the recursive re-entry that carries `None` is
/// `!Send` too.
pub(crate) struct SetGuard {
    set: Set,
    /// A raw-pointer marker: the set is `Sync`, which would make this guard
    /// `Send`, and a hold must be released by the thread that took it.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl SetGuard {
    #[inline(always)]
    fn of(set: Set) -> Self {
        Self {
            set,
            _not_send: std::marker::PhantomData,
        }
    }

    /// Whether this is a hold on [`bootstrap_set`] (id 0).
    #[inline(always)]
    fn on_bootstrap(&self) -> bool {
        self.set.id == 0
    }

    /// C `*dbLockSetAddrTrace(precord)`.
    #[inline]
    pub(crate) fn tracing(&self) -> bool {
        self.set.trace.load(Ordering::Relaxed) != 0
    }

    /// Whether `record` is in the set this guard holds — C `dbLockGetLockId`
    /// equality. Rule R makes the answer stable for the guard's lifetime:
    /// a record cannot leave a set while a thread holds it.
    #[inline]
    pub(crate) fn holds(&self, record: &LockRecord) -> bool {
        std::ptr::eq(record.set(), self.set)
    }
}

impl Drop for SetGuard {
    #[inline]
    fn drop(&mut self) {
        // `depth` counts this thread's live holds, so the one that takes it
        // to zero is the one that gives the mutex up — whichever order the
        // guards are dropped in. No record of the set is borrowed or lent by
        // then.
        let depth = self.set.depth.load(Ordering::Relaxed) - 1;
        if depth != 0 {
            self.set.depth.store(depth, Ordering::Relaxed);
        } else {
            self.set.release_outermost();
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
    sets.into_iter().map(|set| set.acquire()).collect()
}

/// C's `lockSetsActive` / `lockSetsFree` / `next_id` (`dbLock.c:42-70`) plus
/// the record-to-set mapping C keeps in `dbCommon::lset`.
struct Registry {
    /// C `lockSetsActive`, keyed by set id. The list ORDER `dblsr` and
    /// `dbLockShowLocked` walk is [`SetState::listed`], not the id: a set
    /// handed back off the free list keeps its low id but goes on the end.
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
    of_record: HashMap<RecordName, Arc<LockRecord>>,
    next_id: u64,
    /// The last [`SetState::listed`] handed out.
    listed: u64,
    /// When each DB link `(record, FIELD)` was added, ascending: the order
    /// its target's `bklnk` holds it in, since `dbDbInitLink` and
    /// `dbDbAddLink` append (`dbDbLink.c:106`, `:121`). It only orders
    /// [`Registry::split_part`]'s walk; which records a set holds is read
    /// from the link text.
    link_added: HashMap<(RecordName, String), u64>,
    links_added: u64,
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
    /// In list order, which `dblsr` prints: a merge appends the moved set's
    /// list to the survivor's (`dbLock.c:619-623`).
    members: Vec<RecordName>,
    /// Where `makeSet`'s `ellAdd` put the set on `lockSetsActive`
    /// (`dbLock.c:95`): ascending in the order sets went active.
    listed: u64,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            active: BTreeMap::new(),
            free: VecDeque::new(),
            of_record: HashMap::new(),
            next_id: FIRST_SET_ID - 1,
            listed: 0,
            link_added: HashMap::new(),
            links_added: 0,
            revision: 0,
            built: false,
        }
    }
}

impl Registry {
    /// C `makeSet` (`dbLock.c:72-101`): reuse a freed set, keeping its id and
    /// its mutex, or mint one, and put it on the end of the active list with
    /// `members`.
    fn make_set(&mut self, members: Vec<RecordName>) -> Set {
        let set = match self.free.pop_front() {
            Some(set) => {
                debug_assert_eq!(
                    set.many_holds.load(Ordering::Relaxed),
                    0,
                    "C asserts refcount==0 for a set on the free list (dbLock.c:571)"
                );
                set
            }
            None => {
                self.next_id += 1;
                self.revision += 1;
                new_set(self.next_id)
            }
        };
        self.listed += 1;
        self.active.insert(
            set.id,
            SetState {
                set,
                members,
                listed: self.listed,
            },
        );
        set
    }

    /// The cell `name` locks through, or `None` when the name has none —
    /// including a registered record that is still on the bootstrap set,
    /// which is C's null `lset` and what `dblsr` reports as no set at all
    /// (`dbLock.c:900-901`).
    fn real_set_of(&self, name: &[u8]) -> Option<Set> {
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
    fn set_of_registered(&self, name: &[u8]) -> Option<Set> {
        let set = self.real_set_of(name);
        debug_assert!(
            set.is_some() || !self.of_record.contains_key(name),
            "registered record {} is on the bootstrap set after iocInit",
            String::from_utf8_lossy(name)
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
    fn lock_record_of(&mut self, name: &[u8]) -> Arc<LockRecord> {
        if let Some(lr) = self.of_record.get(name) {
            return lr.clone();
        }
        let set = self.make_set(vec![RecordName::from(name)]);
        let lr = LockRecord::new(set);
        self.of_record.insert(RecordName::from(name), lr.clone());
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
    fn adopt(&mut self, name: &[u8], lr: &Arc<LockRecord>) {
        match self.of_record.get(name) {
            Some(existing) if Arc::ptr_eq(existing, lr) => {}
            Some(existing) => {
                lr.store(existing.set());
                self.of_record.insert(RecordName::from(name), lr.clone());
                self.revision += 1;
            }
            None => {
                self.of_record.insert(RecordName::from(name), lr.clone());
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
    fn mint_for(&mut self, name: &[u8], lr: &Arc<LockRecord>) {
        if !lr.is_bootstrap() {
            return;
        }
        let set = self.make_set(vec![RecordName::from(name)]);
        lr.store(set);
        self.of_record
            .entry(RecordName::from(name))
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
    fn place(&mut self, names: impl IntoIterator<Item = RecordName>, set: Set) {
        self.revision += 1;
        for name in names {
            match self.of_record.get(&name) {
                Some(lr) => {
                    let old = lr.set();
                    lr.store(set);
                    if !std::ptr::eq(old, set) {
                        old.forward_owed(lr, set);
                    }
                }
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
    fn merge(&mut self, first: &[u8], second: &[u8]) {
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
    fn repartition(&mut self, seed: &[u8], adjacency: &Adjacency, held: &[Set]) {
        let Some(seed_set) = self.of_record.get(seed).map(|lr| lr.set()) else {
            return;
        };
        let seed_id = seed_set.id;

        let mut affected: BTreeSet<RecordName> = BTreeSet::new();
        let mut work: Vec<RecordName> = vec![RecordName::from(seed)];
        while let Some(name) = work.pop() {
            if !affected.insert(name.clone()) {
                continue;
            }
            if let Some(links) = adjacency.get(&name) {
                work.extend(links.neighbours().cloned());
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
                self.link_added.retain(|(from, _), _| from != name);
            }
        }

        // The components of the region, the seed's first when the seed is
        // still a record. Each is closed inside `affected` by construction:
        // `affected` was built by following the same adjacency.
        let seed_present = adjacency.contains_key(seed);
        let mut components: Vec<BTreeSet<RecordName>> = Vec::new();
        let mut placed: BTreeSet<RecordName> = BTreeSet::new();
        let starts = seed_present
            .then(|| RecordName::from(seed))
            .into_iter()
            .chain(affected.iter().cloned());
        for start in starts {
            if placed.contains(&start) || !adjacency.contains_key(&start) {
                continue;
            }
            let mut component: BTreeSet<RecordName> = BTreeSet::new();
            let mut walk = vec![start];
            while let Some(name) = walk.pop() {
                if !component.insert(name.clone()) {
                    continue;
                }
                if let Some(links) = adjacency.get(&name) {
                    walk.extend(links.neighbours().cloned());
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

        // Each component's list, read before any set is rewritten: the kept
        // set's list first, then the other touched sets' in id order — the
        // order `dbLockSetMerge`'s append leaves when one link joins two sets.
        // Every member is on one of those lists, since relinks run only after
        // `built` and a registered record is then never on the bootstrap set.
        let ordered: Vec<Vec<RecordName>> = components
            .iter()
            .zip(&assigned)
            .map(|(component, id)| {
                let members: Vec<RecordName> = id
                    .iter()
                    .chain(touched.iter().filter(|other| Some(**other) != *id))
                    .flat_map(|id| &self.active[id].members)
                    .filter(|name| component.contains(*name))
                    .cloned()
                    .collect();
                debug_assert_eq!(members.len(), component.len());
                members
            })
            .collect();

        // Mint before freeing, as C does: `dbLockSetSplit` calls `makeSet`
        // while the set it is splitting is still live, so the new set comes
        // off whatever was already on the free list rather than off the id
        // this very edit is about to release.
        let fresh: Vec<Set> = assigned
            .iter()
            .filter(|id| id.is_none())
            .map(|_| self.make_set(Vec::new()))
            .collect();
        let mut fresh = fresh.into_iter();

        for (members, id) in ordered.into_iter().zip(assigned) {
            let set = match id {
                Some(id) => self.active[&id].set,
                None => fresh
                    .next()
                    .expect("one fresh set per unassigned component"),
            };
            self.place(members.iter().cloned(), set);
            self.active
                .get_mut(&set.id)
                .expect("a kept id and a fresh set are both active")
                .members = members;
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

    /// Note that `record`'s DB link in `field` was added now — the end of its
    /// target's `bklnk`.
    fn link_added(&mut self, record: &[u8], field: &str) {
        self.links_added += 1;
        self.link_added.insert(
            (RecordName::from(record), field.to_ascii_uppercase()),
            self.links_added,
        );
    }

    /// The records `links` reaches in `dbLockSetSplit`'s order: the record's
    /// own links in `link_ind` order, then its `bklnk` in the order the
    /// links were added (`dbLock.c:733-783`). A lock partner, which C does
    /// not have, and a link on a record registered after `iocInit` carry no
    /// add order and come after the ones that do.
    fn walk_order<'a>(&self, links: &'a DbLinks) -> impl Iterator<Item = &'a RecordName> {
        let mut back: Vec<&(RecordName, Option<String>)> = links.back.iter().collect();
        back.sort_by_key(|(from, field)| {
            field
                .as_ref()
                .and_then(|field| self.link_added.get(&(from.clone(), field.clone())))
                .copied()
                .unwrap_or(u64::MAX)
        });
        links
            .out
            .iter()
            .chain(back.into_iter().map(|(from, _)| from))
    }

    /// The records C `dbLockSetSplit(first, second)` moves to a new set, in
    /// the order it lists them: breadth-first from `second`
    /// (`dbLock.c:717-790`). `None` when the walk reaches `first` — its
    /// `goto nosplit` — or when the two are not in one set, which C refuses
    /// with `cantProceed`.
    fn split_part(
        &self,
        first: &[u8],
        second: &[u8],
        adjacency: &Adjacency,
    ) -> Option<Vec<RecordName>> {
        let same_set = self
            .real_set_of(first)
            .zip(self.real_set_of(second))
            .is_some_and(|(a, b)| std::ptr::eq(a, b));
        debug_assert!(
            same_set || self.real_set_of(second).is_none(),
            "dbLockSetSplit consistency violation: {} and {} are in different sets",
            String::from_utf8_lossy(first),
            String::from_utf8_lossy(second)
        );
        if first == second || !same_set {
            return None;
        }
        let mut part = vec![RecordName::from(second)];
        let mut seen: HashSet<RecordName> = part.iter().cloned().collect();
        let mut next = 0;
        while let Some(name) = part.get(next).cloned() {
            next += 1;
            let Some(links) = adjacency.get(&name) else {
                continue;
            };
            for other in self.walk_order(links) {
                if other.as_ref() == first {
                    return None;
                }
                if seen.insert(other.clone()) {
                    part.push(other.clone());
                }
            }
        }
        Some(part)
    }

    /// C `dbPutFieldLink`'s relink of `record.field` (`dbAccess.c:1170`,
    /// `:1208`): `dbDbRemoveLink` splits `record`'s set from the old target,
    /// then `dbDbAddLink` merges it with the new one. `adjacency` is the
    /// graph between the two calls — without the field's link.
    ///
    /// When the new target is on the side the split took away, the merge
    /// moves that side straight back, so its records are left in place and
    /// only the two steps' list and free-list effects are applied: the split
    /// set is not held, and [`Self::place`] may move a record only out of a
    /// held set.
    fn relink_field(
        &mut self,
        record: &[u8],
        field: &str,
        old: Option<&[u8]>,
        new: Option<&[u8]>,
        adjacency: &Adjacency,
    ) {
        self.link_added
            .remove(&(RecordName::from(record), field.to_ascii_uppercase()));
        let part = old.and_then(|old| self.split_part(record, old, adjacency));
        if new.is_some() {
            self.link_added(record, field);
        }
        let Some(part) = part else {
            if let Some(new) = new {
                self.merge(record, new);
            }
            return;
        };
        let kept = self
            .real_set_of(record)
            .expect("split_part found record and old target in one set")
            .id;
        debug_assert!(
            part.iter()
                .all(|name| self.real_set_of(name).is_some_and(|set| set.id == kept)),
            "the split walk left the set it splits"
        );
        let split = self.make_set(Vec::new());
        let left: HashSet<&RecordName> = part.iter().collect();
        self.active
            .get_mut(&kept)
            .expect("every id in of_record names an active set")
            .members
            .retain(|name| !left.contains(name));
        if new.is_some_and(|new| left.iter().any(|name| name.as_ref() == new)) {
            self.active
                .get_mut(&kept)
                .expect("every id in of_record names an active set")
                .members
                .extend(part);
            let freed = self
                .active
                .remove(&split.id)
                .expect("make_set put the split set on the active list");
            self.free.push_back(freed.set);
            self.revision += 1;
            return;
        }
        self.place(part.iter().cloned(), split);
        self.active
            .get_mut(&split.id)
            .expect("make_set put the split set on the active list")
            .members = part;
        if let Some(new) = new {
            self.merge(record, new);
        }
    }

    /// Every set a registered record can be behind after `iocInit`: the
    /// active ones. Not the bootstrap set — [`Self::adopt`] mints a set for
    /// every record registered after [`PvDatabase::build_lock_sets`], and
    /// [`PvDatabase::init_sets`] for every one registered before it, so no
    /// record a relink can reach is on id 0.
    fn every_set(&self) -> Vec<Set> {
        self.active.values().map(|state| state.set).collect()
    }

    fn info(&self, id: u64, rows: &HashMap<usize, MutexInfo>) -> LockSetInfo {
        let state = &self.active[&id];
        LockSetInfo {
            id,
            members: state.members.clone(),
            refs: state.members.len() + state.set.many_holds.load(Ordering::Relaxed) as usize,
            locked: state.set.is_locked(),
            mutex: rows.get(&state.set.mutex_addr).cloned(),
        }
    }
}

/// Every row of the process mutex list, keyed by the address it reports —
/// which is [`LockSet::mutex_addr`] for a lock set's own mutex.
fn mutex_rows_by_addr() -> HashMap<usize, MutexInfo> {
    mutex_report(false)
        .shown
        .into_iter()
        .map(|info| (info.addr(), info))
        .collect()
}

/// One active lock set, as `dblsr` and `dbLockShowLocked` report it.
pub struct LockSetInfo {
    /// C's `lockSet::id`.
    pub id: u64,
    /// C's `lockRecordList`, in the order `dblsr` walks it.
    pub members: Vec<RecordName>,
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
    /// [`PvDatabase::set_lock_partners`], by canonical record name. A leaf
    /// lock: nothing is taken under it.
    partners: std::sync::Mutex<HashMap<RecordName, Vec<RecordName>>>,
}

impl RecordLockRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Which set `record` is behind right now, without creating one.
    fn set_id_of(&self, record: &[u8]) -> Option<u64> {
        self.lock().real_set_of(record).map(|set| set.id)
    }

    /// See [`Registry::adopt`].
    pub(crate) fn adopt(&self, name: &[u8], lr: &Arc<LockRecord>) {
        self.lock().adopt(name, lr);
    }

    fn partners_of(&self, record: &[u8]) -> Vec<RecordName> {
        self.partners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(record)
            .cloned()
            .unwrap_or_default()
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
        // C's `forEachRecord` order, which is both the order sets are minted
        // in (their ids) and the order links merge in (their member lists).
        let names: Vec<RecordName> = self
            .iterate_records()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        // Every edge is collected BEFORE the registry lock is taken:
        // `record_link_fields` reads the record map and each record's own
        // lock, and neither may be acquired underneath the registry.
        let mut edges: Vec<(RecordName, Option<String>, RecordName)> = Vec::new();
        for name in &names {
            for (field, target) in self.db_links(name) {
                edges.push((name.clone(), field, target));
            }
        }
        self.init_sets(&names);
        {
            let mut registry = self.inner.record_locks.lock();
            for (from, field, _) in &edges {
                if let Some(field) = field {
                    registry.link_added(from, field);
                }
            }
        }
        for (from, _, to) in edges {
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
    fn init_sets(&self, names: &[RecordName]) {
        let _out_of = bootstrap_set().acquire();
        let mut registry = self.inner.record_locks.lock();
        for name in names {
            let lr = registry.lock_record_of(name);
            registry.mint_for(name, &lr);
        }
        let mut adopted: Vec<(RecordName, Arc<LockRecord>)> = registry
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
    fn merge_sets(&self, first: &[u8], second: &[u8]) {
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
    fn ensure_set(&self, name: &[u8]) -> Arc<LockRecord> {
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
    fn ensure_set_for(&self, name: &[u8], lr: &Arc<LockRecord>) {
        if !lr.is_bootstrap() {
            return;
        }
        let _out_of = bootstrap_set().acquire();
        self.inner.record_locks.lock().mint_for(name, lr);
    }

    /// The records `record`'s DB links reach, canonicalised, and the
    /// partners its device declared ([`Self::set_lock_partners`]).
    ///
    /// Only a link that resolved to a LOCAL record merges: C reaches
    /// `dbLockSetMerge` from `dbDbInitLink`, and a target this IOC does not
    /// have falls through to `dbCaAddLink` instead (`dbLink.c:118-130`), which
    /// merges nothing. `record_link_fields` has already applied that same
    /// locality rule, so a CA link to a local record is a `Ca` link here
    /// and does not widen the set.
    fn db_link_targets(&self, record: &[u8]) -> Vec<RecordName> {
        self.db_links(record)
            .into_iter()
            .map(|(_, target)| target)
            .collect()
    }

    /// The record `record.field`'s DB link reaches, as
    /// [`Self::db_link_targets`] reports it.
    fn db_link_of(&self, record: &[u8], field: &str) -> Option<RecordName> {
        self.db_links(record)
            .into_iter()
            .find(|(name, _)| {
                name.as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(field))
            })
            .map(|(_, target)| target)
    }

    /// [`Self::db_link_targets`] with the field each DB link is in, in
    /// `link_ind` order; a lock partner has no field.
    fn db_links(&self, record: &[u8]) -> Vec<(Option<String>, RecordName)> {
        use crate::server::record::ParsedLink;
        self.record_link_fields(record)
            .into_iter()
            .filter_map(|(field, _, parsed)| match parsed {
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
                    self.get_record_no_resolve(&name)
                        .map(|_| (Some(field), name))
                }
                _ => None,
            })
            .chain(
                self.inner
                    .record_locks
                    .partners_of(record)
                    .into_iter()
                    .filter(|name| self.get_record_no_resolve(name).is_some())
                    .map(|name| (None, name)),
            )
            .collect()
    }

    /// Put `record` behind one lock set with each of `partners`, as a DB
    /// link to it would, in place of the partners declared before.
    ///
    /// C has no such call. A StreamDevice protocol's `%(rec.FIELD)` formats
    /// put to and process other records from the bus callback, which in C
    /// holds no record; a device here holds its own record there, and a
    /// second set taken under it is an ABBA with a record redirecting back.
    ///
    /// Before [`Self::build_lock_sets`] this only records the edges, which
    /// the build merges. After it the seed's component is re-derived from
    /// the link graph, a partner dropped since the last call split off
    /// unless something else still joins it. That relink takes the sets in
    /// id order, so the caller holds none: the device declares its partners
    /// between the holds of an [`InitHold`](super::InitHold).
    pub(crate) fn set_lock_partners(
        &self,
        record: impl AsRef<[u8]>,
        partners: impl IntoIterator<Item = RecordName>,
    ) {
        let record = record.as_ref();
        let canonical = self
            .resolve_alias(record)
            .unwrap_or_else(|| RecordName::from(record));
        let partners: Vec<RecordName> = partners
            .into_iter()
            .map(|name| self.resolve_alias(&name).unwrap_or(name))
            .filter(|name| *name != canonical)
            .collect();
        let edit = self.inner.record_locks.lock().built.then(|| LockSetEdit {
            db: self,
            record: canonical.clone(),
            scope: RelinkScope::Partners,
        });
        #[cfg(debug_assertions)]
        assert!(
            edit.is_none() || SETS_HELD.with(std::cell::Cell::get) == 0,
            "lock partners declared under a hold"
        );
        self.inner
            .record_locks
            .partners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(canonical, partners);
        drop(edit);
    }

    /// C's `lockSetsActive` and `lockSetsFree` as `dblsr("*", n)` and
    /// `dbLockShowLocked(n)` read them.
    pub fn lock_set_report(&self) -> LockSetReport {
        let rows = mutex_rows_by_addr();
        let registry = self.inner.record_locks.lock();
        let mut listed: Vec<(u64, u64)> = registry
            .active
            .values()
            .map(|state| (state.listed, state.set.id))
            .collect();
        listed.sort_unstable();
        LockSetReport {
            active: listed
                .into_iter()
                .map(|(_, id)| registry.info(id, &rows))
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
    pub fn lock_set_of(&self, record: impl AsRef<[u8]>) -> Option<LockSetInfo> {
        let record = record.as_ref();
        let canonical = self
            .resolve_alias(record)
            .unwrap_or_else(|| RecordName::from(record));
        let rows = mutex_rows_by_addr();
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
        record: &[u8],
        field: &str,
    ) -> Option<LockSetEdit<'a>> {
        let canonical = self
            .resolve_alias(record)
            .unwrap_or_else(|| RecordName::from(record));
        if !self.is_dbf_link_field(&canonical, field) {
            return None;
        }
        if !self.inner.record_locks.lock().built {
            return None;
        }
        let old_target = self.db_link_of(&canonical, field);
        Some(LockSetEdit {
            db: self,
            record: canonical,
            scope: RelinkScope::LinkEdit {
                field: field.to_string(),
                old_target,
            },
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
        record: &[u8],
    ) -> Option<LockSetEdit<'a>> {
        if !self.inner.record_locks.lock().built {
            return None;
        }
        Some(LockSetEdit {
            db: self,
            record: self
                .resolve_alias(record)
                .unwrap_or_else(|| RecordName::from(record)),
            scope: RelinkScope::Membership,
        })
    }

    /// **The single owner of the lock-set partition after `iocInit`.**
    ///
    /// A link put replays C's split and merge ([`Registry::relink_field`]);
    /// any other change re-derives the connected component `record` now sits
    /// in and re-partitions every set that component touches
    /// ([`Registry::repartition`]). See the module doc.
    ///
    /// Private, and reachable only by dropping a [`LockSetEdit`].
    ///
    /// The sets are the records' data locks, so the region is taken before
    /// it is read and held while it is re-partitioned — C's `dbPutFieldLink`
    /// holds both `dbLockSetMerge` operands through `dbScanLockMany`
    /// (`dbAccess.c:1115-1123`, `dbLock.c:587-607`). Which sets make up the
    /// region is the scope's business:
    ///
    /// * [`RelinkScope::LinkEdit`] and [`RelinkScope::Partners`] — the
    ///   seed's set and the sets of the records its links now reach. A link
    ///   put already holds them ([`PvDatabase::acquire_put_gate`] takes them
    ///   as `dbPutFieldLink` does), so its acquisition here is a re-entry; a
    ///   partner declaration ([`Self::set_lock_partners`]) holds none, and
    ///   takes them here in id order. The
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
    fn relink_lock_sets(&self, record: &[u8], scope: &RelinkScope) {
        let mut region: Vec<Set> = Vec::new();
        loop {
            let revision = {
                let registry = self.inner.record_locks.lock();
                match scope {
                    RelinkScope::Membership => region = registry.every_set(),
                    RelinkScope::LinkEdit { .. } | RelinkScope::Partners => {
                        // On every pass: with no set held, as a partner
                        // declaration has, the seed moves until it is taken.
                        if let Some(set) = registry.real_set_of(record)
                            && !region.iter().any(|held| std::ptr::eq(*held, set))
                        {
                            region.push(set);
                        }
                    }
                }
                registry.revision
            };
            if *scope != RelinkScope::Membership {
                // The targets are read under the seed's set when a put holds
                // it, and the region's links are read again below under the
                // region either way; their sets come from the registry, not
                // from a lock.
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
                RelinkScope::LinkEdit { .. } | RelinkScope::Partners => {
                    let members: Vec<RecordName> = {
                        let registry = self.inner.record_locks.lock();
                        region
                            .iter()
                            .filter_map(|set| registry.active.get(&set.id))
                            .flat_map(|state| state.members.iter().cloned())
                            .collect()
                    };
                    let without = match scope {
                        RelinkScope::LinkEdit { field, .. } => Some((record, field.as_str())),
                        _ => None,
                    };
                    self.db_link_adjacency_of(&members, without)
                }
            };
            let new_target = match scope {
                RelinkScope::LinkEdit { field, .. } => self.db_link_of(record, field),
                _ => None,
            };
            let mut registry = self.inner.record_locks.lock();
            if registry.revision != revision {
                drop(registry);
                drop(held);
                continue;
            }
            let outside: Vec<Set> = adjacency
                .values()
                .flat_map(DbLinks::neighbours)
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
            match scope {
                RelinkScope::LinkEdit { field, old_target } => registry.relink_field(
                    record,
                    field,
                    old_target.as_deref(),
                    new_target.as_deref(),
                    &adjacency,
                ),
                RelinkScope::Partners | RelinkScope::Membership => {
                    registry.repartition(record, &adjacency, &region)
                }
            }
            return;
        }
    }

    /// The DB-link graph in both directions.
    ///
    /// Both because C's merge is undirected: `dbLockSetMerge(locker,
    /// plink->precord, target)` puts both endpoints behind one mutex
    /// regardless of which way the link points, and `dbLockSetSplit` walks
    /// `bklnk` as well as the record's own links (`dbLock.c:733-783`) for the
    /// same reason. A record that is only ever pointed AT is as much a member
    /// as the one pointing.
    fn db_link_adjacency(&self) -> Adjacency {
        let names: Vec<RecordName> = self.inner.records.read().keys().cloned().collect();
        self.db_link_adjacency_of(&names, None)
    }

    /// [`Self::db_link_adjacency`] over `names` only — the members of the
    /// sets a [`RelinkScope::LinkEdit`] relink holds — and without the
    /// `(record, field)` link `without` names. A target outside `names` still
    /// appears as a neighbour, which is how the relink notices a set it does
    /// not hold.
    fn db_link_adjacency_of(
        &self,
        names: &[RecordName],
        without: Option<(&[u8], &str)>,
    ) -> Adjacency {
        let mut adjacency: Adjacency = names
            .iter()
            .map(|name| (name.clone(), DbLinks::default()))
            .collect();
        for name in names {
            for (field, target) in self.db_links(name) {
                if without.is_some_and(|(record, skip)| {
                    name.as_ref() == record
                        && field
                            .as_deref()
                            .is_some_and(|field| field.eq_ignore_ascii_case(skip))
                }) {
                    continue;
                }
                adjacency
                    .entry(name.clone())
                    .or_default()
                    .out
                    .push(target.clone());
                adjacency
                    .entry(target)
                    .or_default()
                    .back
                    .push((name.clone(), field));
            }
        }
        adjacency
    }
}

/// One record's DB links and lock partners: its own, in `link_ind` order,
/// and those naming it — its `bklnk`, which [`Registry::walk_order`] puts in
/// the order they were added. A partner's entry has no field.
#[derive(Default)]
struct DbLinks {
    out: Vec<RecordName>,
    back: Vec<(RecordName, Option<String>)>,
}

impl DbLinks {
    fn neighbours(&self) -> impl Iterator<Item = &RecordName> {
        self.out
            .iter()
            .chain(self.back.iter().map(|(from, _)| from))
    }
}

type Adjacency = HashMap<RecordName, DbLinks>;

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
    record: RecordName,
    scope: RelinkScope,
}

impl Drop for LockSetEdit<'_> {
    fn drop(&mut self) {
        self.db.relink_lock_sets(&self.record, &self.scope);
    }
}

/// Which sets a relink must hold, and which transition it makes — see
/// [`PvDatabase::relink_lock_sets`].
#[derive(Clone, PartialEq, Eq, Debug)]
enum RelinkScope {
    /// `field`, a link field of the seed, was written; the put holds the
    /// seed's set and the new target's. `old_target` is the record the
    /// field's DB link reached before the write — `dbDbRemoveLink`'s.
    LinkEdit {
        field: String,
        old_target: Option<RecordName>,
    },
    /// The seed's lock partners were declared, with no set held.
    Partners,
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

impl RecordWriteGuard {
    /// The set this gate holds, for the frame that writes under it.
    #[inline(always)]
    pub(crate) fn set(&self) -> &SetGuard {
        &self._guard
    }
}

/// The set a process frame runs under: the one it took at its entry, or the
/// one its caller's frame already holds.
///
/// The second case counts nothing — no level on `depth`, no release — so the
/// set still reads as held exactly once to `unheld` (a breakpoint parks only
/// a thread that holds its set once). It is the caller's hold, borrowed for a
/// call that returns before the caller's guard drops.
pub(crate) enum FrameGate {
    Taken(RecordWriteGuard),
    Held(std::mem::ManuallyDrop<SetGuard>),
}

impl FrameGate {
    /// The gate of a recursive FLNK/OUT/CP frame or a transaction owner's:
    /// the set this thread holds already. A thread that does not hold it —
    /// which no caller is meant to be — takes it, as `write()` always did.
    #[inline(always)]
    pub(crate) fn of_held(lr: &LockRecord) -> Self {
        let set = lr.set();
        if set.owner.load(Ordering::Acquire) == thread_key() {
            return Self::Held(std::mem::ManuallyDrop::new(SetGuard::of(set)));
        }
        Self::Taken(RecordWriteGuard {
            _guard: lr.acquire(),
        })
    }

    /// [`Self::of_held`] for a thread that may hold no set: a device's
    /// callback thread, or its own record's hold that reaches another record.
    /// An unheld set is taken as [`PvDatabase::lock_instance`] takes it.
    pub(crate) fn of_instance(db: &PvDatabase, rec: &Arc<RecordCell>) -> Self {
        let set = rec.lock_record().set();
        if set.owner.load(Ordering::Acquire) == thread_key() {
            return Self::Held(std::mem::ManuallyDrop::new(SetGuard::of(set)));
        }
        Self::Taken(db.lock_instance(rec))
    }

    /// The set the frame writes under.
    #[inline(always)]
    pub(crate) fn set(&self) -> &SetGuard {
        match self {
            Self::Taken(gate) => gate.set(),
            Self::Held(set) => set,
        }
    }
}

/// A process frame's own record, reached through the set the frame holds.
///
/// **Invariant:** inside a process frame the record is read and written only
/// through this handle, whose `read`/`write` borrow under the frame's
/// [`SetGuard`] and never take the set, the thread key or the depth again.
/// `RecordCell::read`/`write` are for code that holds no frame; a frame
/// function takes a `InFrame` and has no `Arc<RecordCell>` to call them on.
/// The cell leaves only through [`Self::unframed`], which names the exit.
///
/// **Owner/gate:** [`FrameGate::frame`] — the one constructor, so a
/// `InFrame` exists only beside the gate that proves the set is held.
#[derive(Clone, Copy)]
pub(crate) struct InFrame<'a> {
    cell: &'a Arc<RecordCell>,
    set: &'a SetGuard,
}

impl<'a> InFrame<'a> {
    /// Shared access under the frame's set.
    #[inline(always)]
    pub(crate) fn read(&self) -> RecordRef<'a> {
        self.cell.read_held(self.set)
    }

    /// Exclusive access under the frame's set.
    #[inline(always)]
    pub(crate) fn write(&self) -> HeldMut<'a> {
        self.cell.write_in(self.set)
    }

    /// Name what this record's next cycle reads, for the scan walk.
    #[inline(always)]
    pub(crate) fn publish_hints(&self) {
        self.read().publish_hints(self.cell.lock_record().hints());
    }

    /// The set the frame holds, which a link target merged into it is read
    /// under without taking it again.
    #[inline(always)]
    /// C `dbProcess`'s trace prologue (`dbAccess.c:519-526`): a TPRO record
    /// turns its set's trace on, unless it is on already, until its
    /// [`Self::trace_exit`]. Whether this pass prints.
    pub(crate) fn trace_enter(&self, tpro: bool) -> bool {
        let trace = &self.set.set.trace;
        if tpro && trace.load(Ordering::Relaxed) == 0 {
            trace.store(Arc::as_ptr(self.cell) as usize, Ordering::Relaxed);
        }
        trace.load(Ordering::Relaxed) != 0
    }

    /// C `dbProcess`'s `if (set_trace) *ptrace = 0;` (`dbAccess.c:620-621`).
    pub(crate) fn trace_exit(&self) {
        let trace = &self.set.set.trace;
        if trace.load(Ordering::Relaxed) == Arc::as_ptr(self.cell) as usize {
            trace.store(0, Ordering::Relaxed);
        }
    }

    pub(crate) fn set(&self) -> &'a SetGuard {
        self.set
    }

    /// The record's process plan — fixed at construction, so no lock.
    #[inline(always)]
    pub(crate) fn process_plan(&self) -> &'a crate::server::record::record_instance::ProcessPlan {
        self.cell.process_plan()
    }

    /// The record type's declaration — fixed at construction, so no lock.
    #[inline(always)]
    pub(crate) fn rdes(&self) -> &'a crate::server::record::record_instance::RecordDesc {
        self.cell.rdes()
    }

    /// The cell, for work that leaves the frame — a spawn, a name for
    /// another set. Reading or writing through it is a bypass of the gate.
    #[inline(always)]
    pub(crate) fn unframed(&self) -> &'a Arc<RecordCell> {
        self.cell
    }

    /// Whether a member of the frame's set is owed a driver callback.
    #[inline(always)]
    pub(crate) fn callbacks_owed(&self) -> bool {
        self.set.set.release_owed.load(Ordering::Relaxed) & OWED_CALLBACKS != 0
    }

    /// The driver callbacks this record was owed inside its dset call
    /// ([`owe_callback`]), run in arrival order now the call has returned.
    /// The record's borrow must be down and its device back.
    #[cold]
    #[inline(never)]
    pub(crate) fn run_owed_callbacks(&self) {
        for job in self.set.set.take_callbacks(Some(self.cell)) {
            job();
        }
    }
}

impl FrameGate {
    /// The handle a frame function takes for `rec`, which this gate holds.
    #[inline(always)]
    pub(crate) fn frame<'a>(&'a self, rec: &'a Arc<RecordCell>) -> InFrame<'a> {
        debug_assert!(self.set().holds(rec.lock_record()));
        InFrame {
            cell: rec,
            set: self.set(),
        }
    }
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
    guards: Vec<SetGuard>,
}

impl ManyRecordWriteGuard {
    /// Count `guards` as the locked list C's `dbScanLockMany` adds a reference
    /// for (`dbLock.c:404`); [`Drop`] gives the references back before the
    /// sets are released.
    fn new(guards: Vec<SetGuard>) -> Self {
        for guard in &guards {
            guard.set.many_holds.fetch_add(1, Ordering::Relaxed);
        }
        Self { guards }
    }
}

impl Drop for ManyRecordWriteGuard {
    fn drop(&mut self) {
        for guard in &self.guards {
            guard.set.many_holds.fetch_sub(1, Ordering::Relaxed);
        }
    }
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
    pub fn lock_record(&self, record: impl AsRef<[u8]>) -> RecordWriteGuard {
        let record = record.as_ref();
        let resolved = self.resolve_alias(record);
        let canonical: &[u8] = resolved.as_deref().unwrap_or(record);
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
        let guard = lr.acquire();
        if guard.on_bootstrap() {
            return self.lock_instance_off_bootstrap(rec, lr, guard);
        }
        RecordWriteGuard { _guard: guard }
    }

    /// First gate this record has ever been given. C mints the set in
    /// `dbLockInitRecords` before anything can lock; the port allows a
    /// database with no `iocInit` at all, so the set is minted on demand
    /// here — once per record, off every later pass. The name is read and
    /// the bootstrap hold dropped BEFORE the move: a guard taken through the
    /// bootstrap set must not outlive the record's stay in it.
    #[cold]
    #[inline(never)]
    fn lock_instance_off_bootstrap(
        &self,
        rec: &Arc<RecordCell>,
        lr: &Arc<LockRecord>,
        bootstrap: SetGuard,
    ) -> RecordWriteGuard {
        let name = rec.read().name.clone();
        drop(bootstrap);
        self.ensure_set_for(&name, lr);
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
        S: AsRef<[u8]>,
    {
        let names: Vec<RecordName> = records
            .into_iter()
            .map(|record| {
                let record = record.as_ref();
                self.resolve_alias(record)
                    .unwrap_or_else(|| RecordName::from(record))
            })
            .collect();
        let cells: Vec<Arc<LockRecord>> = names.iter().map(|name| self.ensure_set(name)).collect();

        loop {
            let taken: Vec<Set> = cells.iter().map(|lr| lr.set()).collect();
            let mut sets = taken.clone();
            sets.sort_unstable_by_key(|set| set.id);
            sets.dedup_by_key(|set| set.id);

            let guards: Vec<SetGuard> = sets.iter().map(|set| set.acquire()).collect();

            // `dbLockUpdateRefs(locker, 0)` (`dbLock.c:432-436`): if a merge
            // moved any member while the sets were being taken, release
            // everything and start again. Each member against the set taken
            // for it, as C compares each ref: a member that moved into
            // another taken set left its own emptied, on the free list, and
            // no reference may be counted on that (`dbLock.c:571`).
            if names
                .iter()
                .zip(&taken)
                .all(|(name, set)| self.inner.record_locks.set_id_of(name) == Some(set.id))
            {
                return ManyRecordWriteGuard::new(guards);
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
            std::ptr::eq(db.ensure_set(b"REC:A").set(), db.ensure_set(b"REC:A").set()),
            "the same canonical name must map to the same lock set"
        );
        assert!(
            !std::ptr::eq(db.ensure_set(b"REC:A").set(), db.ensure_set(b"REC:B").set()),
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

    /// A set finds its `epicsMutexShow` row by its own mutex's address, so
    /// every set gets a row of its own and a mutex created anywhere else —
    /// including in this file — is never mistaken for a set's.
    #[test]
    fn every_set_reports_the_row_of_its_own_mutex() {
        let db = PvDatabase::new();
        for name in ["MS:1", "MS:2", "MS:3"] {
            drop(db.lock_record(name));
        }
        let decoy: PriorityInheritanceMutex<()> =
            PriorityInheritanceMutex::new((), CMutex::LockSet);

        let mut claimed = std::collections::HashSet::new();
        for set in db.lock_set_report().active {
            let row = set
                .mutex
                .unwrap_or_else(|| panic!("set {} has no row", set.id));
            assert_eq!(row.file(), file!(), "a set's mutex is created here");
            assert_ne!(
                row.addr(),
                decoy.registered_addr(),
                "set {} claimed a mutex that is not a lock set's",
                set.id
            );
            assert!(claimed.insert(row.addr()), "two sets claim one row");
        }
        drop(decoy);
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
            let cell = RecordCell::new(
                RecordInstance::new(name, CalcRecord::default()),
                0,
                false,
                None,
            );
            db.inner
                .record_locks
                .adopt(name.as_bytes(), cell.lock_record());
            db.ensure_set_for(name.as_bytes(), cell.lock_record());
            cell
        };
        let reader = cell("RI:A");
        let linked = cell("RI:B");
        let apart = cell("RI:C");
        db.merge_sets(b"RI:A", b"RI:B");

        let gate = db.lock_instance(&reader);
        let mut held = reader.write_in(gate.set());
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
        let cell = RecordCell::new(
            RecordInstance::new(name, CalcRecord::default()),
            0,
            false,
            None,
        );
        db.inner
            .record_locks
            .adopt(name.as_bytes(), cell.lock_record());
        cell
    }

    /// Boundary: a `cvt_dbaddr` owed to a set by a resolve that found the
    /// record busy follows the record when a merge moves it. The emptied set
    /// goes to the free list, where nothing holds it until `Registry::make_set`
    /// hands it out again, and then its drain skips a record no longer in it:
    /// an entry left there is never run.
    #[test]
    fn an_owed_cvt_dbaddr_follows_its_record_through_a_merge() {
        use crate::server::record::{RecordCell, RecordInstance};
        use crate::server::records::acalcout::AcalcoutRecord;
        use crate::types::EpicsValue;
        let db = PvDatabase::new();
        let cell = RecordCell::new(
            RecordInstance::new("OW:CALC", AcalcoutRecord::new()),
            0,
            false,
            None,
        );
        db.inner.record_locks.adopt(b"OW:CALC", cell.lock_record());
        db.build_lock_sets();
        db.ensure_set(b"OW:B");
        let amem = |cell: &RecordCell| cell.read().record.get_field("AMEM");

        let busy = cell.write();
        assert_eq!(cell.resolve_addr("AA"), Some(1));
        db.merge_sets(b"OW:B", b"OW:CALC");
        drop(busy);
        assert_eq!(
            db.inner.record_locks.set_id_of(b"OW:CALC"),
            db.inner.record_locks.set_id_of(b"OW:B"),
            "the record moved to the surviving set"
        );
        assert_eq!(amem(&cell), Some(EpicsValue::Long(8)));
    }

    /// Boundary `built == false`: a record adopted before `iocInit` waits on
    /// the bootstrap set, as C's null `lset` does until `dbLockInitRecords`.
    #[test]
    fn adopt_before_build_leaves_the_record_on_the_bootstrap_set() {
        let db = PvDatabase::new();
        let cell = adopted_cell(&db, "AD:PRE");
        assert!(cell.lock_record().is_bootstrap());
        assert_eq!(db.inner.record_locks.set_id_of(b"AD:PRE"), None);
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
            db.inner.record_locks.set_id_of(b"AD:POST"),
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
        assert!(db.inner.record_locks.set_id_of(b"AD:STRAGGLER").is_some());
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

    /// Boundary: the hold that gives the mutex up is the one that takes the
    /// depth to zero, not the one taken first. Dropping the outer guard
    /// first must leave the set held for the inner one.
    #[test]
    fn the_last_guard_dropped_releases_whichever_order_they_go() {
        let db = PvDatabase::new();
        let outer = db.lock_record("RE:ORDER");
        let inner = db.lock_record("RE:ORDER");

        let db2 = db.clone();
        let entered = Arc::new(AtomicUsize::new(0));
        let entered2 = entered.clone();
        let h = std::thread::spawn(move || {
            let _g = db2.lock_record("RE:ORDER");
            entered2.store(1, Ordering::SeqCst);
        });

        drop(outer);
        std::thread::sleep(SETTLE);
        assert_eq!(
            entered.load(Ordering::SeqCst),
            0,
            "the inner guard still holds the set"
        );
        drop(inner);
        h.join().unwrap();
        assert_eq!(entered.load(Ordering::SeqCst), 1);
    }

    /// Boundary: a many-lock epoch's reference on each set is given back
    /// when the epoch ends, and a single hold on the same set never counted.
    #[test]
    fn a_many_epoch_counts_its_reference_and_returns_it() {
        let db = PvDatabase::new();
        let lr = db.ensure_set(b"RE:MANY");
        let set = lr.set();
        let _plain = db.lock_record("RE:MANY");
        assert_eq!(set.many_holds.load(Ordering::Relaxed), 0);
        let epoch = db.lock_records(&["RE:MANY".to_string()]);
        assert_eq!(set.many_holds.load(Ordering::Relaxed), 1);
        drop(epoch);
        assert_eq!(set.many_holds.load(Ordering::Relaxed), 0);
    }

    /// Boundary: a thread blocked on a record's set while a merge runs under
    /// it wakes holding the old set; the re-check sends it to the set the
    /// record is behind now, and the guard it returns holds that one.
    #[test]
    fn a_merge_while_a_thread_waits_sends_it_to_the_new_set() {
        let db = PvDatabase::new();
        let a = db.ensure_set(b"MG:A");
        db.ensure_set(b"MG:B");
        let held = db.lock_record("MG:A");

        let db2 = db.clone();
        let a2 = a.clone();
        let h = std::thread::spawn(move || {
            let g = a2.acquire();
            (
                g.holds(&a2),
                a2.set().id,
                db2.inner.record_locks.set_id_of(b"MG:A"),
            )
        });
        std::thread::sleep(SETTLE);
        db.merge_sets(b"MG:A", b"MG:B");
        drop(held);
        let (holds, id, registry_id) = h.join().unwrap();
        assert!(holds, "the guard holds the set the record is behind");
        assert_eq!(Some(id), registry_id);
        assert_eq!(
            db.inner.record_locks.set_id_of(b"MG:A"),
            db.inner.record_locks.set_id_of(b"MG:B"),
            "the merge joined the two"
        );
        assert!(!a.set().is_locked(), "every hold has been given back");
    }

    /// Boundary: an epoch waiting on a set that a merge empties wakes still
    /// behind a set of each member, but holding the emptied one too; it
    /// retakes, so the emptied set goes back to the free list unheld and
    /// with no reference counted on it.
    #[test]
    fn a_many_epoch_waiting_on_a_set_a_merge_empties_lets_it_go() {
        let db = PvDatabase::new();
        let a = db.ensure_set(b"ME:A").set();
        db.ensure_set(b"ME:B");
        let held = db.lock_record("ME:A");

        let (taken_tx, taken_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let db2 = db.clone();
        let h = std::thread::spawn(move || {
            let epoch = db2.lock_records(["ME:A", "ME:B"]);
            taken_tx.send(()).unwrap();
            done_rx.recv().unwrap();
            drop(epoch);
        });
        std::thread::sleep(SETTLE);
        db.merge_sets(b"ME:B", b"ME:A");
        drop(held);
        taken_rx.recv().unwrap();

        assert_eq!(a.many_holds.load(Ordering::Relaxed), 0);
        let fresh = db.ensure_set(b"ME:C").set();
        assert!(std::ptr::eq(fresh, a), "the emptied set is re-used");
        assert!(!fresh.is_locked(), "the epoch let the emptied set go");
        done_tx.send(()).unwrap();
        h.join().unwrap();
    }

    /// Boundary: a first lock on a record still behind the bootstrap set
    /// (id 0) mints its set, and the guard returned is on that set, not on
    /// the bootstrap one.
    #[test]
    fn locking_an_instance_on_the_bootstrap_set_moves_it_off_first() {
        let db = PvDatabase::new();
        let cell = adopted_cell(&db, "BS:ONE");
        assert!(cell.lock_record().is_bootstrap());
        let guard = db.lock_instance(&cell);
        assert!(!cell.lock_record().is_bootstrap());
        assert!(guard._guard.holds(cell.lock_record()));
        drop(guard);
        assert!(!bootstrap_set().is_locked(), "the bootstrap hold is gone");
    }

    /// Boundary: a write under the frame's own set guard touches the borrow
    /// flag and nothing of the lock — the depth counter reads the same
    /// before, during and after, where `write()` would have counted a level.
    #[test]
    fn a_hold_inside_a_frame_leaves_the_depth_alone() {
        let db = PvDatabase::new();
        let cell = adopted_cell(&db, "FR:ONE");
        let gate = db.lock_instance(&cell);
        let set = cell.lock_record().set();
        assert_eq!(set.depth.load(Ordering::Relaxed), 1);
        {
            let mut held = cell.write_in(gate.set());
            held.common.putf = true;
            assert_eq!(set.depth.load(Ordering::Relaxed), 1);
        }
        assert_eq!(set.depth.load(Ordering::Relaxed), 1);
        // The flag came back: the plain route can take the record again, and
        // it is the one that counts a level.
        {
            let _again = cell.write();
            assert_eq!(set.depth.load(Ordering::Relaxed), 2);
        }
        drop(gate);
        assert!(!set.is_locked());
    }

    /// Boundary: the frame's guard is the proof, and a guard of another set
    /// is not one.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "another lock set")]
    fn a_hold_under_the_guard_of_another_set_is_refused() {
        let db = PvDatabase::new();
        let one = adopted_cell(&db, "FR:A");
        let other = adopted_cell(&db, "FR:B");
        let gate = db.lock_instance(&other);
        let _held = one.write_in(gate.set());
    }

    /// Boundary: the handle a frame function takes reads and writes under the
    /// frame's set and counts no level, however often it is asked.
    #[test]
    fn the_frame_handle_counts_no_level() {
        let db = PvDatabase::new();
        let cell = adopted_cell(&db, "FR:HANDLE");
        let gate = FrameGate::Taken(db.lock_instance(&cell));
        let frame = gate.frame(&cell);
        let set = cell.lock_record().set();
        for _ in 0..3 {
            frame.write().common.putf = true;
            let _a = frame.read();
            let _b = frame.read();
            assert_eq!(set.depth.load(Ordering::Relaxed), 1);
        }
        drop(gate);
        assert!(!set.is_locked());
    }

    /// Boundary: a recursive frame borrows its caller's hold — no level, no
    /// release — and a thread that holds nothing takes the set instead.
    #[test]
    fn a_recursive_frame_gate_borrows_the_hold_and_an_unheld_one_takes_it() {
        let db = PvDatabase::new();
        let cell = adopted_cell(&db, "FR:REC");
        let outer = db.lock_instance(&cell);
        let set = cell.lock_record().set();
        {
            let inner = FrameGate::of_held(cell.lock_record());
            assert!(matches!(inner, FrameGate::Held(_)));
            assert_eq!(set.depth.load(Ordering::Relaxed), 1);
            inner.frame(&cell).write().common.putf = true;
        }
        assert_eq!(set.depth.load(Ordering::Relaxed), 1);
        assert!(set.is_locked());
        drop(outer);
        assert!(!set.is_locked());
        {
            let taken = FrameGate::of_held(cell.lock_record());
            assert!(matches!(taken, FrameGate::Taken(_)));
            assert_eq!(set.depth.load(Ordering::Relaxed), 1);
        }
        assert!(!set.is_locked());
    }

    /// [`scan_lock_held`] counts where the set's mutex is taken and released —
    /// the free take, the contended take, the retake after `unheld` — so every
    /// body under a hold reads it, a re-entry adds nothing, and an
    /// [`InitPass`] (C `init_record`, no `dbScanLock`) reads no hold.
    #[cfg(debug_assertions)]
    #[test]
    fn scan_lock_held_follows_the_set_mutex() {
        let db = PvDatabase::new();
        let cell = adopted_cell(&db, "SH:REC");
        assert_eq!(scan_lock_held(), Some(false));
        let outer = db.lock_instance(&cell);
        assert_eq!(scan_lock_held(), Some(true), "free take");
        drop(db.lock_instance(&cell));
        assert_eq!(scan_lock_held(), Some(true), "a re-entry's release");
        cell.unheld(|| assert_eq!(scan_lock_held(), Some(false), "unheld"));
        assert_eq!(scan_lock_held(), Some(true), "retake");
        {
            let _init = InitPass::enter();
            assert_eq!(scan_lock_held(), Some(false), "init pass");
        }
        let (taken_tx, taken) = std::sync::mpsc::channel();
        let db2 = db.clone();
        let cell2 = cell.clone();
        let h = std::thread::spawn(move || {
            let _g = db2.lock_instance(&cell2);
            taken_tx.send(scan_lock_held()).unwrap();
        });
        std::thread::sleep(SETTLE);
        drop(outer);
        assert_eq!(scan_lock_held(), Some(false));
        assert_eq!(taken.recv().unwrap(), Some(true), "contended take");
        h.join().unwrap();
    }

    /// Boundary: `f` unwinds out of the park. The set is retaken, so the
    /// frame's outer guard releases a hold the thread owns and the next
    /// holder finds the set consistent.
    #[test]
    fn a_panic_inside_unheld_s_park_leaves_the_set_consistent_for_the_next_holder() {
        let db = PvDatabase::new();
        let cell = adopted_cell(&db, "UH:REC");
        let outer = db.lock_instance(&cell);
        let set = cell.lock_record().set();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cell.unheld(|| {
                assert!(!set.is_locked());
                panic!("park unwound");
            })
        }));
        assert!(r.is_err());
        assert!(set.is_locked());
        assert_eq!(set.owner.load(Ordering::Relaxed), thread_key());
        assert_eq!(set.depth.load(Ordering::Relaxed), 1);
        drop(outer);
        assert!(!set.is_locked());
        let again = db.lock_instance(&cell);
        assert_eq!(set.depth.load(Ordering::Relaxed), 1);
        drop(again);
        assert!(!set.is_locked());
    }

    /// The park gives the mutex up as the outermost release does, so what
    /// the set was owed runs first, still under the hold.
    #[test]
    fn a_callback_owed_to_the_set_runs_before_the_park_gives_it_up() {
        let db = PvDatabase::new();
        let cell = adopted_cell(&db, "UO:REC");
        let outer = db.lock_instance(&cell);
        let set = cell.lock_record().set();
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let l = log.clone();
        owe_callback(
            &cell,
            Box::new(move || l.lock().unwrap().push(set.is_locked())),
        );
        let l = log.clone();
        cell.unheld(|| l.lock().unwrap().push(set.is_locked()));
        assert_eq!(*log.lock().unwrap(), [true, false]);
        drop(outer);
        assert!(!set.is_locked());
    }

    /// Boundary: an owed callback unwinds out of the last guard's release.
    /// That drop does not run again, so the release gives the set up itself.
    #[test]
    fn a_panic_inside_an_owed_callback_still_gives_the_set_up() {
        let db = PvDatabase::new();
        let cell = adopted_cell(&db, "OP:REC");
        let outer = db.lock_instance(&cell);
        let set = cell.lock_record().set();
        owe_callback(&cell, Box::new(|| panic!("owed callback unwound")));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(outer)));
        assert!(r.is_err());
        assert!(!set.is_locked());
        assert_eq!(set.owner.load(Ordering::Relaxed), 0);
        assert_eq!(set.depth.load(Ordering::Relaxed), 0);
    }

    /// Boundary: the same unwind out of the park's release. The set is
    /// retaken for the frame's outer guard, which gives it up on the unwind.
    #[test]
    fn a_panic_inside_a_callback_owed_at_the_park_leaves_the_set_to_the_outer_guard() {
        let db = PvDatabase::new();
        let cell = adopted_cell(&db, "OQ:REC");
        let outer = db.lock_instance(&cell);
        let set = cell.lock_record().set();
        owe_callback(&cell, Box::new(|| panic!("owed callback unwound")));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _outer = outer;
            cell.unheld(|| ());
        }));
        assert!(r.is_err());
        assert!(!set.is_locked());
        assert_eq!(set.owner.load(Ordering::Relaxed), 0);
        assert_eq!(set.depth.load(Ordering::Relaxed), 0);
    }

    /// Boundary: a borrow that adds no depth is still refused at the park.
    #[test]
    fn a_park_with_a_live_borrow_of_the_record_panics_at_the_park() {
        let db = PvDatabase::new();
        let cell = adopted_cell(&db, "UB:REC");
        let outer = db.lock_instance(&cell);
        let set = cell.lock_record().set();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let gate = FrameGate::of_held(cell.lock_record());
            let frame = gate.frame(&cell);
            let _b = frame.read();
            cell.unheld(|| ());
        }));
        assert!(r.is_err());
        assert!(set.is_locked());
        assert_eq!(set.depth.load(Ordering::Relaxed), 1);
        drop(outer);
        assert!(!set.is_locked());
    }

    /// Boundary: an exclusive borrow through `write_in` is refused at the
    /// park as a shared one is.
    #[test]
    fn a_park_with_a_live_write_in_borrow_of_the_record_panics_at_the_park() {
        let db = PvDatabase::new();
        let cell = adopted_cell(&db, "UB:WIN");
        let outer = db.lock_instance(&cell);
        let set = cell.lock_record().set();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let gate = FrameGate::of_held(cell.lock_record());
            let frame = gate.frame(&cell);
            let _b = frame.write();
            cell.unheld(|| ());
        }));
        assert!(r.is_err());
        assert!(set.is_locked());
        drop(outer);
        assert!(!set.is_locked());
    }

    /// Boundary: a borrow of ANOTHER record, held by an outer frame, is
    /// refused at the park of this one (debug builds count borrows per thread;
    /// release builds keep the per-record check only).
    #[cfg(debug_assertions)]
    #[test]
    fn a_park_with_a_live_borrow_of_another_record_panics_at_the_park() {
        let db = PvDatabase::new();
        let outer_cell = adopted_cell(&db, "UB:OUTER");
        let parked = adopted_cell(&db, "UB:PARKED");
        let outer = db.lock_instance(&outer_cell);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let gate = FrameGate::of_held(outer_cell.lock_record());
            let frame = gate.frame(&outer_cell);
            let _b = frame.read();
            let _inner = db.lock_instance(&parked);
            parked.unheld(|| ());
        }));
        let msg = r.expect_err("the park must be refused");
        let msg = msg
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| msg.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        assert!(msg.contains("borrow of a record in its set"), "{msg}");
        drop(outer);
    }

    /// Boundary: a panic inside a frame body, with a nested frame on the
    /// same set and the record borrowed for writing, unwinds every hold. C
    /// has no unwinding to compare against; the port must not leave the
    /// set, its owner or the record's borrow flag behind for the next thread.
    #[test]
    fn a_panic_inside_a_frame_gives_the_set_and_the_record_back() {
        let db = PvDatabase::new();
        let cell = adopted_cell(&db, "FR:PANIC");
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let gate = FrameGate::Taken(db.lock_instance(&cell));
            db.with_frame(&cell, |inner| {
                let mut held = inner.write();
                held.common.putf = true;
                let _outer = gate.frame(&cell);
                panic!("frame body panicked");
            })
        }));
        assert!(caught.is_err());

        let set = cell.lock_record().set();
        assert!(!set.is_locked(), "the mutex was released on unwind");
        assert_eq!(set.depth.load(Ordering::Relaxed), 0);
        assert_eq!(set.owner.load(Ordering::Acquire), 0);

        // Another thread takes the set fresh and finds the borrow flag clear:
        // `write` asserts it is, and a non-reentrant hold would block here.
        let other = cell.clone();
        std::thread::spawn(move || {
            other.write().common.putf = false;
        })
        .join()
        .expect("the next thread takes the set and the record");
    }

    /// One poll of a future that never suspends — `add_record` on a database
    /// with no runtime work to wait for — so these tests stay reactor-free.
    fn ready<F: std::future::Future>(fut: F) -> F::Output {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        match std::pin::pin!(fut).poll(&mut cx) {
            std::task::Poll::Ready(out) => out,
            std::task::Poll::Pending => panic!("the future suspended"),
        }
    }

    /// Boundary: a link-field write from inside a frame — an OUT link aimed
    /// at its target's `FLNK` (`links.rs` `write_db_link_value` →
    /// `put_pv_already_locked`). The put is refused, as C `dbPut` refuses a
    /// link field, but the `link_field_write` guard still relinks while the
    /// frame holds its set; the frame's record must stay in the held set
    /// (Rule R). The frame's write borrow is released first: the relink
    /// reads the frame record's link text.
    #[test]
    fn an_in_frame_link_field_write_keeps_the_frame_record_in_its_set() {
        use crate::server::record::{LinkFieldType, parse_link_field};
        use crate::server::records::ai::AiRecord;
        use crate::types::EpicsValue;

        let db = PvDatabase::new();
        for name in ["LF:SRC", "LF:DST", "LF:NEXT"] {
            ready(db.add_record(name, Box::new(AiRecord::new(0.0)))).expect("add_record");
        }
        {
            let src = db.get_record("LF:SRC").expect("record");
            let mut inst = src.write();
            inst.common.text.inp = "LF:DST".into();
            inst.set_link_cache("INP", parse_link_field("LF:DST", LinkFieldType::In));
        }
        db.build_lock_sets();
        let locks = &db.inner.record_locks;
        let before = locks.set_id_of(b"LF:SRC").expect("a set");
        assert_eq!(locks.set_id_of(b"LF:DST"), Some(before));
        assert_ne!(locks.set_id_of(b"LF:NEXT"), Some(before));

        let src = db.get_record("LF:SRC").expect("record");
        let gate = FrameGate::Taken(db.lock_instance(&src));
        let frame = gate.frame(&src);
        frame.write().common.putf = true;

        let crate::server::record::ParsedLink::Db(link) =
            crate::server::record::parse_output_link_v2("LF:DST.FLNK")
        else {
            panic!("a local DB link");
        };
        let mut visited = crate::server::database::ProcStack::new();
        let failed = db.write_db_link_value(
            &link,
            EpicsValue::String("LF:NEXT".into()),
            super::super::links::OutLinkSrc {
                putf: false,
                notify: super::super::links::OutNotify::None,
                field: "OUT",
            },
            &mut visited,
        );
        assert!(failed, "dbPut refuses a link field");
        assert_eq!(
            db.get_record("LF:DST")
                .expect("record")
                .read()
                .common
                .text
                .flnk,
            ""
        );

        assert_eq!(
            locks.set_id_of(b"LF:SRC"),
            Some(before),
            "the frame's record stayed"
        );
        assert_eq!(locks.set_id_of(b"LF:DST"), Some(before));
        assert_ne!(locks.set_id_of(b"LF:NEXT"), Some(before));
        assert!(gate.set().holds(src.lock_record()));
        // The frame handle still borrows under the held set after the relink.
        frame.write().common.putf = false;
        drop(gate);
        assert!(!src.lock_record().set().is_locked());
    }

    /// Boundary: `ProcessAction::ArmWatchdog` run inside the frame
    /// `execute_process_actions` takes for a `special()` drain (histogram
    /// SDEL). C `wdogInit` runs under the caller's `dbScanLock` and takes
    /// nothing more, so the arm must read the record under the frame's
    /// single hold, not re-acquire its set.
    #[test]
    fn an_in_frame_watchdog_arm_reads_under_the_frame_s_single_hold() {
        use crate::error::CaResult;
        use crate::server::record::{FieldDesc, ProcessAction, ProcessOutcome, Record, RecordCell};
        use crate::types::EpicsValue;
        use std::sync::{Mutex, OnceLock, Weak};

        /// Arms a watchdog and records its set's depth each time the test
        /// thread asks for the interval.
        struct WatchdogProbe {
            cell: Arc<OnceLock<Weak<RecordCell>>>,
            depths: Arc<Mutex<Vec<usize>>>,
            thread: std::thread::ThreadId,
        }

        impl Record for WatchdogProbe {
            fn record_type(&self) -> &'static str {
                "watchdog_depth_probe"
            }
            fn process(&mut self) -> CaResult<ProcessOutcome> {
                Ok(ProcessOutcome::complete())
            }
            fn get_field(&self, name: &str) -> Option<EpicsValue> {
                match name {
                    "VAL" => Some(EpicsValue::Double(0.0)),
                    _ => None,
                }
            }
            fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
                Ok(())
            }
            fn declared_fields(&self) -> &'static [FieldDesc] {
                &[]
            }
            fn watchdog_interval(&self) -> Option<Duration> {
                // `add_record` asks before the cell is known and the armed
                // tick asks from a callback thread; only the arm itself, on
                // the test thread, is measured.
                let cell = self.cell.get().and_then(Weak::upgrade);
                if let Some(cell) = cell.filter(|_| std::thread::current().id() == self.thread) {
                    let depth = cell.lock_record().set().depth.load(Ordering::Relaxed);
                    self.depths.lock().expect("probe mutex").push(depth);
                }
                Some(Duration::from_secs(3600))
            }
        }

        let cell = Arc::new(OnceLock::new());
        let depths = Arc::new(Mutex::new(Vec::new()));
        let db = PvDatabase::new();
        ready(db.add_record(
            "WD:ARM",
            Box::new(WatchdogProbe {
                cell: Arc::clone(&cell),
                depths: Arc::clone(&depths),
                thread: std::thread::current().id(),
            }),
        ))
        .expect("add_record");
        db.build_lock_sets();
        let rec = db.get_record("WD:ARM").expect("record");
        cell.set(Arc::downgrade(&rec)).expect("cell set once");
        let generation = rec.read().watchdog_generation.clone();
        let before = generation.load(Ordering::Acquire);

        let mut visited = crate::server::database::ProcStack::new();
        db.execute_process_actions(b"WD:ARM", &rec, [ProcessAction::ArmWatchdog], &mut visited);

        assert_eq!(
            generation.load(Ordering::Acquire),
            before + 1,
            "the arm bumped the watchdog generation once"
        );
        assert_eq!(
            *depths.lock().expect("probe mutex"),
            [1],
            "the arm read the record once, under the frame's single hold"
        );
        // Both armed ticks (`add_record`'s and this one) take the set from a
        // callback thread, so whether it is locked says nothing about the
        // frame. This thread's own take is fresh only if the frame let go.
        let set = rec.lock_record().set();
        let hold = rec.lock_record().acquire();
        assert_eq!(set.owner.load(Ordering::Acquire), thread_key());
        assert_eq!(
            set.depth.load(Ordering::Relaxed),
            1,
            "the frame released the set"
        );
        drop(hold);
    }
}
