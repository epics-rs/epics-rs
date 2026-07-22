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
//! Wait discipline — priority-ordered, not FIFO
//! -------------------------------------------
//! The gate used to be a `tokio::sync::Mutex<()>`, whose waiter queue is
//! strictly FIFO. On the RTEMS backend both ends of the contention pair
//! are banded IOC threads parked in `std::thread::park()`
//! (`doc/rtems-priority-locks-design.md` §0 finding 4): a `scan-1` thread
//! at EPICS 63 waits behind a `CAS-client` thread at EPICS 20 purely
//! because the low-band thread asked first. Nothing in the kernel can fix
//! that — the waiters are parked on a userspace queue it cannot see — so
//! the queue itself has to be ordered. [`PriorityGate`] orders it by the
//! waiter's declared EPICS band ([`crate::runtime::task::current_thread_band`],
//! published by `enter_ioc_thread`), highest band first, FIFO among equal
//! bands.
//!
//! This closes handoff §8.0 **gap 3** only. It does **not** give the gate
//! priority inheritance: there is still no kernel-visible owner, so a
//! preempted low-band holder is still not boosted. That is gap 4, and it is
//! `doc/rtems-priority-locks-design.md` §5 step 4 (L1 → `PriorityInheritanceMutex`),
//! which discards the ordering logic below because a PI pthread mutex
//! orders by priority in the kernel.
//!
//! ### The wake invariant — MUST
//!
//! > **Only the thread that owns the gate may pop a waiter off the queue.**
//! > Ownership is handed *directly* from the releaser to the popped waiter —
//! > `held` is never cleared while the queue is non-empty — so no third
//! > party can barge in between the pop and the wake. A waiter that is
//! > cancelled while merely queued MUST remove its own entry and nothing
//! > else; a waiter that is cancelled *after* being handed ownership MUST
//! > perform the release itself, because at that moment it **is** the owner.
//!
//! **Owner/gate:** [`PriorityGate::hand_off`] is the single pop site. It is
//! reached from exactly two places, both of which own the gate when they
//! call it: [`PriorityGate::release`] (run by `GateGuard::drop`) and
//! `GateAcquire::drop` on the granted-then-cancelled path. Adding a third
//! caller means adding a third owner and re-opening the barge/lost-wake
//! family this rule exists to close — do not pop the queue anywhere else,
//! including from an "opportunistic" fast path in `poll`.
//!
//! The invariant that makes the fast path safe is the converse: a waiter is
//! only ever enqueued while `held` is true, and `held` is only ever cleared
//! with an empty queue, so `!held` implies "no waiters" and a first-poll
//! acquisition cannot jump the queue.
//!
//! Acquisition order — MUST
//! ------------------------
//! `doc/rtems-priority-locks-design.md` §3's cross-check requires this to be
//! written down, because as of step 4 the locks nested under L1 are *blocking*
//! and a cycle would wedge a thread rather than a task. The order below is the
//! one the code actually takes, not an aspiration — it was derived by reading
//! every nesting site, and the bypass audit is in the commit that added it.
//!
//! > **A thread MUST acquire these in this order and MUST NOT acquire any of
//! > them while holding one that appears later:**
//! >
//! > 1. **L1** — the per-record advisory gate ([`PvDatabase::lock_record`] /
//! >    [`PvDatabase::lock_records`]), *this* module.
//! > 2. **L46** — `PvDatabaseInner::registration_mutex`.
//! > 3. the leaves, none of which is ever held while another lock is taken:
//! >    **L8a** `simple_pvs`, **L8b** one `scan_index` bucket, the `records`
//! >    map, `aliases`, and a record's own `RwLock<RecordInstance>`.
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
//! The rule's teeth are structural for the two rightmost steps: an L46 or
//! L8a/L8b guard is `!Send`, so the compiler refuses any hold across an
//! `.await` at the `tokio::spawn` sites, which is what stops a holder from
//! suspending mid-chain. L1 is not covered by that yet — see below.
//!
//! ### What L1 is today, and the one await still inside its window
//!
//! L1 is still the async [`PriorityGate`] and `lock_record` / `lock_records`
//! are still `async fn`. Step 4 deliberately stopped short of the type flip:
//! seven of the nine holders (`doc/rtems-priority-locks-design.md` §1.1) hold
//! the gate across an `.await`, so a `!Send` guard here would not compile
//! before H6 (§5 step 5) lands.
//!
//! Concretely, after step 4 the H1 (`field_io.rs` `put_pv_inner`) and H2
//! (`put_pv_and_post_with_origin`) windows each contain **exactly one**
//! `.await`, and it is the same one in both: `run_special_actions`. The
//! scan-index tail that used to sit beside it is synchronous now. That last
//! await is not removable here — it re-enters H1/H6. It no longer reaches a
//! `ca://`/`pva://` network write: `write_external_pv` stages the write on the
//! database's link-put queue and returns, as C `dbCaPutLink` does
//! (`dbCa.c:544-631`), so the only lset call left inside the window is the
//! cached-state `put_admission` probe. See `run_special_actions`' own doc
//! comment for the exact list. The remaining holders (H4, H6, H7, H8, H9) are
//! untouched by step 4 and still await freely under the gate.

// RTEMS-EXEC-MODEL-ALLOW(5): checked - these run and pass in the feature-ON suite.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use crate::runtime::task::current_thread_band;

use super::PvDatabase;

/// Position of one waiter in a gate's queue.
///
/// Ordered by band **descending** then arrival **ascending**, which is
/// exactly the derived tuple order given the `Reverse` on the band: the
/// `BTreeMap`'s first entry is always the highest band, and among equal
/// bands always the one that arrived first. Equal-band FIFO is not a
/// detail — without it, two threads in the same band would be reordered
/// against each other by nothing but map iteration, which is the
/// starvation surprise a priority queue is otherwise blamed for.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct WaiterKey {
    band: Reverse<u8>,
    seq: u64,
}

/// Everything a gate knows, under one non-async lock.
///
/// `std::sync::Mutex` deliberately: the critical sections here are a map
/// insert/remove and a few field writes, never an `.await`, and a blocking
/// lock is the only kind whose `unlock` can be reached from `Drop` — which
/// is where cancellation is handled.
#[derive(Default)]
struct GateState {
    /// `true` from the moment ownership is taken until the moment it is
    /// released — *including* the window where ownership has been handed to
    /// a waiter that has not been polled yet. Never cleared while
    /// `waiters` is non-empty.
    held: bool,
    /// The waiter ownership was handed to, if any. It has already been
    /// popped from `waiters`; it learns of the grant on its next poll, or
    /// releases on its `Drop` if it is cancelled first.
    granted_to: Option<WaiterKey>,
    /// Arrival counter, giving equal-band waiters their FIFO order.
    next_seq: u64,
    /// Parked waiters, ordered by [`WaiterKey`]. A `None` waker means the
    /// entry was enqueued by a poll that has not re-registered yet; it
    /// cannot happen today (every insert carries a waker) but the shape
    /// keeps `hand_off` total.
    waiters: BTreeMap<WaiterKey, Option<Waker>>,
}

/// A single-owner advisory gate whose waiters are woken in EPICS-band
/// order — the replacement for `tokio::sync::Mutex<()>` described in the
/// module doc's wake-invariant section.
#[derive(Default)]
struct PriorityGate {
    state: std::sync::Mutex<GateState>,
}

impl PriorityGate {
    /// Acquire the gate, yielding an owned (`'static`, `Send`) guard.
    ///
    /// Cancel-safe: dropping the returned future before it completes leaves
    /// the gate exactly as it found it, whether the future was still queued
    /// or had already been handed ownership.
    fn lock_owned(self: &Arc<Self>) -> GateAcquire {
        GateAcquire {
            gate: Arc::clone(self),
            key: None,
        }
    }

    /// Give up ownership. Run by `GateGuard::drop`; see the module doc's
    /// MUST rule for who else may reach [`Self::hand_off`].
    fn release(&self) {
        let waker = {
            let mut state = self.lock_state();
            debug_assert!(state.held, "released a gate that was not held");
            debug_assert!(
                state.granted_to.is_none(),
                "the gate was granted to a waiter while an owner still held it"
            );
            Self::hand_off(&mut state)
        };
        // Outside the state lock: a waker may run arbitrary scheduler code,
        // and it must not be able to re-enter this gate's lock.
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// **The single pop site.** Hand ownership to the highest-ranked
    /// waiter, or drop it if there is none.
    ///
    /// Returns the waker to fire once the caller has released the state
    /// lock. Ownership is transferred *directly* — `held` stays `true`
    /// whenever a waiter was popped — so nothing can acquire the gate in
    /// the gap between the pop and the wake.
    fn hand_off(state: &mut GateState) -> Option<Waker> {
        match state.waiters.pop_first() {
            Some((key, waker)) => {
                state.granted_to = Some(key);
                waker
            }
            None => {
                state.held = false;
                None
            }
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().expect("record gate state mutex poisoned")
    }
}

/// The future returned by [`PriorityGate::lock_owned`].
struct GateAcquire {
    gate: Arc<PriorityGate>,
    /// `Some` exactly while this future occupies a place in the gate's
    /// queue *or* holds an unclaimed grant. Cleared when the guard is
    /// handed out, which is what makes [`Drop`] a no-op on the success
    /// path.
    key: Option<WaiterKey>,
}

impl Future for GateAcquire {
    type Output = GateGuard;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut state = this.gate.lock_state();
        match this.key {
            // First poll: take the gate outright if it is free. Free
            // implies no waiters (see the module doc's converse
            // invariant), so this cannot jump the queue.
            None => {
                if !state.held {
                    state.held = true;
                    drop(state);
                    return Poll::Ready(GateGuard {
                        gate: Arc::clone(&this.gate),
                    });
                }
                let key = WaiterKey {
                    band: Reverse(current_thread_band()),
                    seq: state.next_seq,
                };
                state.next_seq += 1;
                state.waiters.insert(key, Some(cx.waker().clone()));
                this.key = Some(key);
                Poll::Pending
            }
            Some(key) => {
                if state.granted_to == Some(key) {
                    // The releaser already transferred ownership to us;
                    // `held` was never cleared, so there is nothing to take.
                    state.granted_to = None;
                    drop(state);
                    this.key = None;
                    return Poll::Ready(GateGuard {
                        gate: Arc::clone(&this.gate),
                    });
                }
                if let Some(slot) = state.waiters.get_mut(&key) {
                    let stale = !matches!(slot, Some(waker) if waker.will_wake(cx.waker()));
                    if stale {
                        *slot = Some(cx.waker().clone());
                    }
                }
                Poll::Pending
            }
        }
    }
}

impl Drop for GateAcquire {
    fn drop(&mut self) {
        // `None` means never enqueued, or the guard was already handed out
        // — in the latter case the guard owns the release.
        let Some(key) = self.key.take() else {
            return;
        };
        let waker = {
            let mut state = self.gate.lock_state();
            if state.granted_to == Some(key) {
                // Cancelled after being handed ownership: this future *is*
                // the owner, so it owes the same release the guard would
                // have run. Skipping it wedges the gate forever.
                state.granted_to = None;
                PriorityGate::hand_off(&mut state)
            } else {
                // Cancelled while merely queued: remove this entry and
                // nothing else. Popping the queue here would be a second
                // owner for the wake transition.
                state.waiters.remove(&key);
                None
            }
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// Ownership of one [`PriorityGate`], released on drop.
///
/// `'static` and `Send` (it is just an `Arc`), which is what lets
/// [`RecordWriteGuard`] be held across `.await` points in a `tokio::spawn`ed
/// future exactly as the `OwnedMutexGuard` it replaces was.
struct GateGuard {
    gate: Arc<PriorityGate>,
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.gate.release();
    }
}

/// Registry of per-record advisory write gates.
///
/// Lazily allocates one `Arc<PriorityGate>` per canonical record name on
/// first use. Entries are never removed: an EPICS database is loaded
/// once at IOC init and the record set is effectively static, so the
/// map size is bounded by the record count and removing entries would
/// reintroduce a TOCTOU race with a concurrent locker.
#[derive(Default)]
pub(crate) struct RecordLockRegistry {
    gates: std::sync::Mutex<HashMap<String, Arc<PriorityGate>>>,
}

impl RecordLockRegistry {
    /// Return the advisory gate for `record`, creating it on first use.
    ///
    /// `record` must already be the canonical (alias-resolved) name;
    /// [`PvDatabase::lock_record`] / [`PvDatabase::lock_records`]
    /// resolve aliases before calling this so an alias and its target
    /// always share one gate.
    fn gate_for(&self, record: &str) -> Arc<PriorityGate> {
        let mut map = self
            .gates
            .lock()
            .expect("record-lock registry mutex poisoned");
        map.entry(record.to_string())
            .or_insert_with(|| Arc::new(PriorityGate::default()))
            .clone()
    }
}

/// RAII guard for a single record's advisory write gate.
///
/// Held for the duration of a plain CA/PVA write. Equivalent to the
/// `dbScanLock`+`dbScanUnlock` pair around one `dbPutField` in C
/// EPICS.
pub struct RecordWriteGuard {
    _guard: GateGuard,
}

/// RAII guard for an ordered set of record advisory write gates — the
/// `DBManyLocker` equivalent.
///
/// Acquired by [`PvDatabase::lock_records`] over every member record
/// of a multi-record transaction (QSRV atomic group PUT/GET, pvalink
/// atomic scan-on-update epoch); held across the whole member loop.
/// While alive, every plain CA/PVA write to any of those records — and
/// every other multi-record transaction sharing any of them — blocks.
/// The guards are `'static` (they own an `Arc` of the gate) so the set can
/// be held across `.await` points in the transaction loop.
#[must_use = "the locked epoch ends as soon as the guard is dropped"]
pub struct ManyRecordWriteGuard {
    // Guards drop in vector order; order does not matter for release.
    _guards: Vec<GateGuard>,
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
    /// If the gate is held, this waits in EPICS-band order rather than
    /// FIFO — the calling thread's declared band decides its place in the
    /// queue (module doc, "Wait discipline").
    pub async fn lock_record(&self, record: &str) -> RecordWriteGuard {
        let canonical = self
            .resolve_alias(record)
            .unwrap_or_else(|| record.to_string());
        let gate = self.inner.record_locks.gate_for(&canonical);
        RecordWriteGuard {
            _guard: gate.lock_owned().await,
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
    /// locked exactly once.
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
    pub async fn lock_records<I, S>(&self, records: I) -> ManyRecordWriteGuard
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
            let gate = self.inner.record_locks.gate_for(name);
            guards.push(gate.lock_owned().await);
        }
        ManyRecordWriteGuard { _guards: guards }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// A single-record gate excludes a concurrent same-record locker.
    #[tokio::test]
    async fn lock_record_excludes_same_record() {
        let db = PvDatabase::new();
        let order = Arc::new(AtomicUsize::new(0));

        let g = db.lock_record("ai:1").await;

        let db2 = db.clone();
        let order2 = order.clone();
        let h = tokio::spawn(async move {
            let _g2 = db2.lock_record("ai:1").await;
            // This must observe the first holder having released (1).
            order2.fetch_add(10, Ordering::SeqCst);
        });

        // Give the spawned task time to block on the gate.
        tokio::time::sleep(Duration::from_millis(20)).await;
        // First holder still owns the gate: counter untouched.
        assert_eq!(order.load(Ordering::SeqCst), 0);
        order.fetch_add(1, Ordering::SeqCst);
        drop(g);

        h.await.unwrap();
        assert_eq!(order.load(Ordering::SeqCst), 11);
    }

    /// `lock_records` blocks a plain single-record write to a member.
    #[tokio::test]
    async fn lock_records_excludes_single_member_write() {
        let db = PvDatabase::new();
        let many = db.lock_records(["g:a", "g:b", "g:c"]).await;

        let db2 = db.clone();
        let acquired = Arc::new(AtomicUsize::new(0));
        let acquired2 = acquired.clone();
        let h = tokio::spawn(async move {
            // Plain write to a member must block until `many` drops.
            let _g = db2.lock_record("g:b").await;
            acquired2.store(1, Ordering::SeqCst);
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            acquired.load(Ordering::SeqCst),
            0,
            "single-member write must block while ManyRecordWriteGuard is held"
        );

        drop(many);
        h.await.unwrap();
        assert_eq!(acquired.load(Ordering::SeqCst), 1);
    }

    /// Two overlapping `lock_records` sets acquire in canonical order
    /// and therefore cannot deadlock even with reversed input order.
    #[tokio::test]
    async fn lock_records_overlapping_sets_no_deadlock() {
        let db = PvDatabase::new();

        let db_a = db.clone();
        let ta = tokio::spawn(async move {
            for _ in 0..50 {
                let _g = db_a.lock_records(["x", "y", "z"]).await;
                tokio::task::yield_now().await;
            }
        });
        let db_b = db.clone();
        let tb = tokio::spawn(async move {
            for _ in 0..50 {
                // Reversed input order — sort makes the real
                // acquisition order identical, so no deadlock.
                let _g = db_b.lock_records(["z", "y", "x"]).await;
                tokio::task::yield_now().await;
            }
        });

        tokio::time::timeout(Duration::from_secs(5), async {
            ta.await.unwrap();
            tb.await.unwrap();
        })
        .await
        .expect("overlapping lock_records sets must not deadlock");
    }

    /// An epoch over a record set excludes a second epoch that shares
    /// any record until the first guard drops — and overlapping sets
    /// listed in opposite orders never deadlock (sorted acquisition).
    #[tokio::test]
    async fn overlapping_epochs_are_mutually_exclusive_and_deadlock_free() {
        let db = PvDatabase::new();
        let a = vec!["RECA".to_string(), "RECB".to_string()];
        // Opposite order on purpose — sorted acquisition must still
        // make this safe.
        let b = vec!["RECB".to_string(), "RECC".to_string()];

        let guard_a = db.lock_records(&a).await;

        // A second epoch sharing RECB must not be acquirable while
        // `guard_a` is alive.
        let db2 = db.clone();
        let b2 = b.clone();
        let handle = tokio::spawn(async move { db2.lock_records(&b2).await });

        // Give the spawned task a chance to run; it must still be
        // blocked on RECB.
        tokio::task::yield_now().await;
        assert!(!handle.is_finished(), "epoch B must block on shared RECB");

        drop(guard_a);
        // Now epoch B can complete.
        let _guard_b = handle.await.expect("epoch B task");
    }

    /// Two non-overlapping epochs run concurrently — no false
    /// serialisation.
    #[tokio::test]
    async fn disjoint_epochs_do_not_block_each_other() {
        let db = PvDatabase::new();
        let _g1 = db.lock_records(&["X1".to_string()]).await;
        // Disjoint set: must acquire immediately without blocking.
        let _g2 = db.lock_records(&["X2".to_string()]).await;
    }

    // ------------------------------------------------------------------
    // Wait discipline — the priority-ordered queue (see the module doc).
    //
    // Plain `#[test]`s driven by hand-rolled polling, not `#[tokio::test]`s,
    // for three reasons. The band is a property of a *thread*, so the
    // waiters have to be real banded OS threads rather than tasks
    // multiplexed onto a runtime's worker pool. Hand polling makes the wake
    // sequence directly observable and deterministic instead of
    // sleep-timed. And neither needs a reactor, so these also run under
    // `--features rtems-exec-model` and add no site to this file's
    // RTEMS-EXEC-MODEL-ALLOW census.
    // ------------------------------------------------------------------

    use crate::runtime::task::{ThreadPriority, enter_ioc_thread};

    type Waiter = Pin<Box<dyn Future<Output = RecordWriteGuard> + Send>>;

    /// A waker that appends its id to a shared log when fired, so "who was
    /// woken, in what order" is asserted directly rather than inferred from
    /// which future happens to become ready.
    struct RecordingWaker {
        id: u8,
        log: Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl std::task::Wake for RecordingWaker {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.log.lock().expect("wake log").push(self.id);
        }
    }

    fn recording_waker(id: u8, log: &Arc<std::sync::Mutex<Vec<u8>>>) -> Waker {
        Waker::from(Arc::new(RecordingWaker {
            id,
            log: Arc::clone(log),
        }))
    }

    fn wakes(log: &Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<u8> {
        log.lock().expect("wake log").clone()
    }

    /// Take a free gate with no runtime at all: the uncontended path is
    /// `Ready` on the first poll. Panics instead of hanging when the gate
    /// is unexpectedly held, which is what makes it a wedge detector.
    fn acquire_now(db: &PvDatabase, record: &str) -> RecordWriteGuard {
        let mut fut = Box::pin(db.lock_record(record));
        match fut.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(guard) => guard,
            Poll::Pending => panic!("gate `{record}` was expected to be free"),
        }
    }

    /// Park one waiter on `record` from a thread that declared `band`
    /// through the real `enter_ioc_thread` prologue, then move the still
    /// pending future back to the caller (it is `Send`).
    fn park_waiter(db: &PvDatabase, record: &'static str, band: u8, waker: Waker) -> Waiter {
        let db = db.clone();
        std::thread::spawn(move || {
            let _ = enter_ioc_thread(ThreadPriority::Custom(band));
            let mut fut: Waiter = Box::pin(async move { db.lock_record(record).await });
            let parked = fut
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending();
            assert!(parked, "waiter must park: the gate is held");
            fut
        })
        .join()
        .expect("waiter thread")
    }

    /// Poll `waiter` with its own recording waker — re-registration must
    /// not swap in a waker whose wake the log cannot see — and take the
    /// guard if ownership has been handed over.
    fn poll_waiter(waiter: &mut Waiter, waker: &Waker) -> Option<RecordWriteGuard> {
        match waiter.as_mut().poll(&mut Context::from_waker(waker)) {
            Poll::Ready(guard) => Some(guard),
            Poll::Pending => None,
        }
    }

    /// Release `held`, then drain the queue one grant at a time, returning
    /// the waiter ids in the order the gate actually handed ownership out.
    fn drain_in_grant_order(held: RecordWriteGuard, waiters: Vec<(u8, Waiter, Waker)>) -> Vec<u8> {
        let mut pending = waiters;
        let mut order = Vec::new();
        drop(held);
        while !pending.is_empty() {
            let mut ready = None;
            for (index, (_, waiter, waker)) in pending.iter_mut().enumerate() {
                if let Some(guard) = poll_waiter(waiter, waker) {
                    assert!(
                        ready.is_none(),
                        "the gate handed ownership to two waiters at once"
                    );
                    ready = Some((index, guard));
                }
            }
            let (index, guard) =
                ready.expect("a release must hand ownership to exactly one waiter");
            order.push(pending.remove(index).0);
            // Releasing this one is what wakes the next.
            drop(guard);
        }
        order
    }

    /// Three waiters at EPICS bands 20 / 63 / 70 on a held gate are granted
    /// it in band order 70, 63, 20 — the done-check of
    /// `doc/rtems-priority-locks-design.md` §5 step 2.
    ///
    /// They are enqueued in the *opposite* order on purpose, lowest band
    /// first, so arrival order and band order disagree on every pair. The
    /// `tokio::sync::Mutex` this gate replaced is FIFO and fails this test
    /// on its very first grant.
    #[test]
    fn priority_gate_grants_highest_band_first() {
        let db = PvDatabase::new();
        let held = acquire_now(&db, "ai:band");
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));

        let mut waiters = Vec::new();
        for band in [20u8, 63, 70] {
            let waker = recording_waker(band, &log);
            waiters.push((
                band,
                park_waiter(&db, "ai:band", band, waker.clone()),
                waker,
            ));
        }
        assert!(
            wakes(&log).is_empty(),
            "no waiter may be woken while the gate is still held"
        );

        let order = drain_in_grant_order(held, waiters);
        assert_eq!(
            order,
            vec![70, 63, 20],
            "waiters must be granted the gate in EPICS-band order, highest band first"
        );
        assert_eq!(
            wakes(&log),
            vec![70, 63, 20],
            "each release must wake exactly the next waiter in band order"
        );
    }

    /// Waiters in the *same* band keep arrival order among themselves.
    ///
    /// Without it a priority queue reorders equal-priority threads by
    /// nothing but map iteration, which is the starvation surprise the
    /// ordering is otherwise blamed for.
    #[test]
    fn priority_gate_equal_bands_stay_fifo() {
        let db = PvDatabase::new();
        let held = acquire_now(&db, "ai:fifo");
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));

        let band = ThreadPriority::ScanLow.value();
        let mut waiters = Vec::new();
        // `park_waiter` joins its thread, so arrival order is exactly this
        // loop's order.
        for id in [1u8, 2, 3] {
            let waker = recording_waker(id, &log);
            waiters.push((id, park_waiter(&db, "ai:fifo", band, waker.clone()), waker));
        }

        let order = drain_in_grant_order(held, waiters);
        assert_eq!(
            order,
            vec![1, 2, 3],
            "equal-band waiters must be granted in arrival order"
        );
        assert_eq!(wakes(&log), vec![1, 2, 3]);
    }

    /// A waiter future dropped while queued — and one dropped after it was
    /// handed ownership but before it could be polled — must both leave the
    /// gate usable.
    ///
    /// The second case is the one that wedges a hand-rolled async lock: the
    /// dropped future *is* the owner at that moment, so its drop owes the
    /// same hand-off `GateGuard::drop` would have run.
    #[test]
    fn cancelled_waiter_does_not_wedge_the_gate() {
        let db = PvDatabase::new();
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let held = acquire_now(&db, "ai:cancel");

        // (a) cancelled while merely queued. It is the highest band, so a
        //     surviving queue entry would be granted before the low-band
        //     waiter and the log would say so.
        let queued = park_waiter(&db, "ai:cancel", 70, recording_waker(70, &log));
        let low_waker = recording_waker(20, &log);
        let mut low = park_waiter(&db, "ai:cancel", 20, low_waker.clone());
        drop(queued);

        drop(held);
        assert_eq!(
            wakes(&log),
            vec![20],
            "a cancelled queue entry must not be granted the gate"
        );
        let low_guard =
            poll_waiter(&mut low, &low_waker).expect("low-band waiter must have been granted");

        // (b) cancelled after being handed ownership, before its first
        //     poll. Id 71 rather than 70 only to keep the log unambiguous.
        let owner_waker = recording_waker(71, &log);
        let granted = park_waiter(&db, "ai:cancel", 70, owner_waker);
        let next_waker = recording_waker(63, &log);
        let mut next = park_waiter(&db, "ai:cancel", 63, next_waker.clone());

        drop(low_guard);
        assert_eq!(
            wakes(&log),
            vec![20, 71],
            "the highest-band waiter must be granted next"
        );
        drop(granted);
        assert_eq!(
            wakes(&log),
            vec![20, 71, 63],
            "a cancelled owner must hand the gate to the next waiter"
        );
        let next_guard = poll_waiter(&mut next, &next_waker)
            .expect("the waiter behind the cancelled owner must be granted");
        drop(next_guard);

        // Nothing holds and nothing waits: the gate must be free again.
        // `acquire_now` panics rather than hanging if it is not.
        let _reacquired = acquire_now(&db, "ai:cancel");
    }
}
