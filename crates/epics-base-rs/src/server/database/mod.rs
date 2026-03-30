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
    use std::collections::HashSet;
    use crate::error::CaError;
    use crate::server::records::ai::AiRecord;
    use crate::server::records::ao::AoRecord;

    #[tokio::test]
    async fn test_write_notify_follows_flnk() {
        let db = PvDatabase::new();
        db.add_record("REC_A", Box::new(AoRecord::new(0.0))).await;
        db.add_record("REC_B", Box::new(AoRecord::new(0.0))).await;

        // Set FLNK from A to B
        if let Some(rec) = db.get_record("REC_A").await {
            let mut inst = rec.write().await;
            inst.put_common_field("FLNK", EpicsValue::String("REC_B".into())).unwrap();
        }

        // Process A with links — B should also be processed
        let mut visited = HashSet::new();
        db.process_record_with_links("REC_A", &mut visited, 0).await.unwrap();
        assert!(visited.contains("REC_A"));
        assert!(visited.contains("REC_B"));
    }

    #[tokio::test]
    async fn test_inp_link_processing() {
        let db = PvDatabase::new();
        db.add_record("SOURCE", Box::new(AoRecord::new(42.0))).await;
        db.add_record("DEST", Box::new(AiRecord::new(0.0))).await;

        // Set INP on DEST to read from SOURCE
        if let Some(rec) = db.get_record("DEST").await {
            let mut inst = rec.write().await;
            inst.put_common_field("INP", EpicsValue::String("SOURCE".into())).unwrap();
        }

        let mut visited = HashSet::new();
        db.process_record_with_links("DEST", &mut visited, 0).await.unwrap();

        // DEST should have read SOURCE's value
        let val = db.get_pv("DEST").await.unwrap();
        match val {
            EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
            other => panic!("expected Double(42.0), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_cycle_detection() {
        let db = PvDatabase::new();
        db.add_record("CYCLE_A", Box::new(AoRecord::new(0.0))).await;
        db.add_record("CYCLE_B", Box::new(AoRecord::new(0.0))).await;

        // A → B → A (cycle)
        if let Some(rec) = db.get_record("CYCLE_A").await {
            let mut inst = rec.write().await;
            inst.put_common_field("FLNK", EpicsValue::String("CYCLE_B".into())).unwrap();
        }
        if let Some(rec) = db.get_record("CYCLE_B").await {
            let mut inst = rec.write().await;
            inst.put_common_field("FLNK", EpicsValue::String("CYCLE_A".into())).unwrap();
        }

        // Should not infinite loop
        let mut visited = HashSet::new();
        db.process_record_with_links("CYCLE_A", &mut visited, 0).await.unwrap();
        assert!(visited.contains("CYCLE_A"));
        assert!(visited.contains("CYCLE_B"));
        assert_eq!(visited.len(), 2);
    }

    #[tokio::test]
    async fn test_ao_drvh_drvl_clamp() {
        use crate::server::record::Record;

        let mut rec = AoRecord::new(0.0);
        rec.drvh = 100.0;
        rec.drvl = -50.0;
        rec.val = 200.0;
        rec.process().unwrap();
        assert!((rec.val - 100.0).abs() < 1e-10);

        rec.val = -100.0;
        rec.process().unwrap();
        assert!((rec.val - (-50.0)).abs() < 1e-10);
    }

    #[tokio::test]
    async fn test_ao_oroc_rate_limit() {
        use crate::server::record::Record;

        let mut rec = AoRecord::new(0.0);
        rec.oroc = 5.0;
        rec.drvh = 0.0;
        rec.drvl = 0.0; // no clamping

        // First process — no rate limit (init=false)
        rec.val = 100.0;
        rec.process().unwrap();
        assert!((rec.val - 100.0).abs() < 1e-10);

        // Second process — rate limited
        rec.val = 200.0;
        rec.process().unwrap();
        // delta = 200 - 100 = 100 > oroc=5, so val = 100 + 5 = 105
        assert!((rec.val - 105.0).abs() < 1e-10);
    }

    #[tokio::test]
    async fn test_ao_omsl_dol() {
        let db = PvDatabase::new();
        db.add_record("SOURCE", Box::new(AoRecord::new(42.0))).await;

        let mut ao = AoRecord::new(0.0);
        ao.omsl = 1; // CLOSED_LOOP
        ao.dol = "SOURCE".to_string();
        db.add_record("OUTPUT", Box::new(ao)).await;

        let mut visited = HashSet::new();
        db.process_record_with_links("OUTPUT", &mut visited, 0).await.unwrap();

        let val = db.get_pv("OUTPUT").await.unwrap();
        match val {
            EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
            other => panic!("expected Double(42.0), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_ao_oif_incremental() {
        let db = PvDatabase::new();
        db.add_record("DELTA", Box::new(AoRecord::new(10.0))).await;

        let mut ao = AoRecord::new(100.0);
        ao.omsl = 1; // CLOSED_LOOP
        ao.oif = 1;  // Incremental
        ao.dol = "DELTA".to_string();
        db.add_record("OUTPUT", Box::new(ao)).await;

        let mut visited = HashSet::new();
        db.process_record_with_links("OUTPUT", &mut visited, 0).await.unwrap();

        // VAL = 100 + 10 = 110
        let val = db.get_pv("OUTPUT").await.unwrap();
        match val {
            EpicsValue::Double(v) => assert!((v - 110.0).abs() < 1e-10),
            other => panic!("expected Double(110.0), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_ao_ivoa_dont_drive() {
        let db = PvDatabase::new();
        db.add_record("TARGET", Box::new(AoRecord::new(0.0))).await;

        let mut ao = AoRecord::new(999.0);
        ao.ivoa = 1; // Don't drive outputs when INVALID
        db.add_record("OUTPUT", Box::new(ao)).await;

        // Set OUT link + HIHI alarm to trigger INVALID severity
        if let Some(rec) = db.get_record("OUTPUT").await {
            let mut inst = rec.write().await;
            inst.put_common_field("OUT", EpicsValue::String("TARGET".into())).unwrap();
            inst.put_common_field("HIHI", EpicsValue::Double(100.0)).unwrap();
            inst.put_common_field("HHSV", EpicsValue::Short(
                crate::server::record::AlarmSeverity::Invalid as i16)).unwrap();
        }

        let mut visited = HashSet::new();
        db.process_record_with_links("OUTPUT", &mut visited, 0).await.unwrap();

        // OUTPUT's VAL=999 > HIHI=100 → INVALID alarm → IVOA=1 → don't drive
        // TARGET should still be 0
        let val = db.get_pv("TARGET").await.unwrap();
        match val {
            EpicsValue::Double(v) => assert!((v - 0.0).abs() < 1e-10),
            other => panic!("expected Double(0.0), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_sim_mode_input() {
        let db = PvDatabase::new();
        // SIM_SW controls simulation mode (1=YES)
        db.add_record("SIM_SW", Box::new(AoRecord::new(1.0))).await;
        // SIM_VAL provides simulated value
        db.add_record("SIM_VAL", Box::new(AoRecord::new(99.0))).await;

        // AI record with SIML and SIOL
        let mut ai = AiRecord::new(0.0);
        ai.siml = "SIM_SW".to_string();
        ai.siol = "SIM_VAL".to_string();
        ai.sims = 1; // MINOR severity during simulation
        db.add_record("SIM_AI", Box::new(ai)).await;

        let mut visited = HashSet::new();
        db.process_record_with_links("SIM_AI", &mut visited, 0).await.unwrap();

        // Should have read from SIM_VAL directly (no conversion)
        let val = db.get_pv("SIM_AI").await.unwrap();
        match val {
            EpicsValue::Double(v) => assert!((v - 99.0).abs() < 1e-10),
            other => panic!("expected Double(99.0), got {:?}", other),
        }

        // Should have MINOR alarm from SIMS
        let sevr = db.get_pv("SIM_AI.SEVR").await.unwrap();
        assert!(matches!(sevr, EpicsValue::Short(1))); // MINOR
    }

    #[tokio::test]
    async fn test_sim_mode_toggle() {
        let db = PvDatabase::new();
        db.add_record("SIM_SW", Box::new(AoRecord::new(0.0))).await; // OFF
        db.add_record("SIM_VAL", Box::new(AoRecord::new(42.0))).await;
        db.add_record("REAL_SRC", Box::new(AoRecord::new(10.0))).await;

        let mut ai = AiRecord::new(0.0);
        ai.siml = "SIM_SW".to_string();
        ai.siol = "SIM_VAL".to_string();
        db.add_record("TEST_AI", Box::new(ai)).await;

        // Set INP to REAL_SRC
        if let Some(rec) = db.get_record("TEST_AI").await {
            let mut inst = rec.write().await;
            inst.put_common_field("INP", EpicsValue::String("REAL_SRC".into())).unwrap();
        }

        // SIM_SW=0 → normal processing, reads from REAL_SRC
        let mut visited = HashSet::new();
        db.process_record_with_links("TEST_AI", &mut visited, 0).await.unwrap();
        let val = db.get_pv("TEST_AI").await.unwrap();
        match val {
            EpicsValue::Double(v) => assert!((v - 10.0).abs() < 1e-10),
            other => panic!("expected Double(10.0), got {:?}", other),
        }

        // Toggle SIM_SW=1 → simulation, reads from SIM_VAL
        db.put_pv("SIM_SW", EpicsValue::Double(1.0)).await.unwrap();
        let mut visited = HashSet::new();
        db.process_record_with_links("TEST_AI", &mut visited, 0).await.unwrap();
        let val = db.get_pv("TEST_AI").await.unwrap();
        match val {
            EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
            other => panic!("expected Double(42.0), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_sim_mode_output() {
        let db = PvDatabase::new();
        db.add_record("SIM_SW", Box::new(AoRecord::new(1.0))).await;
        db.add_record("SIM_OUT", Box::new(AoRecord::new(0.0))).await;

        let mut ao = AoRecord::new(77.0);
        ao.siml = "SIM_SW".to_string();
        ao.siol = "SIM_OUT".to_string();
        db.add_record("TEST_AO", Box::new(ao)).await;

        let mut visited = HashSet::new();
        db.process_record_with_links("TEST_AO", &mut visited, 0).await.unwrap();

        // Output sim: VAL should be written to SIM_OUT
        let val = db.get_pv("SIM_OUT").await.unwrap();
        match val {
            EpicsValue::Double(v) => assert!((v - 77.0).abs() < 1e-10),
            other => panic!("expected Double(77.0), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_sdis_disable_skips_process() {
        let db = PvDatabase::new();
        db.add_record("DISABLE_SW", Box::new(AoRecord::new(1.0))).await;
        db.add_record("TARGET", Box::new(AoRecord::new(0.0))).await;

        // Set SDIS link and DISV=1 (default)
        if let Some(rec) = db.get_record("TARGET").await {
            let mut inst = rec.write().await;
            inst.put_common_field("SDIS", EpicsValue::String("DISABLE_SW".into())).unwrap();
            inst.put_common_field("DISS", EpicsValue::Short(1)).unwrap(); // MINOR
        }

        // Process TARGET — DISABLE_SW.VAL=1 matches DISV=1 → disabled
        let mut visited = HashSet::new();
        db.process_record_with_links("TARGET", &mut visited, 0).await.unwrap();

        let rec = db.get_record("TARGET").await.unwrap();
        let inst = rec.read().await;
        assert_eq!(inst.common.stat, 14); // DISABLE_ALARM
        assert_eq!(inst.common.sevr, crate::server::record::AlarmSeverity::Minor);

        // Now set DISABLE_SW to 0 → not disabled
        drop(inst);
        db.put_pv("DISABLE_SW", EpicsValue::Double(0.0)).await.unwrap();
        let mut visited = HashSet::new();
        db.process_record_with_links("TARGET", &mut visited, 0).await.unwrap();

        let rec = db.get_record("TARGET").await.unwrap();
        let inst = rec.read().await;
        assert_ne!(inst.common.stat, 14); // Not disabled
    }

    #[tokio::test]
    async fn test_phas_scan_order() {
        use crate::server::record::CommonFieldPutResult;
        let db = PvDatabase::new();

        // Create records with different PHAS values
        db.add_record("REC_C", Box::new(AoRecord::new(0.0))).await;
        db.add_record("REC_A", Box::new(AoRecord::new(0.0))).await;
        db.add_record("REC_B", Box::new(AoRecord::new(0.0))).await;

        // Set PHAS first, then SCAN — scan index now correctly captures PHAS
        for (name, phas) in &[("REC_C", 2i16), ("REC_A", 0), ("REC_B", 1)] {
            if let Some(rec) = db.get_record(name).await {
                let mut inst = rec.write().await;
                inst.put_common_field("PHAS", EpicsValue::Short(*phas)).unwrap();
                let result = inst.put_common_field("SCAN", EpicsValue::String("1 second".into())).unwrap();
                if let CommonFieldPutResult::ScanChanged { old_scan, new_scan, phas: p } = result {
                    drop(inst);
                    db.update_scan_index(name, old_scan, new_scan, p, p).await;
                }
            }
        }

        let names = db.records_for_scan(crate::server::record::ScanType::Sec1).await;
        assert_eq!(names, vec!["REC_A", "REC_B", "REC_C"]);
    }

    #[tokio::test]
    async fn test_depth_limit() {
        let db = PvDatabase::new();
        // Create a chain of 20 records
        for i in 0..20 {
            db.add_record(&format!("CHAIN_{i}"), Box::new(AoRecord::new(0.0))).await;
        }
        for i in 0..19 {
            if let Some(rec) = db.get_record(&format!("CHAIN_{i}")).await {
                let mut inst = rec.write().await;
                inst.put_common_field("FLNK", EpicsValue::String(format!("CHAIN_{}", i + 1))).unwrap();
            }
        }

        let mut visited = HashSet::new();
        db.process_record_with_links("CHAIN_0", &mut visited, 0).await.unwrap();
        // Depth limit is 16, so not all 20 should be visited
        assert!(visited.len() <= 17); // depth 0..16 = 17 records max
        assert!(visited.contains("CHAIN_0"));
    }

    #[tokio::test]
    async fn test_disp_blocks_ca_put() {
        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(0.0))).await;

        // Set DISP=1
        if let Some(rec) = db.get_record("REC").await {
            let mut inst = rec.write().await;
            inst.put_common_field("DISP", EpicsValue::Char(1)).unwrap();
        }

        let result = db.put_record_field_from_ca("REC", "VAL", EpicsValue::Double(42.0)).await;
        assert!(matches!(result, Err(CaError::PutDisabled(_))));
    }

    #[tokio::test]
    async fn test_disp_allows_disp_write() {
        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(0.0))).await;

        // Set DISP=1
        if let Some(rec) = db.get_record("REC").await {
            let mut inst = rec.write().await;
            inst.put_common_field("DISP", EpicsValue::Char(1)).unwrap();
        }

        // Should still be able to write DISP itself
        let result = db.put_record_field_from_ca("REC", "DISP", EpicsValue::Char(0)).await;
        assert!(result.is_ok());

        // DISP should now be false
        let rec = db.get_record("REC").await.unwrap();
        let inst = rec.read().await;
        assert!(!inst.common.disp);
    }

    #[tokio::test]
    async fn test_disp_bypassed_by_internal_put() {
        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(0.0))).await;

        // Set DISP=1
        if let Some(rec) = db.get_record("REC").await {
            let mut inst = rec.write().await;
            inst.put_common_field("DISP", EpicsValue::Char(1)).unwrap();
        }

        // Internal put_pv should bypass DISP
        let result = db.put_pv("REC", EpicsValue::Double(42.0)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_proc_triggers_processing() {
        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(0.0))).await;

        // Set a value first
        db.put_pv("REC", EpicsValue::Double(42.0)).await.unwrap();

        // PROC put should trigger processing
        let result = db.put_record_field_from_ca("REC", "PROC", EpicsValue::Char(1)).await;
        assert!(result.is_ok());

        // Verify UDF is false after processing
        let rec = db.get_record("REC").await.unwrap();
        let inst = rec.read().await;
        assert!(!inst.common.udf);
    }

    #[tokio::test]
    async fn test_proc_works_any_scan() {
        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(0.0))).await;

        // Set SCAN=1 second (non-Passive)
        if let Some(rec) = db.get_record("REC").await {
            let mut inst = rec.write().await;
            inst.put_common_field("SCAN", EpicsValue::String("1 second".into())).unwrap();
        }

        // PROC should still trigger processing regardless of SCAN
        let result = db.put_record_field_from_ca("REC", "PROC", EpicsValue::Char(1)).await;
        assert!(result.is_ok());

        let rec = db.get_record("REC").await.unwrap();
        let inst = rec.read().await;
        assert!(!inst.common.udf);
    }

    #[tokio::test]
    async fn test_proc_bypasses_disp() {
        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(0.0))).await;

        // Set DISP=1
        if let Some(rec) = db.get_record("REC").await {
            let mut inst = rec.write().await;
            inst.put_common_field("DISP", EpicsValue::Char(1)).unwrap();
        }

        // PROC put should work even with DISP=1
        let result = db.put_record_field_from_ca("REC", "PROC", EpicsValue::Char(1)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_proc_while_pact() {
        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(0.0))).await;

        // PROC put should succeed even when record is already processing
        // (process_record_with_links handles its own re-entrance via visited set)
        let result = db.put_record_field_from_ca("REC", "PROC", EpicsValue::Char(1)).await;
        assert!(result.is_ok());

        // Verify it actually processed
        let rec = db.get_record("REC").await.unwrap();
        let inst = rec.read().await;
        assert!(!inst.common.udf);
    }

    #[tokio::test]
    async fn test_lcnt_ca_write_rejected() {
        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(0.0))).await;

        let result = db.put_record_field_from_ca("REC", "LCNT", EpicsValue::Short(0)).await;
        assert!(matches!(result, Err(CaError::ReadOnlyField(_))));
    }

    #[tokio::test]
    async fn test_ca_put_scan_index_update() {
        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(0.0))).await;

        // CA put to change SCAN from Passive to 1 second
        db.put_record_field_from_ca("REC", "SCAN", EpicsValue::String("1 second".into())).await.unwrap();

        let names = db.records_for_scan(crate::server::record::ScanType::Sec1).await;
        assert!(names.contains(&"REC".to_string()));
    }

    // --- Mock DeviceSupport for write/read counting ---

    use std::sync::atomic::{AtomicU32, Ordering};

    struct MockDeviceSupport {
        read_count: Arc<AtomicU32>,
        write_count: Arc<AtomicU32>,
        dtyp_name: String,
    }

    impl MockDeviceSupport {
        fn new(dtyp: &str, read_count: Arc<AtomicU32>, write_count: Arc<AtomicU32>) -> Self {
            Self {
                read_count,
                write_count,
                dtyp_name: dtyp.to_string(),
            }
        }
    }

    impl super::super::device_support::DeviceSupport for MockDeviceSupport {
        fn read(&mut self, _record: &mut dyn crate::server::record::Record) -> crate::error::CaResult<()> {
            self.read_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn write(&mut self, _record: &mut dyn crate::server::record::Record) -> crate::error::CaResult<()> {
            self.write_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn dtyp(&self) -> &str {
            &self.dtyp_name
        }
    }

    #[tokio::test]
    async fn test_ca_put_no_double_device_write() {
        // Passive ao with mock device: CA put should trigger device.write() exactly once
        // (via process_record_with_links, not from put_record_field_from_ca directly)
        let db = PvDatabase::new();
        db.add_record("AO_REC", Box::new(AoRecord::new(0.0))).await;

        let read_count = Arc::new(AtomicU32::new(0));
        let write_count = Arc::new(AtomicU32::new(0));
        let mock = MockDeviceSupport::new("MockDev", read_count.clone(), write_count.clone());

        if let Some(rec) = db.get_record("AO_REC").await {
            let mut inst = rec.write().await;
            inst.common.dtyp = "MockDev".to_string();
            inst.device = Some(Box::new(mock));
        }

        // CA put to Passive ao → field put + process → device.write() once
        db.put_record_field_from_ca("AO_REC", "VAL", EpicsValue::Double(42.0)).await.unwrap();

        assert_eq!(write_count.load(Ordering::SeqCst), 1, "device.write() should be called exactly once");
    }

    #[tokio::test]
    async fn test_input_record_no_device_write() {
        // Non-soft ai with mock device: process should call device.read() but NOT device.write()
        let db = PvDatabase::new();
        db.add_record("AI_REC", Box::new(AiRecord::new(0.0))).await;

        let read_count = Arc::new(AtomicU32::new(0));
        let write_count = Arc::new(AtomicU32::new(0));
        let mock = MockDeviceSupport::new("MockDev", read_count.clone(), write_count.clone());

        if let Some(rec) = db.get_record("AI_REC").await {
            let mut inst = rec.write().await;
            inst.common.dtyp = "MockDev".to_string();
            inst.device = Some(Box::new(mock));
        }

        let mut visited = HashSet::new();
        db.process_record_with_links("AI_REC", &mut visited, 0).await.unwrap();

        assert_eq!(read_count.load(Ordering::SeqCst), 1, "device.read() should be called");
        assert_eq!(write_count.load(Ordering::SeqCst), 0, "device.write() should NOT be called for input record");
    }

    #[tokio::test]
    async fn test_non_passive_output_ca_put_triggers_write() {
        // Non-Passive ao with mock device: CA put SHOULD trigger process → device.write()
        // (C EPICS processes on any CA put to VAL, regardless of SCAN type)
        let db = PvDatabase::new();
        db.add_record("AO_NP", Box::new(AoRecord::new(0.0))).await;

        let read_count = Arc::new(AtomicU32::new(0));
        let write_count = Arc::new(AtomicU32::new(0));
        let mock = MockDeviceSupport::new("MockDev", read_count.clone(), write_count.clone());

        if let Some(rec) = db.get_record("AO_NP").await {
            let mut inst = rec.write().await;
            inst.common.dtyp = "MockDev".to_string();
            inst.common.scan = crate::server::record::ScanType::Sec1;
            inst.device = Some(Box::new(mock));
        }

        db.put_record_field_from_ca("AO_NP", "VAL", EpicsValue::Double(42.0)).await.unwrap();

        assert_eq!(write_count.load(Ordering::SeqCst), 1, "device.write() should be called on CA put to output record");
    }

    #[tokio::test]
    async fn test_proc_triggers_device_write() {
        // Passive ao with mock device: PROC put should trigger process → device.write() once
        let db = PvDatabase::new();
        db.add_record("AO_PROC", Box::new(AoRecord::new(0.0))).await;

        let read_count = Arc::new(AtomicU32::new(0));
        let write_count = Arc::new(AtomicU32::new(0));
        let mock = MockDeviceSupport::new("MockDev", read_count.clone(), write_count.clone());

        if let Some(rec) = db.get_record("AO_PROC").await {
            let mut inst = rec.write().await;
            inst.common.dtyp = "MockDev".to_string();
            inst.device = Some(Box::new(mock));
        }

        db.put_record_field_from_ca("AO_PROC", "PROC", EpicsValue::Char(1)).await.unwrap();

        assert_eq!(write_count.load(Ordering::SeqCst), 1, "device.write() should be called once via PROC");
    }

    // --- PR 2: Scan Index Fix tests ---

    #[tokio::test]
    async fn test_phas_change_updates_scan_index() {
        use crate::server::record::CommonFieldPutResult;
        let db = PvDatabase::new();
        db.add_record("REC_A", Box::new(AoRecord::new(0.0))).await;
        db.add_record("REC_B", Box::new(AoRecord::new(0.0))).await;

        // Set both to SCAN=1s, different PHAS
        for (name, phas) in &[("REC_A", 10i16), ("REC_B", 5)] {
            if let Some(rec) = db.get_record(name).await {
                let mut inst = rec.write().await;
                inst.put_common_field("PHAS", EpicsValue::Short(*phas)).unwrap();
                let result = inst.put_common_field("SCAN", EpicsValue::String("1 second".into())).unwrap();
                if let CommonFieldPutResult::ScanChanged { old_scan, new_scan, phas: p } = result {
                    drop(inst);
                    db.update_scan_index(name, old_scan, new_scan, p, p).await;
                }
            }
        }

        // REC_B(phas=5) before REC_A(phas=10)
        let names = db.records_for_scan(crate::server::record::ScanType::Sec1).await;
        assert_eq!(names, vec!["REC_B", "REC_A"]);

        // Now change REC_A's PHAS from 10 to 0
        if let Some(rec) = db.get_record("REC_A").await {
            let mut inst = rec.write().await;
            let result = inst.put_common_field("PHAS", EpicsValue::Short(0)).unwrap();
            if let CommonFieldPutResult::PhasChanged { scan, old_phas, new_phas } = result {
                drop(inst);
                db.update_scan_index("REC_A", scan, scan, old_phas, new_phas).await;
            }
        }

        // Now REC_A(phas=0) before REC_B(phas=5)
        let names = db.records_for_scan(crate::server::record::ScanType::Sec1).await;
        assert_eq!(names, vec!["REC_A", "REC_B"]);
    }

    #[tokio::test]
    async fn test_scan_change_preserves_phas() {
        use crate::server::record::CommonFieldPutResult;
        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(0.0))).await;

        // Set PHAS=3, then change SCAN Passive→Sec1
        if let Some(rec) = db.get_record("REC").await {
            let mut inst = rec.write().await;
            inst.put_common_field("PHAS", EpicsValue::Short(3)).unwrap();
            let result = inst.put_common_field("SCAN", EpicsValue::String("1 second".into())).unwrap();
            match result {
                CommonFieldPutResult::ScanChanged { phas, .. } => assert_eq!(phas, 3),
                other => panic!("expected ScanChanged, got {:?}", other),
            }
        }
    }

    #[tokio::test]
    async fn test_phas_change_passive_no_index() {
        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(0.0))).await;

        // PHAS change on Passive record should not touch index
        if let Some(rec) = db.get_record("REC").await {
            let mut inst = rec.write().await;
            let result = inst.put_common_field("PHAS", EpicsValue::Short(5)).unwrap();
            assert_eq!(result, crate::server::record::CommonFieldPutResult::NoChange);
        }
    }

    // --- PR 3: Async Processing Contract tests ---

    /// Record that returns AsyncPending from process()
    struct AsyncRecord { val: f64 }
    impl crate::server::record::Record for AsyncRecord {
        fn record_type(&self) -> &'static str { "async_test" }
        fn process(&mut self) -> crate::error::CaResult<crate::server::record::RecordProcessResult> {
            Ok(crate::server::record::RecordProcessResult::AsyncPending)
        }
        fn get_field(&self, name: &str) -> Option<EpicsValue> {
            match name { "VAL" => Some(EpicsValue::Double(self.val)), _ => None }
        }
        fn put_field(&mut self, name: &str, value: EpicsValue) -> crate::error::CaResult<()> {
            match name {
                "VAL" => {
                    if let EpicsValue::Double(v) = value { self.val = v; Ok(()) }
                    else { Err(CaError::InvalidValue("bad".into())) }
                }
                _ => Err(CaError::FieldNotFound(name.into())),
            }
        }
        fn field_list(&self) -> &'static [crate::server::record::FieldDesc] { &[] }
    }

    #[tokio::test]
    async fn test_async_pending_skips_post_process() {
        let db = PvDatabase::new();
        db.add_record("ASYNC", Box::new(AsyncRecord { val: 0.0 })).await;
        db.add_record("FLNK_TARGET", Box::new(AoRecord::new(0.0))).await;

        // Set FLNK from ASYNC to FLNK_TARGET
        if let Some(rec) = db.get_record("ASYNC").await {
            let mut inst = rec.write().await;
            inst.put_common_field("FLNK", EpicsValue::String("FLNK_TARGET".into())).unwrap();
        }

        // Process — should return AsyncPending and skip post-process
        let mut visited = HashSet::new();
        db.process_record_with_links("ASYNC", &mut visited, 0).await.unwrap();

        // FLNK should NOT have been followed (only ASYNC in visited)
        assert!(visited.contains("ASYNC"));
        assert!(!visited.contains("FLNK_TARGET"));

        // UDF should NOT be cleared
        let rec = db.get_record("ASYNC").await.unwrap();
        let inst = rec.read().await;
        assert!(inst.common.udf);
    }

    #[tokio::test]
    async fn test_complete_async_record() {
        let db = PvDatabase::new();
        db.add_record("ASYNC", Box::new(AsyncRecord { val: 42.0 })).await;
        db.add_record("FLNK_TARGET", Box::new(AoRecord::new(0.0))).await;

        if let Some(rec) = db.get_record("ASYNC").await {
            let mut inst = rec.write().await;
            inst.put_common_field("FLNK", EpicsValue::String("FLNK_TARGET".into())).unwrap();
        }

        // Process — AsyncPending
        let mut visited = HashSet::new();
        db.process_record_with_links("ASYNC", &mut visited, 0).await.unwrap();
        assert!(!visited.contains("FLNK_TARGET"));

        // Complete — should now run post-process including FLNK
        db.complete_async_record("ASYNC").await.unwrap();

        // UDF should now be cleared
        let rec = db.get_record("ASYNC").await.unwrap();
        let inst = rec.read().await;
        assert!(!inst.common.udf);
    }

    // --- PR 4: Monitor Mask tests ---

    #[tokio::test]
    async fn test_notify_field_respects_mask() {
        use crate::server::recgbl::EventMask;

        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(42.0))).await;

        let rec = db.get_record("REC").await.unwrap();
        let (mut value_rx, mut alarm_rx) = {
            let mut inst = rec.write().await;
            // VALUE-only subscriber
            let value_rx = inst.add_subscriber("VAL", 1, crate::types::DbFieldType::Double, EventMask::VALUE.bits());
            // ALARM-only subscriber
            let alarm_rx = inst.add_subscriber("VAL", 2, crate::types::DbFieldType::Double, EventMask::ALARM.bits());
            (value_rx, alarm_rx)
        };

        // Notify with VALUE mask
        {
            let inst = rec.read().await;
            inst.notify_field("VAL", EventMask::VALUE);
        }

        // VALUE subscriber should get it
        assert!(value_rx.try_recv().is_ok());
        // ALARM subscriber should NOT
        assert!(alarm_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_sdis_disable_notifies_alarm() {
        use crate::server::recgbl::EventMask;

        let db = PvDatabase::new();
        db.add_record("DISABLE_SW", Box::new(AoRecord::new(1.0))).await;
        db.add_record("TARGET", Box::new(AoRecord::new(0.0))).await;

        // Set SDIS link and DISS
        if let Some(rec) = db.get_record("TARGET").await {
            let mut inst = rec.write().await;
            inst.put_common_field("SDIS", EpicsValue::String("DISABLE_SW".into())).unwrap();
            inst.put_common_field("DISS", EpicsValue::Short(1)).unwrap(); // MINOR
        }

        // Add ALARM subscriber
        let mut alarm_rx = {
            let rec = db.get_record("TARGET").await.unwrap();
            let mut inst = rec.write().await;
            inst.add_subscriber("SEVR", 1, crate::types::DbFieldType::Short, EventMask::ALARM.bits())
        };

        // Process — disabled path
        let mut visited = HashSet::new();
        db.process_record_with_links("TARGET", &mut visited, 0).await.unwrap();

        // ALARM subscriber should be notified
        assert!(alarm_rx.try_recv().is_ok());
    }

    // --- PR 5: UDF in database context ---

    #[tokio::test]
    async fn test_udf_cleared_by_process_with_links() {
        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(0.0))).await;

        // UDF starts true
        let rec = db.get_record("REC").await.unwrap();
        assert!(rec.read().await.common.udf);

        let mut visited = HashSet::new();
        db.process_record_with_links("REC", &mut visited, 0).await.unwrap();

        assert!(!rec.read().await.common.udf);
    }

    #[tokio::test]
    async fn test_udf_not_cleared_by_clears_udf_false() {
        struct NoClearRecord { val: f64 }
        impl crate::server::record::Record for NoClearRecord {
            fn record_type(&self) -> &'static str { "noclear" }
            fn get_field(&self, name: &str) -> Option<EpicsValue> {
                match name { "VAL" => Some(EpicsValue::Double(self.val)), _ => None }
            }
            fn put_field(&mut self, name: &str, value: EpicsValue) -> crate::error::CaResult<()> {
                match name {
                    "VAL" => {
                        if let EpicsValue::Double(v) = value { self.val = v; Ok(()) }
                        else { Err(CaError::InvalidValue("bad".into())) }
                    }
                    _ => Err(CaError::FieldNotFound(name.into())),
                }
            }
            fn field_list(&self) -> &'static [crate::server::record::FieldDesc] { &[] }
            fn clears_udf(&self) -> bool { false }
        }

        let db = PvDatabase::new();
        db.add_record("REC", Box::new(NoClearRecord { val: 0.0 })).await;

        let rec = db.get_record("REC").await.unwrap();
        assert!(rec.read().await.common.udf);

        let mut visited = HashSet::new();
        db.process_record_with_links("REC", &mut visited, 0).await.unwrap();

        // UDF should still be true
        assert!(rec.read().await.common.udf);
    }

    #[tokio::test]
    async fn test_constant_inp_link() {
        let db = PvDatabase::new();
        db.add_record("AI_CONST", Box::new(AiRecord::new(0.0))).await;

        // Set INP to a constant
        if let Some(rec) = db.get_record("AI_CONST").await {
            let mut inst = rec.write().await;
            inst.put_common_field("INP", EpicsValue::String("3.14".into())).unwrap();
        }

        let mut visited = HashSet::new();
        db.process_record_with_links("AI_CONST", &mut visited, 0).await.unwrap();

        let val = db.get_pv("AI_CONST").await.unwrap();
        match val {
            EpicsValue::Double(v) => assert!((v - 3.14).abs() < 1e-10),
            other => panic!("expected Double(3.14), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_calc_multi_input_db_links() {
        use crate::server::records::calc::CalcRecord;

        let db = PvDatabase::new();
        db.add_record("SRC_A", Box::new(AoRecord::new(10.0))).await;
        db.add_record("SRC_B", Box::new(AoRecord::new(20.0))).await;

        let mut calc = CalcRecord::new("A+B");
        calc.inpa = "SRC_A".to_string();
        calc.inpb = "SRC_B".to_string();
        db.add_record("CALC_REC", Box::new(calc)).await;

        let mut visited = HashSet::new();
        db.process_record_with_links("CALC_REC", &mut visited, 0).await.unwrap();

        let val = db.get_pv("CALC_REC").await.unwrap();
        match val {
            EpicsValue::Double(v) => assert!((v - 30.0).abs() < 1e-10),
            other => panic!("expected Double(30.0), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_calc_constant_inputs() {
        use crate::server::records::calc::CalcRecord;

        let db = PvDatabase::new();
        let mut calc = CalcRecord::new("A+B");
        calc.inpa = "5".to_string();
        calc.inpb = "3.5".to_string();
        db.add_record("CALC_CONST", Box::new(calc)).await;

        let mut visited = HashSet::new();
        db.process_record_with_links("CALC_CONST", &mut visited, 0).await.unwrap();

        let val = db.get_pv("CALC_CONST").await.unwrap();
        match val {
            EpicsValue::Double(v) => assert!((v - 8.5).abs() < 1e-10),
            other => panic!("expected Double(8.5), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_fanout_all() {
        use crate::server::records::fanout::FanoutRecord;

        let db = PvDatabase::new();
        let mut fanout = FanoutRecord::new();
        fanout.selm = 0; // All
        fanout.lnk1 = "TARGET_1".to_string();
        fanout.lnk2 = "TARGET_2".to_string();
        db.add_record("FANOUT", Box::new(fanout)).await;
        db.add_record("TARGET_1", Box::new(AoRecord::new(0.0))).await;
        db.add_record("TARGET_2", Box::new(AoRecord::new(0.0))).await;

        let mut visited = HashSet::new();
        db.process_record_with_links("FANOUT", &mut visited, 0).await.unwrap();

        assert!(visited.contains("FANOUT"));
        assert!(visited.contains("TARGET_1"));
        assert!(visited.contains("TARGET_2"));
    }

    #[tokio::test]
    async fn test_fanout_specified() {
        use crate::server::records::fanout::FanoutRecord;

        let db = PvDatabase::new();
        let mut fanout = FanoutRecord::new();
        fanout.selm = 1; // Specified
        fanout.seln = 1;  // LNK2 (index 1)
        db.add_record("FANOUT", Box::new(fanout)).await;
        db.add_record("T1", Box::new(AoRecord::new(0.0))).await;
        db.add_record("T2", Box::new(AoRecord::new(0.0))).await;

        // Set links
        if let Some(rec) = db.get_record("FANOUT").await {
            let mut inst = rec.write().await;
            inst.record.put_field("LNK1", EpicsValue::String("T1".into())).unwrap();
            inst.record.put_field("LNK2", EpicsValue::String("T2".into())).unwrap();
        }

        let mut visited = HashSet::new();
        db.process_record_with_links("FANOUT", &mut visited, 0).await.unwrap();

        assert!(visited.contains("FANOUT"));
        assert!(!visited.contains("T1")); // Not selected
        assert!(visited.contains("T2"));  // Index 1 selected
    }

    #[tokio::test]
    async fn test_dfanout_value_write() {
        use crate::server::records::dfanout::DfanoutRecord;

        let db = PvDatabase::new();
        let mut dfan = DfanoutRecord::new(42.0);
        dfan.selm = 0; // All
        dfan.outa = "DEST_A".to_string();
        dfan.outb = "DEST_B".to_string();
        db.add_record("DFAN", Box::new(dfan)).await;
        db.add_record("DEST_A", Box::new(AoRecord::new(0.0))).await;
        db.add_record("DEST_B", Box::new(AoRecord::new(0.0))).await;

        let mut visited = HashSet::new();
        db.process_record_with_links("DFAN", &mut visited, 0).await.unwrap();

        let val_a = db.get_pv("DEST_A").await.unwrap();
        match val_a {
            EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
            other => panic!("expected Double(42.0), got {:?}", other),
        }
        let val_b = db.get_pv("DEST_B").await.unwrap();
        match val_b {
            EpicsValue::Double(v) => assert!((v - 42.0).abs() < 1e-10),
            other => panic!("expected Double(42.0), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_seq_dol_lnk_dispatch() {
        use crate::server::records::seq::SeqRecord;

        let db = PvDatabase::new();
        db.add_record("SEQ_SRC1", Box::new(AoRecord::new(100.0))).await;
        db.add_record("SEQ_SRC2", Box::new(AoRecord::new(200.0))).await;
        db.add_record("SEQ_DEST1", Box::new(AoRecord::new(0.0))).await;
        db.add_record("SEQ_DEST2", Box::new(AoRecord::new(0.0))).await;

        let mut seq = SeqRecord::new();
        seq.selm = 0; // All
        seq.dol1 = "SEQ_SRC1".to_string();
        seq.lnk1 = "SEQ_DEST1".to_string();
        seq.dol2 = "SEQ_SRC2".to_string();
        seq.lnk2 = "SEQ_DEST2".to_string();
        db.add_record("SEQ_REC", Box::new(seq)).await;

        let mut visited = HashSet::new();
        db.process_record_with_links("SEQ_REC", &mut visited, 0).await.unwrap();

        let val1 = db.get_pv("SEQ_DEST1").await.unwrap();
        match val1 {
            EpicsValue::Double(v) => assert!((v - 100.0).abs() < 1e-10),
            other => panic!("expected Double(100.0), got {:?}", other),
        }
        let val2 = db.get_pv("SEQ_DEST2").await.unwrap();
        match val2 {
            EpicsValue::Double(v) => assert!((v - 200.0).abs() < 1e-10),
            other => panic!("expected Double(200.0), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_sel_nvl_link() {
        use crate::server::records::sel::SelRecord;

        let db = PvDatabase::new();
        db.add_record("NVL_SRC", Box::new(AoRecord::new(2.0))).await;

        let mut sel = SelRecord::default();
        sel.selm = 0; // Specified
        sel.nvl = "NVL_SRC".to_string();
        sel.a = 10.0;
        sel.b = 20.0;
        sel.c = 30.0;
        db.add_record("SEL_REC", Box::new(sel)).await;

        let mut visited = HashSet::new();
        db.process_record_with_links("SEL_REC", &mut visited, 0).await.unwrap();

        // NVL_SRC=2.0 → SELN=2 → value C=30.0
        let seln = db.get_pv("SEL_REC.SELN").await.unwrap();
        match seln {
            EpicsValue::Short(v) => assert_eq!(v, 2),
            other => panic!("expected Short(2), got {:?}", other),
        }
        let val = db.get_pv("SEL_REC").await.unwrap();
        match val {
            EpicsValue::Double(v) => assert!((v - 30.0).abs() < 1e-10),
            other => panic!("expected Double(30.0), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_select_link_indices() {
        // All
        assert_eq!(super::select_link_indices(0, 0, 6), vec![0,1,2,3,4,5]);
        // Specified
        assert_eq!(super::select_link_indices(1, 2, 6), vec![2]);
        assert_eq!(super::select_link_indices(1, 10, 6), Vec::<usize>::new());
        // Mask: seln=5 = 0b101 → indices 0 and 2
        assert_eq!(super::select_link_indices(2, 5, 6), vec![0, 2]);
    }

    #[tokio::test]
    async fn test_dol_cp_link_registration() {
        let db = PvDatabase::new();

        // Source record (motor RBV)
        db.add_record("MTR", Box::new(AoRecord::new(0.0))).await;

        // Output record with DOL CP link
        let mut ao = AoRecord::new(0.0);
        ao.omsl = 1;
        ao.dol = "MTR CP".to_string();
        db.add_record("MOTOR_POS", Box::new(ao)).await;

        db.setup_cp_links().await;

        let targets = db.get_cp_targets("MTR").await;
        assert_eq!(targets, vec!["MOTOR_POS"]);
    }

    #[tokio::test]
    async fn test_dol_cp_link_triggers_processing() {
        let db = PvDatabase::new();

        // Source record
        db.add_record("SRC", Box::new(AoRecord::new(10.0))).await;

        // Output record with closed-loop DOL CP
        let mut ao = AoRecord::new(0.0);
        ao.omsl = 1;
        ao.dol = "SRC CP".to_string();
        db.add_record("DST", Box::new(ao)).await;

        db.setup_cp_links().await;

        // Process the source — should trigger DST via CP link
        let mut visited = HashSet::new();
        db.process_record_with_links("SRC", &mut visited, 0).await.unwrap();

        // DST should have picked up the value from SRC via DOL
        let val = db.get_pv("DST").await.unwrap();
        match val {
            EpicsValue::Double(v) => assert!((v - 10.0).abs() < 1e-10),
            other => panic!("expected Double(10.0), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_seq_dol_cp_link_registration() {
        use crate::server::records::seq::SeqRecord;

        let db = PvDatabase::new();
        db.add_record("SENSOR", Box::new(AoRecord::new(0.0))).await;

        let mut seq = SeqRecord::default();
        seq.dol1 = "SENSOR CP".to_string();
        db.add_record("MY_SEQ", Box::new(seq)).await;

        db.setup_cp_links().await;

        let targets = db.get_cp_targets("SENSOR").await;
        assert_eq!(targets, vec!["MY_SEQ"]);
    }

    #[tokio::test]
    async fn test_sel_nvl_cp_link_registration() {
        use crate::server::records::sel::SelRecord;

        let db = PvDatabase::new();
        db.add_record("INDEX_SRC", Box::new(AoRecord::new(0.0))).await;

        let mut sel = SelRecord::default();
        sel.nvl = "INDEX_SRC CP".to_string();
        db.add_record("MY_SEL", Box::new(sel)).await;

        db.setup_cp_links().await;

        let targets = db.get_cp_targets("INDEX_SRC").await;
        assert_eq!(targets, vec!["MY_SEL"]);
    }

    #[tokio::test]
    async fn test_sdis_cp_link_registration() {
        let db = PvDatabase::new();
        db.add_record("DISABLE_SRC", Box::new(AoRecord::new(0.0))).await;
        db.add_record("GUARDED", Box::new(AoRecord::new(0.0))).await;

        // Set SDIS CP link on the record's common fields
        if let Some(rec_arc) = db.get_record("GUARDED").await {
            rec_arc.write().await.common.sdis = "DISABLE_SRC CP".to_string();
        }

        db.setup_cp_links().await;

        let targets = db.get_cp_targets("DISABLE_SRC").await;
        assert_eq!(targets, vec!["GUARDED"]);
    }

    #[tokio::test]
    async fn test_tse_minus1_preserves_device_timestamp() {
        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(0.0))).await;

        // Set TSE=-1 and a device timestamp
        let device_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1234567);
        if let Some(rec) = db.get_record("REC").await {
            let mut inst = rec.write().await;
            inst.common.tse = -1;
            inst.common.time = device_time;
        }

        let mut visited = HashSet::new();
        db.process_record_with_links("REC", &mut visited, 0).await.unwrap();

        // TSE=-1: device timestamp should be preserved (not overwritten)
        let rec = db.get_record("REC").await.unwrap();
        let inst = rec.read().await;
        assert_eq!(inst.common.time, device_time);
    }

    #[tokio::test]
    async fn test_tse_minus2_keeps_time_unchanged() {
        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(0.0))).await;

        let fixed_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(999);
        if let Some(rec) = db.get_record("REC").await {
            let mut inst = rec.write().await;
            inst.common.tse = -2;
            inst.common.time = fixed_time;
        }

        let mut visited = HashSet::new();
        db.process_record_with_links("REC", &mut visited, 0).await.unwrap();

        let rec = db.get_record("REC").await.unwrap();
        let inst = rec.read().await;
        assert_eq!(inst.common.time, fixed_time);
    }

    #[tokio::test]
    async fn test_putf_read_only_from_ca() {
        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(0.0))).await;

        let result = db.put_record_field_from_ca("REC", "PUTF", EpicsValue::Char(1)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_rpro_causes_reprocessing() {
        let db = PvDatabase::new();
        db.add_record("SRC", Box::new(AoRecord::new(10.0))).await;
        db.add_record("DEST", Box::new(AiRecord::new(0.0))).await;

        // DEST reads from SRC
        if let Some(rec) = db.get_record("DEST").await {
            let mut inst = rec.write().await;
            inst.put_common_field("INP", EpicsValue::String("SRC".into())).unwrap();
        }

        // Process DEST, it reads SRC=10
        let mut visited = HashSet::new();
        db.process_record_with_links("DEST", &mut visited, 0).await.unwrap();
        let val = db.get_pv("DEST").await.unwrap();
        assert_eq!(val.to_f64().unwrap() as i64, 10);

        // Now change SRC and set RPRO on DEST
        db.put_pv_no_process("SRC", EpicsValue::Double(20.0)).await.unwrap();
        if let Some(rec) = db.get_record("DEST").await {
            let mut inst = rec.write().await;
            inst.common.rpro = true;
        }

        let mut visited = HashSet::new();
        db.process_record_with_links("DEST", &mut visited, 0).await.unwrap();

        // After RPRO, DEST should have re-read SRC=20
        let val = db.get_pv("DEST").await.unwrap();
        assert_eq!(val.to_f64().unwrap() as i64, 20);

        // RPRO should be cleared
        let rec = db.get_record("DEST").await.unwrap();
        let inst = rec.read().await;
        assert!(!inst.common.rpro);
    }

    #[tokio::test]
    async fn test_tsel_cp_link_registration() {
        let db = PvDatabase::new();
        db.add_record("TSE_SRC", Box::new(AoRecord::new(0.0))).await;
        db.add_record("TARGET", Box::new(AiRecord::new(0.0))).await;

        if let Some(rec_arc) = db.get_record("TARGET").await {
            let mut inst = rec_arc.write().await;
            inst.common.tsel = "TSE_SRC CP".to_string();
            inst.parsed_tsel = crate::server::record::parse_link_v2(&inst.common.tsel);
        }

        db.setup_cp_links().await;

        let targets = db.get_cp_targets("TSE_SRC").await;
        assert_eq!(targets, vec!["TARGET"]);
    }

    #[tokio::test]
    async fn test_new_common_fields_get_put() {
        let db = PvDatabase::new();
        db.add_record("REC", Box::new(AoRecord::new(0.0))).await;

        let rec = db.get_record("REC").await.unwrap();

        // UDFS default = Invalid (3)
        {
            let inst = rec.read().await;
            assert_eq!(inst.get_common_field("UDFS"), Some(EpicsValue::Short(3)));
        }
        // Set UDFS = Minor (1)
        {
            let mut inst = rec.write().await;
            inst.put_common_field("UDFS", EpicsValue::Short(1)).unwrap();
        }
        {
            let inst = rec.read().await;
            assert_eq!(inst.get_common_field("UDFS"), Some(EpicsValue::Short(1)));
        }

        // SSCN default = Passive (0)
        {
            let inst = rec.read().await;
            assert_eq!(inst.get_common_field("SSCN"), Some(EpicsValue::Enum(0)));
        }

        // BKPT default = 0
        {
            let inst = rec.read().await;
            assert_eq!(inst.get_common_field("BKPT"), Some(EpicsValue::Char(0)));
        }
        {
            let mut inst = rec.write().await;
            inst.put_common_field("BKPT", EpicsValue::Char(1)).unwrap();
        }
        {
            let inst = rec.read().await;
            assert_eq!(inst.get_common_field("BKPT"), Some(EpicsValue::Char(1)));
        }

        // TSE default = 0
        {
            let inst = rec.read().await;
            assert_eq!(inst.get_common_field("TSE"), Some(EpicsValue::Short(0)));
        }

        // TSEL default = ""
        {
            let inst = rec.read().await;
            assert_eq!(inst.get_common_field("TSEL"), Some(EpicsValue::String(String::new())));
        }

        // PUTF default = 0, read-only
        {
            let inst = rec.read().await;
            assert_eq!(inst.get_common_field("PUTF"), Some(EpicsValue::Char(0)));
        }
        {
            let mut inst = rec.write().await;
            let result = inst.put_common_field("PUTF", EpicsValue::Char(1));
            assert!(result.is_err());
        }

        // RPRO default = false
        {
            let inst = rec.read().await;
            assert_eq!(inst.get_common_field("RPRO"), Some(EpicsValue::Char(0)));
        }
        {
            let mut inst = rec.write().await;
            inst.put_common_field("RPRO", EpicsValue::Char(1)).unwrap();
        }
        {
            let inst = rec.read().await;
            assert_eq!(inst.get_common_field("RPRO"), Some(EpicsValue::Char(1)));
        }
    }
}
