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

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, OwnedMutexGuard};

use super::PvDatabase;

/// Registry of per-record advisory write gates.
///
/// Lazily allocates one `Arc<Mutex<()>>` per canonical record name on
/// first use. Entries are never removed: an EPICS database is loaded
/// once at IOC init and the record set is effectively static, so the
/// map size is bounded by the record count and removing entries would
/// reintroduce a TOCTOU race with a concurrent locker.
#[derive(Default)]
pub(crate) struct RecordLockRegistry {
    gates: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl RecordLockRegistry {
    /// Return the advisory gate for `record`, creating it on first use.
    ///
    /// `record` must already be the canonical (alias-resolved) name;
    /// [`PvDatabase::lock_record`] / [`PvDatabase::lock_records`]
    /// resolve aliases before calling this so an alias and its target
    /// always share one gate.
    fn gate_for(&self, record: &str) -> Arc<Mutex<()>> {
        let mut map = self
            .gates
            .lock()
            .expect("record-lock registry mutex poisoned");
        map.entry(record.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

/// RAII guard for a single record's advisory write gate.
///
/// Held for the duration of a plain CA/PVA write. Equivalent to the
/// `dbScanLock`+`dbScanUnlock` pair around one `dbPutField` in C
/// EPICS.
pub struct RecordWriteGuard {
    _guard: OwnedMutexGuard<()>,
}

/// RAII guard for an ordered set of record advisory write gates — the
/// `DBManyLocker` equivalent.
///
/// Acquired by [`PvDatabase::lock_records`] over every member record
/// of a multi-record transaction (QSRV atomic group PUT/GET, pvalink
/// atomic scan-on-update epoch); held across the whole member loop.
/// While alive, every plain CA/PVA write to any of those records — and
/// every other multi-record transaction sharing any of them — blocks.
/// The guards are `'static` (`OwnedMutexGuard`) so the set can be held
/// across `.await` points in the transaction loop.
#[must_use = "the locked epoch ends as soon as the guard is dropped"]
pub struct ManyRecordWriteGuard {
    // Guards drop in vector order; order does not matter for release.
    _guards: Vec<OwnedMutexGuard<()>>,
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
    pub async fn lock_record(&self, record: &str) -> RecordWriteGuard {
        let canonical = self
            .resolve_alias(record)
            .await
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
                    .await
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
}
