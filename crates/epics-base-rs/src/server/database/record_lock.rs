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
//! `epics-base-rs` stores each record behind its own
//! `parking_lot::RwLock<RecordInstance>`, but the put/process helpers
//! (`put_record_field_from_ca`, `put_pv`, `process_record`,
//! `process_record_with_links`) acquire that `RwLock` *internally*
//! and recurse into link targets, so a caller cannot hold N
//! `write_owned()` guards across the member loop without dead-locking
//! the recursive link processing.
//!
//! This module adds the missing layer: C's **lock sets**, one mutex per
//! connected component of the DB-link graph, with the record-to-set map C
//! keeps in `dbCommon::lset`.
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
//! The gate is *advisory*: it does not replace the per-record
//! `parking_lot::RwLock<RecordInstance>` that still guards the record's data. It
//! is an additional serialization layer that the multi-record
//! transaction owner and the single-record writers both honour,
//! exactly as `dbScanLock` is a layer above the record's own field
//! storage.
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
//! >    the `records` map, `aliases`, and a record's own
//! >    `RwLock<RecordInstance>` (`parking_lot::RwLock` — C has no
//! >    reader-writer lock to be PI-faithful to, §5.3 addendum).
//! >
//! > Every rung is a blocking lock. There is no async lock left anywhere on
//! > the put/process path, which is what makes the order a MUST rather than a
//! > preference: a cycle wedges a thread.
//!
//! [`RecordLockRegistry`]'s own mutex (a `std::sync::Mutex`) is *not* a rung of
//! that order: it is taken and released inside `RecordLockRegistry::set_of`,
//! strictly before the lock set it returns is taken, and no other lock is ever
//! acquired while it is held. A guard's release path deliberately does not
//! touch it — everything a release needs lives in the set's own cell — because
//! taking it while holding a set would close a cycle against that rule.
//!
//! **Owner/Gate:** [`PvDatabase::update_scan_index`] is the **only** production
//! function that takes L46 from inside an L1-held window, and therefore the
//! single owner of the whole L1 → L46 → L8b chain. Every other L46 holder
//! (`add_pv`, `add_pv_with_hooks_full`, `remove_simple_pv`,
//! `add_loaded_record`, `remove_record`, `add_alias`, `add_breaktables`) is a
//! registration entry point reached from `.db` load, iocsh or the gateway,
//! never from inside a put/process cycle — verified with `rg` over those
//! symbols in `field_io.rs`, `processing.rs`, `links.rs`, `qsrv/group.rs` and
//! `pvalink/integration.rs`, where every hit is inside a `#[cfg(test)]`
//! module. A second function that nests L46 under L1 is a second owner of
//! this chain: route it through `update_scan_index` instead, or the order
//! above stops being checkable by reading one function.
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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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
/// while holding the set would close a cycle against [`Registry::set_of`],
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
}

/// The `'static` handle to a lock set. Sets are leaked: a guard hands out a
/// `'static` borrow of the mutex, and a set outlives every record it holds —
/// C keeps its emptied sets on `lockSetsFree` for the same reason and frees
/// them only in `dbLockCleanupRecords` (`dbLock.c:563-576`).
type Set = &'static LockSet;

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

/// A process-unique non-zero key for the current thread.
///
/// `ThreadId` has no stable integer form on stable Rust, and the value only
/// has to be comparable and never reused while a thread lives.
fn thread_key() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    thread_local! {
        static KEY: u64 = NEXT.fetch_add(1, Ordering::Relaxed);
    }
    KEY.with(|k| *k)
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
    fn acquire(&'static self, many: bool) -> SetGuard {
        let me = thread_key();
        if self.owner.load(Ordering::Acquire) == me {
            self.depth.fetch_add(1, Ordering::Relaxed);
            if many {
                self.many_holds.fetch_add(1, Ordering::Relaxed);
            }
            return SetGuard {
                set: self,
                guard: None,
                many,
            };
        }
        let guard = self.lock.lock();
        self.owner.store(me, Ordering::Release);
        self.depth.store(1, Ordering::Relaxed);
        if many {
            self.many_holds.fetch_add(1, Ordering::Relaxed);
        }
        SetGuard {
            set: self,
            guard: Some(guard),
            many,
        }
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
struct SetGuard {
    set: Set,
    /// `None` for a recursive re-entry, which acquired no new mutex.
    guard: Option<PriorityInheritanceMutexGuard<'static, ()>>,
    many: bool,
}

impl Drop for SetGuard {
    fn drop(&mut self) {
        if self.many {
            self.set.many_holds.fetch_sub(1, Ordering::Relaxed);
        }
        if self.guard.is_some() {
            // Clear ownership BEFORE the mutex is released, or the next owner
            // could publish itself and be overwritten by this store.
            self.set.depth.store(0, Ordering::Relaxed);
            self.set.owner.store(0, Ordering::Release);
        } else {
            self.set.depth.fetch_sub(1, Ordering::Relaxed);
        }
    }
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
    /// C's `dbCommon::lset` → `lockRecord::plockSet` chain, by canonical
    /// record name.
    of_record: HashMap<String, u64>,
    next_id: u64,
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
        let id = self.next_id;
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
        }));
        drop(seq);
        set
    }

    /// The set `record` belongs to, creating a one-member set if it has none.
    ///
    /// C has no lazy path: `dbLockInitRecords` gives every record a set before
    /// anything can lock one, and `dblsr` returns early for a record whose
    /// `lset` is still null (`dbLock.c:900-901`). This port lets a
    /// programmatic database take a gate with no `iocInit`, and
    /// [`PvDatabase::lock_records`] accepts a name that never was a record at
    /// all, so a missing set is created here exactly as `createLockRecord`
    /// (`:505-527`) would have created it.
    fn set_of(&mut self, record: &str) -> Set {
        if let Some(id) = self.of_record.get(record) {
            return self.active[id].set;
        }
        let set = self.make_set();
        self.of_record.insert(record.to_string(), set.id);
        self.active.insert(
            set.id,
            SetState {
                set,
                members: BTreeSet::from([record.to_string()]),
            },
        );
        set
    }

    /// C `dbLockSetMerge` (`dbLock.c:580-666`): every record behind
    /// `second`'s mutex moves behind `first`'s, and the emptied set goes on
    /// the free list with its id and mutex intact.
    ///
    /// The direction matters and is C's: the SOURCE record's set survives
    /// (`dbDbLink.c:110` passes `plink->precord` first), so which id ends up
    /// holding a component depends on link order exactly as in C.
    fn merge(&mut self, first: &str, second: &str) {
        let a = self.set_of(first).id;
        let b = self.set_of(second).id;
        if a == b {
            return;
        }
        let moved = self
            .active
            .remove(&b)
            .expect("every id in of_record names an active set");
        for name in &moved.members {
            self.of_record.insert(name.clone(), a);
        }
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
    fn repartition(&mut self, seed: &str, adjacency: &HashMap<String, BTreeSet<String>>) {
        // A record that joined the database after `iocInit` has no set yet.
        // C's `dbCreateRecord` cannot run then at all; the port allows it, so
        // the record is given its own set here exactly as `createLockRecord`
        // would have, and the components below fold it in.
        let seed_id = if adjacency.contains_key(seed) {
            self.set_of(seed).id
        } else {
            let Some(id) = self.of_record.get(seed).copied() else {
                return;
            };
            id
        };

        let mut affected: BTreeSet<String> = BTreeSet::new();
        let mut work: Vec<String> = vec![seed.to_string()];
        while let Some(name) = work.pop() {
            if !affected.insert(name.clone()) {
                continue;
            }
            if let Some(targets) = adjacency.get(&name) {
                work.extend(targets.iter().cloned());
            }
            if let Some(id) = self.of_record.get(&name).copied() {
                work.extend(self.active[&id].members.iter().cloned());
            }
        }

        // Every set the region covers. Each is emptied below and either
        // re-used for one of the new components or returned to the free list.
        let touched: BTreeSet<u64> = affected
            .iter()
            .filter_map(|name| self.of_record.get(name).copied())
            .collect();

        // A record the database no longer has leaves the partition with it —
        // `dbDeleteRecord` frees the `lockRecord` — so it is dropped here
        // rather than being carried into a component of one.
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
                .map(|name| self.of_record.get(name).copied());
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
            for name in &component {
                self.of_record.insert(name.clone(), set.id);
            }
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
    }

    /// C `dbLockInitRecords` (`dbLock.c:526-532`) through `createLockRecord`:
    /// one set per record, before any link has merged anything.
    fn init_records(&mut self, names: &[String]) {
        for name in names {
            self.set_of(name);
        }
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

    /// The set `record` locks through, created on first use.
    ///
    /// The registry lock is released before the caller takes the set — it is
    /// not a rung of the acquisition order, and holding it across an
    /// acquisition would serialise every record in the database behind one
    /// writer.
    fn set_of(&self, record: &str) -> Set {
        self.lock().set_of(record)
    }

    /// Which set `record` is behind right now, without creating one.
    fn set_id_of(&self, record: &str) -> Option<u64> {
        self.lock().of_record.get(record).copied()
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
        let mut names: Vec<String> = self.inner.records.read().keys().cloned().collect();
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
        let mut registry = self.inner.record_locks.lock();
        registry.init_records(&names);
        for (from, to) in edges {
            registry.merge(&from, &to);
        }
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
                        .unwrap_or_else(|| link.target().record);
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
        let id = registry.of_record.get(&canonical).copied()?;
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
        if self.inner.record_locks.lock().of_record.is_empty() {
            return None;
        }
        Some(LockSetEdit {
            db: self,
            record: canonical,
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
        if self.inner.record_locks.lock().of_record.is_empty() {
            return None;
        }
        Some(LockSetEdit {
            db: self,
            record: self
                .resolve_alias(record)
                .unwrap_or_else(|| record.to_string()),
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
    fn relink_lock_sets(&self, record: &str) {
        // The whole adjacency is read BEFORE the registry lock, because
        // `record_link_fields` takes the record map and each record's own
        // lock and neither may be acquired underneath the registry — the same
        // rule `build_lock_sets` states.
        let adjacency = self.db_link_adjacency();
        let mut registry = self.inner.record_locks.lock();
        registry.repartition(record, &adjacency);
    }

    /// The DB-link graph as an undirected adjacency map.
    ///
    /// Undirected because C's merge is: `dbLockSetMerge(locker, plink->precord,
    /// target)` puts both endpoints behind one mutex regardless of which way
    /// the link points, and `dbLockSetSplit` walks `bklnk` as well as the
    /// record's own links (`dbLock.c:735-770`) for the same reason. A record
    /// that is only ever pointed AT is as much a member as the one pointing.
    fn db_link_adjacency(&self) -> HashMap<String, BTreeSet<String>> {
        let names: Vec<String> = self.inner.records.read().keys().cloned().collect();
        let mut adjacency: HashMap<String, BTreeSet<String>> = HashMap::new();
        for name in &names {
            adjacency.entry(name.clone()).or_default();
        }
        for name in &names {
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
}

impl Drop for LockSetEdit<'_> {
    fn drop(&mut self) {
        self.db.relink_lock_sets(&self.record);
    }
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
    /// `record` is alias-resolved internally, so an alias and its target
    /// always reach the same set.
    ///
    /// **Blocks the calling thread** when another thread holds the set. A
    /// thread that already holds it recurses, as C's recursive `epicsMutex`
    /// does — which is not optional once a set spans a whole link component,
    /// because processing a record and then its link target takes one mutex
    /// twice.
    pub fn lock_record(&self, record: &str) -> RecordWriteGuard {
        let canonical = self
            .resolve_alias(record)
            .unwrap_or_else(|| record.to_string());
        loop {
            let set = self.inner.record_locks.set_of(&canonical);
            let guard = set.acquire(false);
            // C `dbScanLock`'s `retry:` (`dbLock.c:194-213`): a merge can move
            // the record to another set between the lookup and the
            // acquisition, and the set just taken would then guard nothing.
            if self.inner.record_locks.set_id_of(&canonical) == Some(set.id) {
                return RecordWriteGuard { _guard: guard };
            }
            drop(guard);
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

        loop {
            let mut sets: Vec<Set> = names
                .iter()
                .map(|name| self.inner.record_locks.set_of(name))
                .collect();
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
        let registry = &db.inner.record_locks;
        assert!(
            std::ptr::eq(registry.set_of("REC:A"), registry.set_of("REC:A")),
            "the same canonical name must map to the same lock set"
        );
        assert!(
            !std::ptr::eq(registry.set_of("REC:A"), registry.set_of("REC:B")),
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
}
