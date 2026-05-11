use crate::server::record::ScanType;

use super::PvDatabase;

impl PvDatabase {
    /// Update scan index when a record's SCAN or PHAS field changes.
    ///
    /// Round-35 (R35-G1): takes `registration_mutex` so the read of
    /// the records map (to verify the record still exists) and the
    /// scan_index mutation are atomic vs. concurrent `remove_record`.
    /// Pre-fix a CA put that flipped SCAN concurrent with a
    /// `dbDeleteRecord` could land an `(phas, name)` entry in the
    /// scan_index AFTER `remove_record` swept it, leaving a phantom
    /// pointing at a non-existent record — the scan scheduler then
    /// looked it up by name and silently skipped each tick.
    pub async fn update_scan_index(
        &self,
        name: &str,
        old_scan: ScanType,
        new_scan: ScanType,
        old_phas: i16,
        new_phas: i16,
    ) {
        let _gate = self.inner.registration_mutex.lock().await;
        // If the record was concurrently removed before we got here,
        // the new index entry would dangle — skip the insert. The
        // remove side has already cleaned old_scan entries.
        let record_present = self.inner.records.read().await.contains_key(name);
        let mut index = self.inner.scan_index.write().await;
        if old_scan != ScanType::Passive {
            if let Some(set) = index.get_mut(&old_scan) {
                set.remove(&(old_phas, name.to_string()));
                if set.is_empty() {
                    index.remove(&old_scan);
                }
            }
        }
        if record_present && new_scan != ScanType::Passive {
            index
                .entry(new_scan)
                .or_default()
                .insert((new_phas, name.to_string()));
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
