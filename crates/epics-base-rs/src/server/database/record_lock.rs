//! Database-level multi-record write lock — the `DBManyLock` equivalent.
//!
//! C EPICS / pvxs background
//! -------------------------
//! Every `dbPutField` / `dbProcess` in C EPICS takes the target
//! record's `dbCommon::lock` mutex via `dbScanLock(precord)`
//! (`dbLock.c`). A QSRV *atomic group* operation must apply or read
//! several backing records as one indivisible transaction, so pvxs
//! builds a `DBManyLock` over every group-member record
//! (`groupconfigprocessor.cpp:1165` `initialiseDbLocker`) and takes a
//! `DBManyLocker` across the whole member loop
//! (`groupsource.cpp:444` atomic GET, `groupsource.cpp:569` atomic
//! PUT). `DBManyLock` locks the member records' `lock` mutexes in a
//! deadlock-free canonical order (`dbLock.c` sorts the lock set), and
//! because those are the *same* mutexes a plain `dbPutField` takes, a
//! direct CA/PVA write to a backing member record cannot interleave
//! with the atomic group transaction.
//!
//! Rust port
//! ---------
//! `epics-base-rs` stores each record behind its own
//! `RwLock<RecordInstance>`, but the put/process helpers
//! (`put_record_field_from_ca`, `put_pv`, `process_record`) acquire
//! that `RwLock` *internally* and recurse into link targets, so the
//! QSRV gateway cannot hold N `write_owned()` guards across the
//! member loop without dead-locking the recursive link processing.
//!
//! This module adds the missing layer: a per-record **advisory write
//! gate** registry keyed by canonical record name. It is the direct
//! analogue of `dbCommon::lock`:
//!
//! * A plain CA/PVA write (`put_record_field_from_ca`, `put_pv`,
//!   `process_record`) takes the single record's gate for the
//!   duration of the write.
//! * A QSRV atomic group PUT/GET takes *all* member-record gates
//!   up-front via [`PvDatabase::lock_records`], sorted by canonical
//!   record name so two overlapping group transactions acquire in the
//!   same order and cannot deadlock.
//!
//! Because both paths take the same gate, a direct backing-record
//! write blocks until the atomic group transaction finishes —
//! restoring the `DBManyLock` exclusion that BR-R15 found missing.
//!
//! The gate is *advisory*: it does not replace the per-record
//! `RwLock<RecordInstance>` that still guards the record's data. It
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
/// `DBScanLocker` / `dbScanLock`+`dbScanUnlock` pair around one
/// `dbPutField` in C EPICS.
pub struct RecordWriteGuard {
    _guard: OwnedMutexGuard<()>,
}

/// RAII guard for an ordered set of record advisory write gates — the
/// `DBManyLocker` equivalent.
///
/// Acquired by [`PvDatabase::lock_records`] over every group-member
/// record; held across the whole atomic group PUT/GET member loop.
/// While alive, every plain CA/PVA write to any of those records
/// blocks on [`PvDatabase::lock_record`].
pub struct ManyRecordWriteGuard {
    // Guards drop in vector order; order does not matter for release.
    _guards: Vec<OwnedMutexGuard<()>>,
}

impl PvDatabase {
    /// Acquire the advisory write gate for a single record.
    ///
    /// This is the `dbScanLock(precord)` analogue. The plain CA/PVA
    /// write path holds this for the duration of one record write so
    /// it cannot interleave with an atomic group transaction that
    /// owns the same record's gate via [`Self::lock_records`].
    ///
    /// `record` must be the **canonical** record name (alias already
    /// resolved); the gateway and the DB write helpers resolve
    /// aliases before calling this so an alias and its target share
    /// one gate.
    pub async fn lock_record(&self, record: &str) -> RecordWriteGuard {
        let gate = self.inner.record_locks.gate_for(record);
        RecordWriteGuard {
            _guard: gate.lock_owned().await,
        }
    }

    /// Acquire the advisory write gates for an ordered set of records
    /// — the `DBManyLock` / `DBManyLocker` equivalent.
    ///
    /// Records are sorted and de-duplicated by canonical name before
    /// locking, so any two overlapping multi-record transactions
    /// acquire their shared records in the same order and cannot
    /// deadlock (mirrors pvxs `DBManyLock` sorting the lock set in
    /// `dbLock.c`).
    ///
    /// The returned [`ManyRecordWriteGuard`] must be held for the
    /// whole atomic member loop. While it is alive, a concurrent
    /// plain write to any of those records blocks on
    /// [`Self::lock_record`].
    pub async fn lock_records<I, S>(&self, records: I) -> ManyRecordWriteGuard
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut names: Vec<String> = records
            .into_iter()
            .map(|s| s.as_ref().to_string())
            .collect();
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
}
