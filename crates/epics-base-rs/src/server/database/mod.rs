pub mod db_access;
mod field_io;
mod links;
mod processing;
mod scan_index;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use crate::runtime::sync::RwLock;

use crate::server::pv::ProcessVariable;
use crate::server::record::{Record, RecordInstance, ScanType};
use crate::types::EpicsValue;

/// Parse a PV name into (base_name, field_name).
/// "TEMP.EGU" → ("TEMP", "EGU")
/// "TEMP"     → ("TEMP", "VAL")
pub fn parse_pv_name(name: &str) -> (&str, &str) {
    match name.rsplit_once('.') {
        Some((base, field)) => (base, field),
        None => (name, "VAL"),
    }
}

/// Apply timestamp to a record based on its TSE field.
/// `is_soft` indicates a Soft Channel device type.
fn apply_timestamp(common: &mut super::record::CommonFields, _is_soft: bool) {
    match common.tse {
        0 => {
            // generalTime current time (default behavior).
            // Always update — C EPICS recGblGetTimeStamp sets TIME on every process.
            common.time = crate::runtime::general_time::get_current();
        }
        -1 => {
            // Device-provided time; fallback to generalTime BestTime if not set
            if common.time == std::time::SystemTime::UNIX_EPOCH {
                common.time = crate::runtime::general_time::get_event(-1);
            }
        }
        -2 => {
            // Keep TIME field as-is
        }
        _ => {
            // generalTime event time
            common.time = crate::runtime::general_time::get_event(common.tse as i32);
        }
    }
}

/// Unified entry in the PV database.
pub enum PvEntry {
    Simple(Arc<ProcessVariable>),
    Record(Arc<RwLock<RecordInstance>>),
}

struct PvDatabaseInner {
    simple_pvs: RwLock<HashMap<String, Arc<ProcessVariable>>>,
    records: RwLock<HashMap<String, Arc<RwLock<RecordInstance>>>>,
    /// Scan index: maps scan type → sorted set of (PHAS, record_name).
    scan_index: RwLock<HashMap<ScanType, BTreeSet<(i16, String)>>>,
    /// CP link index: maps source_record → list of target records to process when source changes.
    cp_links: RwLock<HashMap<String, Vec<String>>>,
}

/// Database of all process variables hosted by this server.
#[derive(Clone)]
pub struct PvDatabase {
    inner: Arc<PvDatabaseInner>,
}

/// Select which link indices are active based on SELM and SELN.
/// SELM: 0=All, 1=Specified, 2=Mask
fn select_link_indices(selm: i16, seln: i16, count: usize) -> Vec<usize> {
    match selm {
        0 => (0..count).collect(),
        1 => {
            let i = seln as usize;
            if i < count { vec![i] } else { vec![] }
        }
        2 => (0..count).filter(|i| (seln as u16) & (1 << i) != 0).collect(),
        _ => (0..count).collect(),
    }
}

impl PvDatabase {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(PvDatabaseInner {
                simple_pvs: RwLock::new(HashMap::new()),
                records: RwLock::new(HashMap::new()),
                scan_index: RwLock::new(HashMap::new()),
                cp_links: RwLock::new(HashMap::new()),
            }),
        }
    }

    /// Add a simple PV with an initial value.
    pub async fn add_pv(&self, name: &str, initial: EpicsValue) {
        let pv = Arc::new(ProcessVariable::new(name.to_string(), initial));
        self.inner.simple_pvs
            .write()
            .await
            .insert(name.to_string(), pv);
    }

    /// Add a record (accepts a boxed Record to avoid double-boxing).
    pub async fn add_record(&self, name: &str, record: Box<dyn Record>) {
        let instance = RecordInstance::new_boxed(name.to_string(), record);
        let scan = instance.common.scan;
        let phas = instance.common.phas;
        self.inner.records
            .write()
            .await
            .insert(name.to_string(), Arc::new(RwLock::new(instance)));

        // Register in scan index
        if scan != ScanType::Passive {
            self.inner.scan_index
                .write()
                .await
                .entry(scan)
                .or_default()
                .insert((phas, name.to_string()));
        }
    }

    /// Look up an entry by name. Supports "record.FIELD" syntax.
    pub async fn find_entry(&self, name: &str) -> Option<PvEntry> {
        let (base, _field) = parse_pv_name(name);

        // Check simple PVs first (exact match on full name)
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name) {
            return Some(PvEntry::Simple(pv.clone()));
        }

        // Check records by base name
        if let Some(rec) = self.inner.records.read().await.get(base) {
            return Some(PvEntry::Record(rec.clone()));
        }

        None
    }

    /// Check if a base name exists (for UDP search).
    pub async fn has_name(&self, name: &str) -> bool {
        let (base, _) = parse_pv_name(name);
        if self.inner.simple_pvs.read().await.contains_key(name) {
            return true;
        }
        self.inner.records.read().await.contains_key(base)
    }

    /// Look up a simple PV by name (backward-compatible).
    pub async fn find_pv(&self, name: &str) -> Option<Arc<ProcessVariable>> {
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name) {
            return Some(pv.clone());
        }
        None
    }




    /// Get a record Arc by name.
    pub async fn get_record(&self, name: &str) -> Option<Arc<RwLock<RecordInstance>>> {
        self.inner.records.read().await.get(name).cloned()
    }

    /// Get all record names.
    pub async fn all_record_names(&self) -> Vec<String> {
        self.inner.records.read().await.keys().cloned().collect()
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_select_link_indices() {
        // All
        assert_eq!(select_link_indices(0, 0, 6), vec![0,1,2,3,4,5]);
        // Specified
        assert_eq!(select_link_indices(1, 2, 6), vec![2]);
        assert_eq!(select_link_indices(1, 10, 6), Vec::<usize>::new());
        // Mask: seln=5 = 0b101 -> indices 0 and 2
        assert_eq!(select_link_indices(2, 5, 6), vec![0, 2]);
    }
}
