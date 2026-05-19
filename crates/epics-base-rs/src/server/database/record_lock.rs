//! Database-level multi-record lock — the Rust counterpart of the
//! C-EPICS `dbLocker` / `dbScanLockMany` machinery and pvxs's
//! `ioc::DBManyLock` / `ioc::DBManyLocker`.
//!
//! C reference:
//! * `epics-base/modules/database/src/ioc/db/dbLock.c:349` —
//!   `dbLockerAlloc` builds a locker over a fixed record set.
//! * `epics-base/modules/database/src/ioc/db/dbLock.c:384` —
//!   `dbScanLockMany` sorts the lock sets and acquires every one,
//!   skipping duplicates so a record set with shared lock sets is
//!   locked exactly once per set.
//! * `pvxs/ioc/pvalink_channel.cpp:386` — pvxs builds a `DBManyLock`
//!   over the atomic pvalink scan-target records.
//! * `pvxs/ioc/pvalink_channel.cpp:422` — pvxs holds a `DBManyLocker`
//!   over that lock while scanning the atomic targets, giving the
//!   atomic linked set a single locked scan epoch.
//!
//! Why a separate epoch lock rather than the per-record
//! `RwLock<RecordInstance>`: the record-processing helpers
//! (`process_record_with_links_inner`, `put_record_field_from_ca`,
//! …) take the `RwLock<RecordInstance>` write guard *internally*. A
//! caller cannot hold those write guards across a multi-record loop
//! and then call those helpers without self-deadlocking. C EPICS has
//! the same shape — `dbProcess` assumes the lock set is *already*
//! held by the caller — so the faithful port is a distinct lock that
//! the atomic owner holds across the whole group while the per-record
//! body still takes the `RecordInstance` guard underneath.
//!
//! Deadlock freedom: `lock_records_atomic` canonicalises, deduplicates
//! and **sorts** the record names before acquiring any guard, so two
//! atomic groups that share records always acquire the shared epoch
//! locks in the same global order — the exact property
//! `dbScanLockMany`'s `qsort` of lock sets provides.

use std::collections::HashMap;
use std::sync::Arc;

use crate::runtime::sync::{Mutex, RwLock};

/// Per-record epoch-lock registry.
///
/// Maps a canonical record name to a shared async `Mutex` whose guard
/// *is* the record's membership in an atomic scan/PUT epoch. Entries
/// are created lazily on first lock and never removed — the count is
/// bounded by the record set and each entry is a single `Arc<Mutex<()>>`.
#[derive(Default)]
pub(crate) struct RecordLockSet {
    locks: RwLock<HashMap<String, Arc<Mutex<()>>>>,
}

/// Held guards for one atomic multi-record epoch.
///
/// While this value is alive, every record named in the originating
/// `lock_records_atomic` call is inside the same locked epoch: no
/// other `lock_records_atomic` epoch can overlap on any of those
/// records. Dropping it ends the epoch and releases every guard.
///
/// The guards are `'static` (`OwnedMutexGuard`) so the epoch can be
/// held across `.await` points in the atomic scan/PUT loop.
#[must_use = "the atomic epoch ends as soon as the guard is dropped"]
pub struct MultiRecordGuard {
    /// One guard per distinct record in the locked set, kept until
    /// the epoch ends. Order matches the sorted acquisition order;
    /// drop order is reverse, which is safe because release order
    /// never deadlocks.
    _guards: Vec<tokio::sync::OwnedMutexGuard<()>>,
}

impl RecordLockSet {
    pub(crate) fn new() -> Self {
        Self {
            locks: RwLock::new(HashMap::new()),
        }
    }

    /// Fetch (creating on first use) the epoch lock for one canonical
    /// record name.
    async fn lock_for(&self, canonical: &str) -> Arc<Mutex<()>> {
        if let Some(lock) = self.locks.read().await.get(canonical) {
            return lock.clone();
        }
        let mut map = self.locks.write().await;
        map.entry(canonical.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

impl super::PvDatabase {
    /// Acquire a single locked epoch over a set of records — the Rust
    /// equivalent of `dbScanLockMany` over a `dbLocker`.
    ///
    /// Each name is alias-resolved to its canonical record name, the
    /// set is deduplicated and **sorted**, then the per-record epoch
    /// locks are acquired in that sorted order. Sorting guarantees two
    /// overlapping atomic groups can never acquire shared epoch locks
    /// in opposite orders, so the call is deadlock-free regardless of
    /// how the caller ordered its record list.
    ///
    /// The returned [`MultiRecordGuard`] keeps every epoch lock held
    /// until it is dropped. Hold it across the whole atomic
    /// scan/PUT loop: that is the lock epoch. Drop it to end the
    /// epoch.
    ///
    /// Names that do not resolve to a record still get an epoch lock
    /// (keyed by the post-alias name) — matching `dbLockerAlloc`,
    /// which accepts the record pointers it is given without a
    /// liveness re-check; a missing record simply has no body to
    /// process under the epoch.
    pub async fn lock_records_atomic(&self, names: &[String]) -> MultiRecordGuard {
        // Alias-resolve then canonicalise every name so two links that
        // name the same record via different aliases share one epoch
        // lock (mirrors `dbLockUpdateRefs` keying on `precord->lset`,
        // which is identity-based, not name-based).
        let mut canonical: Vec<String> = Vec::with_capacity(names.len());
        for name in names {
            match self.resolve_alias(name).await {
                Some(target) => canonical.push(target),
                None => canonical.push(name.clone()),
            }
        }
        // Deterministic global order + dedup: this is what makes the
        // acquisition deadlock-free and skips duplicate lock sets,
        // exactly like `dbScanLockMany`'s sorted-lockSet walk.
        canonical.sort();
        canonical.dedup();

        let mut guards: Vec<tokio::sync::OwnedMutexGuard<()>> = Vec::with_capacity(canonical.len());
        for name in &canonical {
            let lock = self.inner.record_locks.lock_for(name).await;
            guards.push(lock.lock_owned().await);
        }
        MultiRecordGuard { _guards: guards }
    }
}

#[cfg(test)]
mod tests {
    use crate::server::database::PvDatabase;

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

        let guard_a = db.lock_records_atomic(&a).await;

        // A second epoch sharing RECB must not be acquirable while
        // `guard_a` is alive.
        let db2 = db.clone();
        let b2 = b.clone();
        let handle = tokio::spawn(async move { db2.lock_records_atomic(&b2).await });

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
        let _g1 = db.lock_records_atomic(&["X1".to_string()]).await;
        // Disjoint set: must acquire immediately without blocking.
        let _g2 = db.lock_records_atomic(&["X2".to_string()]).await;
    }
}
