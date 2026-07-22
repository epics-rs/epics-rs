//! Database-level record locking — the Rust counterpart of the
//! C-EPICS `dbScanLock` / `dbScanLockMany` machinery and pvxs's
//! `ioc::DBManyLock` / `ioc::DBManyLocker`.
//!
//! C EPICS / pvxs background
//! -------------------------
//! Every `dbPutField` / `dbProcess` in C EPICS takes the target
//! record's `dbCommon::lock` mutex via `dbScanLock(precord)`
//! (`dbLock.c`). A multi-record transaction — a QSRV *atomic group*
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
//!   `pvxs/ioc/groupsource.cpp:444,569` — atomic group GET/PUT.
//! * `pvxs/ioc/pvalink_channel.cpp:386,422` — `DBManyLock` /
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
//! This module adds the missing layer: a single per-record
//! **advisory write gate** registry keyed by canonical record name.
//! It is the direct analogue of `dbCommon::lock`:
//!
//! * A plain CA/PVA write (`put_record_field_from_ca`, `put_pv`,
//!   `process_record`) takes the single record's gate for the
//!   duration of the write via [`PvDatabase::lock_record`].
//! * A multi-record transaction — the QSRV atomic group PUT/GET
//!   and the pvalink atomic scan-on-update epoch —
//!   takes *all* member-record gates up-front via
//!   [`PvDatabase::lock_records`], sorted by canonical record name so
//!   two overlapping transactions acquire shared records in the same
//!   order and cannot deadlock.
//!
//! Because every path takes the same gate from the same registry, a
//! direct backing-record write blocks until the transaction owning
//! that record finishes, and a QSRV atomic group PUT and a pvalink
//! atomic scan can never interleave on a shared record — restoring
//! the `DBManyLock` exclusion that earlier review found missing.
//!
//! The gate is *advisory*: it does not replace the per-record
//! `parking_lot::RwLock<RecordInstance>` that still guards the record's data. It
//! is an additional serialization layer that the multi-record
//! transaction owner and the single-record writers both honour,
//! exactly as `dbScanLock` is a layer above the record's own field
//! storage.
//!
//! What the gate *is* — a blocking priority-inheritance mutex
//! ----------------------------------------------------------
//! The gate is a [`crate::runtime::sync::PriorityInheritanceMutex`], the
//! same primitive L46 (`registration_mutex`), L8a (`simple_pvs`) and L8b
//! (one `scan_index` bucket) already use, and [`PvDatabase::lock_record`] /
//! [`PvDatabase::lock_records`] are plain synchronous `fn`s returning RAII
//! guards. This is `doc/rtems-priority-locks-design.md` §5 steps 5–6, and it
//! is the parity shape rather than a Rust-side invention: C's `dbScanLock`
//! takes `precord->mlok`, a plain `epicsMutex`, and on the RTEMS arm
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
//! could order them (`doc/rtems-priority-locks-design.md` §0 finding 4, §2
//! option C). It was the async bridge, not the target design.
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
//!   territory (`doc/rtems-priority-locks-design.md` §5 step 7).
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
//! `doc/rtems-priority-locks-design.md` §3's cross-check requires this to be
//! written down, because every lock in the chain is now *blocking* and a
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
//! [`RecordLockRegistry`]'s own map mutex (a `std::sync::Mutex`, unchanged by
//! this flip) is *not* a rung of that order: it is taken and released inside
//! [`RecordLockRegistry::gate_for`], strictly before the record gate it
//! returns is locked, and no other lock is ever taken while it is held.
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
//! ### The rule's teeth are structural, and now they cover L1 too
//!
//! Every guard in the list above is `!Send`. A `!Send` value held across an
//! `.await` makes the enclosing future `!Send`, which the compiler rejects at
//! every `tokio::spawn` / `runtime::task::spawn` site in this workspace — so
//! "no suspension point inside a gate window" is a build error rather than a
//! review convention. That is the structural guarantee `doc/rtems-priority-locks-design.md`
//! §5 steps 5–6 (holders H1–H9) were staged to make reachable: each holder
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

use std::collections::HashMap;

use crate::runtime::sync::{PriorityInheritanceMutex, PriorityInheritanceMutexGuard};

use super::PvDatabase;

/// One record's advisory write gate, and the lifetime it is handed out with.
///
/// `&'static` rather than `Arc`: [`RecordLockRegistry`] never removes an
/// entry (see its doc), so a gate's lifetime already *is* the process's, and
/// the PI mutex — a `pthread_mutex_t` on the target arms — has no owned-guard
/// API to build an `Arc`-carrying guard from. Leaking one allocation per
/// record at first use makes the guards genuinely `'static` with no `unsafe`
/// and no reference counting on the write hot path, and frees exactly as much
/// memory as the previous `Arc`-in-a-never-pruned-map did: none.
type Gate = &'static PriorityInheritanceMutex<()>;

/// Registry of per-record advisory write gates.
///
/// Lazily allocates one gate per canonical record name on first use.
/// Entries are never removed: an EPICS database is loaded once at IOC init
/// and the record set is effectively static, so the map size is bounded by
/// the record count and removing entries would reintroduce a TOCTOU race
/// with a concurrent locker.
#[derive(Default)]
pub(crate) struct RecordLockRegistry {
    gates: std::sync::Mutex<HashMap<String, Gate>>,
}

impl RecordLockRegistry {
    /// Return the advisory gate for `record`, creating it on first use.
    ///
    /// `record` must already be the canonical (alias-resolved) name;
    /// [`PvDatabase::lock_record`] / [`PvDatabase::lock_records`]
    /// resolve aliases before calling this so an alias and its target
    /// always share one gate.
    ///
    /// The map lock is released before the caller locks the gate — it is not
    /// a rung of the acquisition order, and holding it across a gate
    /// acquisition would serialise every record in the database behind one
    /// writer.
    fn gate_for(&self, record: &str) -> Gate {
        let mut map = self
            .gates
            .lock()
            .expect("record-lock registry mutex poisoned");
        match map.get(record) {
            Some(gate) => gate,
            None => {
                let gate: Gate = Box::leak(Box::new(PriorityInheritanceMutex::new(())));
                map.insert(record.to_string(), gate);
                gate
            }
        }
    }
}

/// RAII guard for a single record's advisory write gate.
///
/// Held for the duration of a plain CA/PVA write. Equivalent to the
/// `dbScanLock`+`dbScanUnlock` pair around one `dbPutField` in C
/// EPICS. `!Send`, so the compiler refuses to let it live across an
/// `.await` in any spawned future — see the module doc.
#[must_use = "the write gate is released as soon as the guard is dropped"]
pub struct RecordWriteGuard {
    _guard: PriorityInheritanceMutexGuard<'static, ()>,
}

/// RAII guard for an ordered set of record advisory write gates — the
/// `DBManyLocker` equivalent.
///
/// Acquired by [`PvDatabase::lock_records`] over every member record
/// of a multi-record transaction (QSRV atomic group PUT/GET, pvalink
/// atomic scan-on-update epoch); held across the whole member loop.
/// While alive, every plain CA/PVA write to any of those records — and
/// every other multi-record transaction sharing any of them — blocks.
/// `!Send`, exactly as [`RecordWriteGuard`] is.
#[must_use = "the locked epoch ends as soon as the guard is dropped"]
pub struct ManyRecordWriteGuard {
    // Guards drop in vector order; order does not matter for release.
    _guards: Vec<PriorityInheritanceMutexGuard<'static, ()>>,
}

impl PvDatabase {
    /// Acquire the advisory write gate for a single record.
    ///
    /// This is the `dbScanLock(precord)` analogue. The plain CA/PVA
    /// write path holds this for the duration of one record write so
    /// it cannot interleave with a multi-record transaction that owns
    /// the same record's gate via [`Self::lock_records`].
    ///
    /// `record` is alias-resolved internally, so an alias and its
    /// target always map to the same gate as [`Self::lock_records`]
    /// keys them.
    ///
    /// **Blocks the calling thread** if the gate is held. The gate is not
    /// reentrant — a caller that already owns it (a transaction owner inside
    /// its own [`Self::lock_records`] epoch) MUST use the `_already_locked`
    /// entry points instead, or it deadlocks against itself.
    pub fn lock_record(&self, record: &str) -> RecordWriteGuard {
        let canonical = self
            .resolve_alias(record)
            .unwrap_or_else(|| record.to_string());
        let gate = self.inner.record_locks.gate_for(&canonical);
        RecordWriteGuard {
            _guard: gate.lock(),
        }
    }

    /// Acquire the advisory write gates for a set of records — the
    /// `DBManyLock` / `DBManyLocker` equivalent.
    ///
    /// Every name is alias-resolved to its canonical record name, the
    /// set is sorted and de-duplicated, then the per-record gates are
    /// acquired in that sorted order. Sorting guarantees two
    /// overlapping multi-record transactions acquire their shared
    /// records in the same global order and cannot deadlock (mirrors
    /// pvxs `DBManyLock` sorting the lock set in `dbLock.c`); the
    /// dedup means a record bound by more than one member link is
    /// locked exactly once. That canonical order is load-bearing in a way
    /// it was not while the gate was async: an out-of-order acquisition now
    /// wedges two threads rather than two tasks.
    ///
    /// The returned [`ManyRecordWriteGuard`] must be held for the
    /// whole transaction. While it is alive, a concurrent plain write
    /// to any of those records blocks on [`Self::lock_record`], and a
    /// concurrent overlapping transaction blocks on this method.
    ///
    /// Names that do not resolve to a record still get a gate (keyed
    /// by the post-alias name) — matching `dbLockerAlloc`, which
    /// accepts the record pointers it is given without a liveness
    /// re-check.
    pub fn lock_records<I, S>(&self, records: I) -> ManyRecordWriteGuard
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        // Alias-resolve every name so two links naming the same record
        // via different aliases share one gate.
        let mut names: Vec<String> = Vec::new();
        for record in records {
            let record = record.as_ref();
            names.push(
                self.resolve_alias(record)
                    .unwrap_or_else(|| record.to_string()),
            );
        }
        // Deadlock-free canonical order: sort + dedup so the same
        // record is locked once and overlapping transactions share an
        // acquisition order.
        names.sort_unstable();
        names.dedup();

        let mut guards = Vec::with_capacity(names.len());
        for name in &names {
            guards.push(self.inner.record_locks.gate_for(name).lock());
        }
        ManyRecordWriteGuard { _guards: guards }
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
    // of demonstrating exclusion. Being reactor-free they also run under
    // `--features rtems-exec-model` and add no site to this file's
    // RTEMS-EXEC-MODEL-ALLOW census.

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

    /// An alias and its target share one gate — the property every caller
    /// relies on when it hands `lock_record` a client-supplied name and
    /// `lock_records` a link-derived one.
    #[test]
    fn alias_and_target_share_one_gate() {
        let db = PvDatabase::new();
        let registry = &db.inner.record_locks;
        assert!(
            std::ptr::eq(registry.gate_for("REC:A"), registry.gate_for("REC:A")),
            "the same canonical name must map to the same gate instance"
        );
        assert!(
            !std::ptr::eq(registry.gate_for("REC:A"), registry.gate_for("REC:B")),
            "distinct records must not share a gate"
        );
    }
}
