use crate::server::record::ScanType;

use super::PvDatabase;

impl PvDatabase {
    /// Update scan index when a record's SCAN or PHAS field changes.
    ///
    /// Round-35 (R35-G1): takes `registration_mutex` so the read of
    /// the records map (to verify the record still exists) and the
    /// scan_index mutation are atomic vs. concurrent `remove_record`.
    ///
    /// Round-48 (R48-G4): the `new_scan` / `new_phas` parameters
    /// the caller passes are advisory only. After acquiring the
    /// mutex we read the LIVE record's current scan/phas and insert
    /// based on those. Pre-fix a put-then-update sequence could
    /// race a remove+re-add of the same name: the caller's
    /// `new_scan` reflected the old (now-removed) record's value;
    /// inserting that under the fresh record's name produced a
    /// stale scan-index entry pointing at a wrong scan rate. The
    /// live-read makes the index strictly reflect the record's
    /// current state at insert time.
    pub async fn update_scan_index(
        &self,
        name: &str,
        old_scan: ScanType,
        _new_scan: ScanType,
        old_phas: i16,
        _new_phas: i16,
    ) {
        let _gate = self.inner.registration_mutex.lock().await;
        // 1) Remove the OLD entry the caller knew about — even if
        // remove_record already swept it (idempotent BTreeSet
        // removal).
        {
            let mut index = self.inner.scan_index.write().await;
            if old_scan != ScanType::Passive {
                if let Some(set) = index.get_mut(&old_scan) {
                    set.remove(&(old_phas, name.to_string()));
                    if set.is_empty() {
                        index.remove(&old_scan);
                    }
                }
            }
        }
        // 2) Look up the LIVE record under the mutex. If concurrent
        // remove+re-add replaced the Arc with a fresh one whose
        // scan differs from the caller's `_new_scan`, we re-insert
        // based on the fresh record's state. The fresh record's
        // own `add_record` call also registered its scan index, so
        // duplicate-insertion of the same (phas, name) pair into
        // the same scan bucket is a no-op (`BTreeSet::insert`
        // returns false on present key).
        let rec_arc = match self.inner.records.read().await.get(name).cloned() {
            Some(r) => r,
            None => return,
        };
        let (cur_scan, cur_phas) = {
            let inst = rec_arc.read().await;
            (inst.common.scan, inst.common.phas)
        };
        if cur_scan != ScanType::Passive {
            self.inner
                .scan_index
                .write()
                .await
                .entry(cur_scan)
                .or_default()
                .insert((cur_phas, name.to_string()));
        }
    }

    /// Get record names for a given scan type, sorted by PHAS.
    pub async fn records_for_scan(&self, scan_type: ScanType) -> Vec<String> {
        self.inner
            .scan_index
            .read()
            .await
            .get(&scan_type)
            .map(|s| s.iter().map(|(_, name)| name.clone()).collect())
            .unwrap_or_default()
    }

    /// Get all record names that have PINI=true.
    ///
    /// Round-35 (R33-G1): snapshot the records map under the outer
    /// read lock, then drop it before fanning out per-record reads.
    /// Pre-fix the outer `records.read()` lock was held across every
    /// `rec.read().await` — under contention with a pending
    /// `add_record` (which now takes the registration_mutex →
    /// records.write()), startup could stall while every PINI
    /// record was inspected serially.
    pub async fn pini_records(&self) -> Vec<String> {
        let snapshot: Vec<_> = {
            let records = self.inner.records.read().await;
            records
                .iter()
                .map(|(n, r)| (n.clone(), r.clone()))
                .collect()
        };
        let mut result = Vec::new();
        for (name, rec) in snapshot {
            let instance = rec.read().await;
            if instance.common.pini {
                result.push(name);
            }
        }
        result
    }

    /// Process all records with SCAN=Event. Equivalent to C EPICS post_event().
    pub async fn post_event(&self) {
        let names = self.records_for_scan(ScanType::Event).await;
        for name in &names {
            let mut visited = std::collections::HashSet::new();
            let _ = self.process_record_with_links(name, &mut visited, 0).await;
        }
    }
}
