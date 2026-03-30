use crate::server::record::ScanType;

use super::PvDatabase;

impl PvDatabase {
    /// Update scan index when a record's SCAN or PHAS field changes.
    pub async fn update_scan_index(
        &self,
        name: &str,
        old_scan: ScanType,
        new_scan: ScanType,
        old_phas: i16,
        new_phas: i16,
    ) {
        let mut index = self.inner.scan_index.write().await;
        if old_scan != ScanType::Passive {
            if let Some(set) = index.get_mut(&old_scan) {
                set.remove(&(old_phas, name.to_string()));
            }
        }
        if new_scan != ScanType::Passive {
            index
                .entry(new_scan)
                .or_default()
                .insert((new_phas, name.to_string()));
        }
    }

    /// Get record names for a given scan type, sorted by PHAS.
    pub async fn records_for_scan(&self, scan_type: ScanType) -> Vec<String> {
        self.inner.scan_index
            .read()
            .await
            .get(&scan_type)
            .map(|s| s.iter().map(|(_, name)| name.clone()).collect())
            .unwrap_or_default()
    }

    /// Get all record names that have PINI=true.
    pub async fn pini_records(&self) -> Vec<String> {
        let records = self.inner.records.read().await;
        let mut result = Vec::new();
        for (name, rec) in records.iter() {
            let instance = rec.read().await;
            if instance.common.pini {
                result.push(name.clone());
            }
        }
        result
    }
}
