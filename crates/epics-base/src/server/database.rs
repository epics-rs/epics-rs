use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use crate::runtime::sync::RwLock;

use crate::error::{CaError, CaResult};
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

    /// Get the current value of a PV or record field.
    /// Uses resolve_field for records (3-level priority).
    pub async fn get_pv(&self, name: &str) -> CaResult<EpicsValue> {
        let (base, field) = parse_pv_name(name);
        let field = field.to_ascii_uppercase();

        // Check simple PVs first (exact match)
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name) {
            return Ok(pv.get().await);
        }

        // Check records — use resolve_field for 3-level priority
        if let Some(rec) = self.inner.records.read().await.get(base) {
            let instance = rec.read().await;
            return instance
                .resolve_field(&field)
                .ok_or_else(|| CaError::ChannelNotFound(name.to_string()));
        }

        Err(CaError::ChannelNotFound(name.to_string()))
    }

    /// Set a PV value or record field, notifying subscribers.
    /// Tries record put_field first, then put_common_field as fallback.
    pub async fn put_pv(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        let (base, field) = parse_pv_name(name);
        let field = field.to_ascii_uppercase();

        // Check simple PVs first
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name) {
            pv.set(value).await;
            return Ok(());
        }

        // Check records
        if let Some(rec) = self.inner.records.read().await.get(base) {
            let mut instance = rec.write().await;

            // Coerce value to field's native type
            let value = {
                let target_type = instance.record.field_list().iter()
                    .find(|f| f.name.eq_ignore_ascii_case(&field))
                    .map(|f| f.dbf_type);
                if let Some(target) = target_type {
                    if value.dbr_type() != target {
                        value.convert_to(target)
                    } else {
                        value
                    }
                } else {
                    value
                }
            };

            // Try record-specific field first; only fall back to common on FieldNotFound
            use crate::server::record::CommonFieldPutResult;
            let common_result = match instance.record.put_field(&field, value.clone()) {
                Ok(()) => CommonFieldPutResult::NoChange,
                Err(CaError::FieldNotFound(_)) => instance.put_common_field(&field, value)?,
                Err(e) => return Err(e),
            };

            instance.cleanup_subscribers();
            instance.notify_field(&field, crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG);

            // Update scan index if SCAN or PHAS changed
            match common_result {
                CommonFieldPutResult::ScanChanged { old_scan, new_scan, phas } => {
                    drop(instance);
                    self.update_scan_index(base, old_scan, new_scan, phas, phas).await;
                }
                CommonFieldPutResult::PhasChanged { scan, old_phas, new_phas } => {
                    drop(instance);
                    self.update_scan_index(base, scan, scan, old_phas, new_phas).await;
                }
                CommonFieldPutResult::NoChange => {}
            }

            return Ok(());
        }

        Err(CaError::ChannelNotFound(name.to_string()))
    }

    /// CA client's unified entry point for record field put.
    /// Handles DISP/PROC/PACT/LCNT checks, field put, device write, and Passive process.
    pub async fn put_record_field_from_ca(
        &self,
        record_name: &str,
        field: &str,
        value: EpicsValue,
    ) -> CaResult<()> {
        let field = field.to_ascii_uppercase();

        // Get record Arc
        let rec = {
            let records = self.inner.records.read().await;
            records.get(record_name).cloned()
                .ok_or_else(|| CaError::ChannelNotFound(record_name.to_string()))?
        };

        // Special field intercepts (read lock, then drop)
        {
            let instance = rec.read().await;
            match field.as_str() {
                "PACT" => return Err(CaError::ReadOnlyField("PACT".into())),
                "LCNT" => return Err(CaError::ReadOnlyField("LCNT".into())),
                _ => {}
            }

            // PROC intercept: trigger processing regardless of DISP
            if field == "PROC" {
                let is_nonzero = match &value {
                    EpicsValue::Char(v) => *v != 0,
                    EpicsValue::Short(v) => *v != 0,
                    EpicsValue::Long(v) => *v != 0,
                    EpicsValue::Double(v) => *v != 0.0,
                    _ => true,
                };
                if is_nonzero {
                    drop(instance);
                    let mut visited = HashSet::new();
                    let _ = self.process_record_with_links(record_name, &mut visited, 0).await;
                    return Ok(());
                }
                return Ok(());
            }

            // DISP check: block CA puts to non-DISP fields when DISP=1
            if instance.common.disp && field != "DISP" {
                return Err(CaError::PutDisabled(field));
            }
        }

        // Normal field put (write lock)
        let (common_result, scan) = {
            let mut instance = rec.write().await;

            // Coerce value to the field's native DBR type (e.g. String → Double for ao.VAL).
            // This matches C EPICS db_put_field() which converts from the CA client's type
            // to the record field's native type.
            let value = {
                let target_type = instance.record.field_list().iter()
                    .find(|f| f.name.eq_ignore_ascii_case(&field))
                    .map(|f| f.dbf_type);
                if let Some(target) = target_type {
                    if value.dbr_type() != target {
                        value.convert_to(target)
                    } else {
                        value
                    }
                } else {
                    value
                }
            };

            // Try record-specific field first; fall back to common on FieldNotFound
            use crate::server::record::CommonFieldPutResult;
            let common_result = match instance.record.put_field(&field, value.clone()) {
                Ok(()) => CommonFieldPutResult::NoChange,
                Err(CaError::FieldNotFound(_)) => {
                    instance.put_common_field(&field, value)?
                }
                Err(e) => return Err(e),
            };

            instance.cleanup_subscribers();
            // For non-Passive, notify immediately; Passive will notify after process
            if instance.common.scan != ScanType::Passive {
                instance.notify_field(&field, crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG);
            }

            let scan = instance.common.scan;
            (common_result, scan)
        };
        // record lock released

        // Update scan index if SCAN or PHAS changed
        match common_result {
            crate::server::record::CommonFieldPutResult::ScanChanged { old_scan, new_scan, phas } => {
                self.update_scan_index(record_name, old_scan, new_scan, phas, phas).await;
            }
            crate::server::record::CommonFieldPutResult::PhasChanged { scan: s, old_phas, new_phas } => {
                self.update_scan_index(record_name, s, s, old_phas, new_phas).await;
            }
            crate::server::record::CommonFieldPutResult::NoChange => {}
        }

        // Process if Passive
        if scan == ScanType::Passive {
            let mut visited = HashSet::new();
            let _ = self.process_record_with_links(record_name, &mut visited, 0).await;
        }

        Ok(())
    }

    /// Process a record by name (process_local + notify).
    pub async fn process_record(&self, name: &str) -> CaResult<()> {
        let rec = {
            let records = self.inner.records.read().await;
            records.get(name).cloned()
        };

        if let Some(rec) = rec {
            let snapshot = {
                let mut instance = rec.write().await;
                instance.process_local()?
            };
            // Notify outside lock
            let instance = rec.read().await;
            instance.notify_from_snapshot(&snapshot);
            Ok(())
        } else {
            Err(CaError::ChannelNotFound(name.to_string()))
        }
    }

    /// Read a value from a parsed link (DB or Constant). Returns None for None/Ca/Pva.
    async fn read_link_value(&self, link: &super::record::ParsedLink) -> Option<EpicsValue> {
        match link {
            super::record::ParsedLink::None
            | super::record::ParsedLink::Ca(_)
            | super::record::ParsedLink::Pva(_) => None,
            super::record::ParsedLink::Constant(_) => link.constant_value(),
            super::record::ParsedLink::Db(db) => {
                let pv_name = if db.field == "VAL" {
                    db.record.clone()
                } else {
                    format!("{}.{}", db.record, db.field)
                };
                self.get_pv(&pv_name).await.ok()
            }
        }
    }

    /// Read a value from a parsed link for INP (only reads DB links when soft channel).
    async fn read_link_value_soft(
        &self,
        link: &super::record::ParsedLink,
        is_soft: bool,
    ) -> Option<EpicsValue> {
        match link {
            super::record::ParsedLink::Constant(_) => link.constant_value(),
            super::record::ParsedLink::Db(db) if is_soft => {
                let pv_name = if db.field == "VAL" {
                    db.record.clone()
                } else {
                    format!("{}.{}", db.record, db.field)
                };
                self.get_pv(&pv_name).await.ok()
            }
            _ => None,
        }
    }

    /// Write a value through a DbLink, optionally processing the target if PP and Passive.
    async fn write_db_link_value(
        &self,
        link: &super::record::DbLink,
        value: EpicsValue,
        visited: &mut HashSet<String>,
        depth: usize,
    ) {
        let target_name = if link.field == "VAL" {
            link.record.clone()
        } else {
            format!("{}.{}", link.record, link.field)
        };
        let _ = self.put_pv(&target_name, value).await;

        if link.policy == super::record::LinkProcessPolicy::ProcessPassive {
            if let Some(target_rec) = self.inner.records.read().await.get(&link.record) {
                let target_scan = target_rec.read().await.common.scan;
                if target_scan == ScanType::Passive {
                    let _ = self
                        .process_record_with_links(&link.record, visited, depth + 1)
                        .await;
                }
            }
        }
    }

    /// Multi-output dispatch for fanout, dfanout, seq record types.
    async fn dispatch_multi_output(
        &self,
        rec: &Arc<RwLock<RecordInstance>>,
        visited: &mut HashSet<String>,
        depth: usize,
    ) {
        let (_rtype, dispatch_info) = {
            let instance = rec.read().await;
            let rtype = instance.record.record_type().to_string();
            match rtype.as_str() {
                "fanout" => {
                    let selm = instance.record.get_field("SELM")
                        .and_then(|v| v.to_f64()).unwrap_or(0.0) as i16;
                    let seln = instance.record.get_field("SELN")
                        .and_then(|v| v.to_f64()).unwrap_or(0.0) as i16;
                    let links: Vec<String> = ["LNK1","LNK2","LNK3","LNK4","LNK5","LNK6"]
                        .iter()
                        .map(|f| instance.record.get_field(f)
                            .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                            .unwrap_or_default())
                        .collect();
                    (rtype, Some(("fanout".to_string(), selm, seln, links, None::<EpicsValue>)))
                }
                "dfanout" => {
                    let selm = instance.record.get_field("SELM")
                        .and_then(|v| v.to_f64()).unwrap_or(0.0) as i16;
                    let seln = instance.record.get_field("SELN")
                        .and_then(|v| v.to_f64()).unwrap_or(0.0) as i16;
                    let val = instance.record.val();
                    let links: Vec<String> = ["OUTA","OUTB","OUTC","OUTD","OUTE","OUTF","OUTG","OUTH"]
                        .iter()
                        .map(|f| instance.record.get_field(f)
                            .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                            .unwrap_or_default())
                        .collect();
                    (rtype, Some(("dfanout".to_string(), selm, seln, links, val)))
                }
                "seq" => {
                    let selm = instance.record.get_field("SELM")
                        .and_then(|v| v.to_f64()).unwrap_or(0.0) as i16;
                    let seln = instance.record.get_field("SELN")
                        .and_then(|v| v.to_f64()).unwrap_or(0.0) as i16;
                    // Collect DOL/LNK pairs
                    let dol_names = ["DOL1","DOL2","DOL3","DOL4","DOL5","DOL6","DOL7","DOL8","DOL9","DOLA"];
                    let lnk_names = ["LNK1","LNK2","LNK3","LNK4","LNK5","LNK6","LNK7","LNK8","LNK9","LNKA"];
                    let mut pairs = Vec::new();
                    for (dol_f, lnk_f) in dol_names.iter().zip(lnk_names.iter()) {
                        let dol_str = instance.record.get_field(dol_f)
                            .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                            .unwrap_or_default();
                        let lnk_str = instance.record.get_field(lnk_f)
                            .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                            .unwrap_or_default();
                        pairs.push(format!("{}\0{}", dol_str, lnk_str));
                    }
                    (rtype, Some(("seq".to_string(), selm, seln, pairs, None)))
                }
                "sseq" => {
                    let selm = instance.record.get_field("SELM")
                        .and_then(|v| v.to_f64()).unwrap_or(0.0) as i16;
                    let seln = instance.record.get_field("SELN")
                        .and_then(|v| v.to_f64()).unwrap_or(0.0) as i16;
                    // Collect DOL/LNK pairs (same as seq but also read DO/STR fields)
                    let dol_names = ["DOL1","DOL2","DOL3","DOL4","DOL5","DOL6","DOL7","DOL8","DOL9","DOLA"];
                    let lnk_names = ["LNK1","LNK2","LNK3","LNK4","LNK5","LNK6","LNK7","LNK8","LNK9","LNKA"];
                    let do_names = ["DO1","DO2","DO3","DO4","DO5","DO6","DO7","DO8","DO9","DOA"];
                    let str_names = ["STR1","STR2","STR3","STR4","STR5","STR6","STR7","STR8","STR9","STRA"];
                    let mut pairs = Vec::new();
                    for i in 0..10 {
                        let dol_str = instance.record.get_field(dol_names[i])
                            .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                            .unwrap_or_default();
                        let lnk_str = instance.record.get_field(lnk_names[i])
                            .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                            .unwrap_or_default();
                        // For sseq: if DOL is empty, use DO/STR value directly
                        let do_val = instance.record.get_field(do_names[i])
                            .and_then(|v| v.to_f64()).unwrap_or(0.0);
                        let str_val = instance.record.get_field(str_names[i])
                            .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                            .unwrap_or_default();
                        // Encode: dol\0lnk\0do_val\0str_val
                        pairs.push(format!("{}\0{}\0{}\0{}", dol_str, lnk_str, do_val, str_val));
                    }
                    (rtype, Some(("sseq".to_string(), selm, seln, pairs, None)))
                }
                _ => (rtype, None),
            }
        };

        let (dispatch_type, selm, seln, links, val) = match dispatch_info {
            Some(info) => info,
            None => return,
        };

        let indices = select_link_indices(selm, seln, links.len());

        match dispatch_type.as_str() {
            "fanout" => {
                for idx in indices {
                    let link_str = &links[idx];
                    if link_str.is_empty() { continue; }
                    let parsed = super::record::parse_link_v2(link_str);
                    if let super::record::ParsedLink::Db(ref db) = parsed {
                        let _ = self.process_record_with_links(&db.record, visited, depth + 1).await;
                    }
                }
            }
            "dfanout" => {
                if let Some(ref val) = val {
                    for idx in indices {
                        let link_str = &links[idx];
                        if link_str.is_empty() { continue; }
                        let parsed = super::record::parse_link_v2(link_str);
                        if let super::record::ParsedLink::Db(ref db) = parsed {
                            self.write_db_link_value(db, val.clone(), visited, depth).await;
                        }
                    }
                }
            }
            "seq" => {
                for idx in indices {
                    let pair_str = &links[idx];
                    let parts: Vec<&str> = pair_str.splitn(2, '\0').collect();
                    if parts.len() != 2 { continue; }
                    let (dol_str, lnk_str) = (parts[0], parts[1]);
                    if lnk_str.is_empty() { continue; }
                    // Read value from DOL
                    let dol_val = if !dol_str.is_empty() {
                        let dol_parsed = super::record::parse_link_v2(dol_str);
                        self.read_link_value(&dol_parsed).await
                    } else {
                        None
                    };
                    if let Some(value) = dol_val {
                        let lnk_parsed = super::record::parse_link_v2(lnk_str);
                        if let super::record::ParsedLink::Db(ref db) = lnk_parsed {
                            self.write_db_link_value(db, value, visited, depth).await;
                        }
                    }
                }
            }
            "sseq" => {
                for idx in indices {
                    let pair_str = &links[idx];
                    let parts: Vec<&str> = pair_str.splitn(4, '\0').collect();
                    if parts.len() != 4 { continue; }
                    let (dol_str, lnk_str, do_val_str, str_val) = (parts[0], parts[1], parts[2], parts[3]);
                    if lnk_str.is_empty() { continue; }
                    // Determine value: read from DOL link, or use DO/STR field
                    let value = if !dol_str.is_empty() {
                        let dol_parsed = super::record::parse_link_v2(dol_str);
                        self.read_link_value(&dol_parsed).await
                    } else if !str_val.is_empty() {
                        Some(EpicsValue::String(str_val.to_string()))
                    } else {
                        do_val_str.parse::<f64>().ok().map(EpicsValue::Double)
                    };
                    if let Some(value) = value {
                        let lnk_parsed = super::record::parse_link_v2(lnk_str);
                        if let super::record::ParsedLink::Db(ref db) = lnk_parsed {
                            self.write_db_link_value(db, value, visited, depth).await;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Process a record with full link handling (INP → process → alarms → OUT → FLNK).
    /// Uses visited set for cycle detection and depth limit.
    pub fn process_record_with_links<'a>(
        &'a self,
        name: &'a str,
        visited: &'a mut HashSet<String>,
        depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CaResult<()>> + Send + 'a>> {
        Box::pin(async move { self.process_record_with_links_inner(name, visited, depth).await })
    }

    async fn process_record_with_links_inner(
        &self,
        name: &str,
        visited: &mut HashSet<String>,
        depth: usize,
    ) -> CaResult<()> {
        const MAX_LINK_DEPTH: usize = 16;
        const MAX_LINK_OPS: usize = 256;

        if depth >= MAX_LINK_DEPTH {
            eprintln!("link chain depth limit reached at record {name}");
            return Ok(());
        }
        if visited.len() >= MAX_LINK_OPS {
            eprintln!("link chain ops budget exhausted at record {name}");
            return Ok(());
        }
        if !visited.insert(name.to_string()) {
            return Ok(()); // Cycle detected, skip
        }

        let rec = {
            let records = self.inner.records.read().await;
            records.get(name).cloned()
        };

        let rec = match rec {
            Some(r) => r,
            None => return Err(CaError::ChannelNotFound(name.to_string())),
        };

        // 0. SDIS disable check
        {
            let (sdis_link, disv, diss) = {
                let instance = rec.read().await;
                (instance.parsed_sdis.clone(), instance.common.disv, instance.common.diss)
            };

            if let super::record::ParsedLink::Db(ref link) = sdis_link {
                let pv_name = if link.field == "VAL" {
                    link.record.clone()
                } else {
                    format!("{}.{}", link.record, link.field)
                };
                if let Ok(val) = self.get_pv(&pv_name).await {
                    let disa_val = val.to_f64().unwrap_or(0.0) as i16;
                    let mut instance = rec.write().await;
                    instance.common.disa = disa_val;
                }
            }

            let disa = rec.read().await.common.disa;
            if disa == disv {
                let mut instance = rec.write().await;
                let prev_sevr = instance.common.sevr;
                let prev_stat = instance.common.stat;
                instance.common.sevr = diss;
                instance.common.stat = crate::server::recgbl::alarm_status::DISABLE_ALARM;
                if instance.common.sevr != prev_sevr || instance.common.stat != prev_stat {
                    let mut changed_fields = Vec::new();
                    changed_fields.push(("SEVR".to_string(), EpicsValue::Short(instance.common.sevr as i16)));
                    changed_fields.push(("STAT".to_string(), EpicsValue::Short(instance.common.stat as i16)));
                    if let Some(val) = instance.record.val() {
                        changed_fields.push(("VAL".to_string(), val));
                    }
                    let snapshot = super::record::ProcessSnapshot {
                        changed_fields,
                        event_mask: crate::server::recgbl::EventMask::ALARM,
                    };
                    instance.notify_from_snapshot(&snapshot);
                }
                return Ok(());
            }
        }

        // 0.5. Simulation mode check
        let sim_result = self.check_simulation_mode(&rec).await;
        if let Some(sim_handled) = sim_result {
            return sim_handled;
        }

        // 1. Read INP link value and DOL link (outside lock)
        let (inp_parsed, is_soft, dol_info) = {
            let instance = rec.read().await;
            let rtype = instance.record.record_type();

            let inp = instance.parsed_inp.clone();
            let is_soft = instance.common.dtyp.is_empty() || instance.common.dtyp == "Soft Channel";

            // DOL link info for output records with OMSL=CLOSED_LOOP
            let dol = match rtype {
                "ao" | "longout" | "bo" | "mbbo" => {
                    let omsl = instance.record.get_field("OMSL")
                        .and_then(|v| if let EpicsValue::Short(s) = v { Some(s) } else { None })
                        .unwrap_or(0);
                    let oif = instance.record.get_field("OIF")
                        .and_then(|v| if let EpicsValue::Short(s) = v { Some(s) } else { None })
                        .unwrap_or(0);
                    if omsl == 1 {
                        let dol_parsed = instance.record.get_field("DOL")
                            .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                            .map(|s| super::record::parse_link_v2(&s))
                            .unwrap_or(super::record::ParsedLink::None);
                        Some((dol_parsed, oif))
                    } else { None }
                }
                _ => None,
            };

            (inp, is_soft, dol)
        };

        // Read INP value
        let inp_value = self.read_link_value_soft(&inp_parsed, is_soft).await;

        // Read DOL value
        let dol_value = if let Some((ref dol_parsed, _oif)) = dol_info {
            self.read_link_value(dol_parsed).await
        } else {
            None
        };

        // 1.5. Multi-input link fetch (calc/calcout/sel/sub)
        let multi_input_values: Vec<(String, EpicsValue)> = {
            let link_info: Vec<(String, String)> = {
                let instance = rec.read().await;
                instance.record.multi_input_links().iter().map(|(lf, vf)| {
                    let link_str = instance.record.get_field(lf)
                        .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                        .unwrap_or_default();
                    (link_str, vf.to_string())
                }).collect()
            }; // read lock dropped
            let mut results = Vec::new();
            for (link_str, val_field) in &link_info {
                if !link_str.is_empty() {
                    let parsed = super::record::parse_link_v2(link_str);
                    if let Some(value) = self.read_link_value(&parsed).await {
                        results.push((val_field.clone(), value));
                    }
                }
            }
            results
        };

        // 1.6. Sel NVL link: resolve NVL → SELN
        let sel_nvl_value: Option<EpicsValue> = {
            let instance = rec.read().await;
            if instance.record.record_type() == "sel" {
                let nvl_str = instance.record.get_field("NVL")
                    .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                    .unwrap_or_default();
                if !nvl_str.is_empty() {
                    drop(instance); // release read lock before async read
                    let parsed = super::record::parse_link_v2(&nvl_str);
                    self.read_link_value(&parsed).await
                } else {
                    None
                }
            } else {
                None
            }
        };

        // 2. Lock record, apply INP/DOL, process, evaluate alarms, build snapshot
        let (snapshot, out_info, flnk_name) = {
            let mut instance = rec.write().await;

            // Apply DOL value for output records (OMSL=CLOSED_LOOP)
            if let Some(dol_val) = dol_value {
                let oif = dol_info.as_ref().map(|(_, oif)| *oif).unwrap_or(0);
                if oif == 1 {
                    // Incremental: VAL += DOL value
                    if let (Some(cur), Some(dol_f)) = (
                        instance.record.val().and_then(|v| v.to_f64()),
                        dol_val.to_f64(),
                    ) {
                        let _ = instance.record.set_val(EpicsValue::Double(cur + dol_f));
                    }
                } else {
                    // Full: VAL = DOL value
                    let _ = instance.record.set_val(dol_val);
                }
            }

            // Apply INP value
            if let Some(inp_val) = inp_value {
                let _ = instance.record.set_val(inp_val);
            }

            // Apply multi-input values (INPA..INPL → A..L)
            for (val_field, value) in &multi_input_values {
                if let Some(f) = value.to_f64() {
                    let _ = instance.record.put_field(val_field, EpicsValue::Double(f));
                }
            }

            // Apply sel NVL → SELN
            if let Some(nvl_val) = sel_nvl_value {
                if let Some(f) = nvl_val.to_f64() {
                    let _ = instance.record.put_field("SELN", EpicsValue::Short(f as i16));
                }
            }

            // Device support read (input records only, not output records)
            let is_soft = instance.common.dtyp.is_empty()
                || instance.common.dtyp == "Soft Channel";
            let is_output = instance.record.can_device_write();
            if !is_soft && !is_output {
                if let Some(mut dev) = instance.device.take() {
                    if let Err(e) = dev.read(&mut *instance.record) {
                        eprintln!("device read error on {}: {e}", instance.name);
                        use crate::server::recgbl::{rec_gbl_set_sevr, alarm_status};
                        rec_gbl_set_sevr(&mut instance.common, alarm_status::READ_ALARM, super::record::AlarmSeverity::Invalid);
                    }
                    instance.device = Some(dev);
                }
            }

            // Process
            let process_result = instance.record.process()?;

            if process_result == super::record::RecordProcessResult::AsyncPending {
                // PACT stays set; skip alarm/timestamp/snapshot/OUT/FLNK
                return Ok(());
            }

            // Evaluate alarms (accumulates into nsta/nsev)
            instance.evaluate_alarms();

            // Device support alarm/timestamp override
            if !is_soft {
                let (dev_alarm, dev_ts) = if let Some(ref dev) = instance.device {
                    (dev.last_alarm(), dev.last_timestamp())
                } else {
                    (None, None)
                };
                if let Some((stat, sevr)) = dev_alarm {
                    use crate::server::recgbl::rec_gbl_set_sevr;
                    rec_gbl_set_sevr(&mut instance.common, stat, super::record::AlarmSeverity::from_u16(sevr));
                }
                if let Some(ts) = dev_ts {
                    instance.common.time = ts;
                }
            }

            // Transfer nsta/nsev → sevr/stat, detect alarm change
            let alarm_result = crate::server::recgbl::rec_gbl_reset_alarms(&mut instance.common);

            // Device support may have set timestamp; if not, use now
            if is_soft || instance.common.time == std::time::SystemTime::UNIX_EPOCH {
                instance.common.time = crate::runtime::time::now_wall();
            }
            if instance.record.clears_udf() {
                instance.common.udf = false;
            }

            // IVOA check for output records with INVALID alarm
            let skip_out = if instance.common.sevr == super::record::AlarmSeverity::Invalid {
                let ivoa = instance.record.get_field("IVOA")
                    .and_then(|v| if let EpicsValue::Short(s) = v { Some(s) } else { None })
                    .unwrap_or(0);
                match ivoa {
                    1 => true, // Don't drive outputs
                    2 => {
                        // Set output to IVOV
                        if let Some(ivov) = instance.record.get_field("IVOV") {
                            let _ = instance.record.set_val(ivov);
                        }
                        false
                    }
                    _ => false, // Continue normally
                }
            } else {
                false
            };

            // OUT stage: soft channel → link put, non-soft → device.write()
            // Must run BEFORE check_deadband_ext so MLST is not prematurely
            // updated for async writes that return early.
            let can_dev_write = instance.record.can_device_write();
            let is_soft_out = instance.common.dtyp.is_empty()
                || instance.common.dtyp == "Soft Channel";
            let record_should_output = instance.record.should_output();
            let out_info = if skip_out {
                None
            } else if !can_dev_write {
                // Non-output records (calcout, etc.) may still have a soft OUT link.
                // Write OVAL to OUT when the record says should_output().
                if record_should_output {
                    if let super::record::ParsedLink::Db(ref link) = instance.parsed_out {
                        let out_val = instance.record.get_field("OVAL")
                            .or_else(|| instance.record.val());
                        out_val.map(|v| (link.clone(), v))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else if is_soft_out {
                if let super::record::ParsedLink::Db(ref link) = instance.parsed_out {
                    let out_val = instance.record.get_field("OVAL")
                        .or_else(|| instance.record.val());
                    out_val.map(|v| (link.clone(), v))
                } else {
                    None
                }
            } else {
                if let Some(mut dev) = instance.device.take() {
                    // Try async write_begin() first
                    match dev.write_begin(&mut *instance.record) {
                        Ok(Some(completion)) => {
                            // Async write submitted — set PACT, return early.
                            // complete_async_record will handle deadband, snapshot,
                            // notification, and FLNK when the write completes.
                            instance.processing.store(true, std::sync::atomic::Ordering::Release);
                            instance.device = Some(dev);
                            let rec_name = instance.name.clone();
                            let timeout = std::time::Duration::from_secs(5);
                            let db = self.clone();
                            tokio::spawn(async move {
                                let _ = tokio::task::spawn_blocking(move || {
                                    completion.wait(timeout)
                                }).await;
                                let _ = db.complete_async_record(&rec_name).await;
                            });
                            return Ok(());
                        }
                        Ok(None) => {
                            // No async support — fall back to synchronous write
                            if let Err(e) = dev.write(&mut *instance.record) {
                                eprintln!("device write error on {}: {e}", instance.name);
                                instance.common.stat = crate::server::recgbl::alarm_status::WRITE_ALARM;
                                instance.common.sevr = super::record::AlarmSeverity::Invalid;
                            }
                        }
                        Err(e) => {
                            eprintln!("device write_begin error on {}: {e}", instance.name);
                            instance.common.stat = crate::server::recgbl::alarm_status::WRITE_ALARM;
                            instance.common.sevr = super::record::AlarmSeverity::Invalid;
                        }
                    }
                    instance.device = Some(dev);
                }
                None
            };

            // Compute event mask (after OUT stage so async writes don't
            // update MLST/ALST prematurely before returning early)
            use crate::server::recgbl::EventMask;
            let mut event_mask = EventMask::NONE;

            let (include_val, include_archive) = instance.check_deadband_ext();
            if include_val {
                event_mask |= EventMask::VALUE;
            }
            if include_archive {
                event_mask |= EventMask::LOG;
            }
            if alarm_result.alarm_changed {
                event_mask |= EventMask::ALARM;
            }

            // Build snapshot — only include fields when something actually triggered
            let mut changed_fields = Vec::new();
            if include_val {
                if let Some(val) = instance.record.val() {
                    changed_fields.push(("VAL".to_string(), val));
                }
                // Include other subscribed fields only when value changes
                for (field, subs) in &instance.subscribers {
                    if !subs.is_empty() && field != "VAL" && field != "SEVR" && field != "STAT" && field != "UDF" {
                        if let Some(val) = instance.resolve_field(field) {
                            changed_fields.push((field.clone(), val));
                        }
                    }
                }
            }
            if alarm_result.alarm_changed {
                changed_fields.push(("SEVR".to_string(), EpicsValue::Short(instance.common.sevr as i16)));
                changed_fields.push(("STAT".to_string(), EpicsValue::Short(instance.common.stat as i16)));
            }
            if !event_mask.is_empty() {
                changed_fields.push(("UDF".to_string(), EpicsValue::Char(if instance.common.udf { 1 } else { 0 })));
            }
            let snapshot = super::record::ProcessSnapshot { changed_fields, event_mask };

            let flnk_name = if instance.record.should_fire_forward_link() {
                if let super::record::ParsedLink::Db(ref l) = instance.parsed_flnk {
                    Some(l.record.clone())
                } else {
                    None
                }
            } else {
                None
            };

            (snapshot, out_info, flnk_name)
        };

        // 3. Notify subscribers (outside lock)
        {
            let instance = rec.read().await;
            instance.notify_from_snapshot(&snapshot);
        }

        // 4. OUT link
        if let Some((link, out_val)) = out_info {
            self.write_db_link_value(&link, out_val, visited, depth).await;
        }

        // 4.5. Multi-output dispatch (fanout/dfanout/seq)
        self.dispatch_multi_output(&rec, visited, depth).await;

        // 4.6. Generic multi-output links (transform OUTA..OUTP → A..P)
        {
            let multi_out = {
                let instance = rec.read().await;
                let links = instance.record.multi_output_links();
                if links.is_empty() {
                    None
                } else {
                    let mut pairs = Vec::new();
                    for &(link_field, val_field) in links {
                        let link_str = instance.record.get_field(link_field)
                            .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                            .unwrap_or_default();
                        if link_str.is_empty() { continue; }
                        if let Some(val) = instance.record.get_field(val_field) {
                            pairs.push((link_str, val));
                        }
                    }
                    if pairs.is_empty() { None } else { Some(pairs) }
                }
            };
            if let Some(pairs) = multi_out {
                for (link_str, val) in pairs {
                    let parsed = super::record::parse_link_v2(&link_str);
                    if let super::record::ParsedLink::Db(ref db) = parsed {
                        self.write_db_link_value(db, val, visited, depth).await;
                    }
                }
            }
        }

        // 5. FLNK
        if let Some(flnk) = flnk_name {
            let _ = self.process_record_with_links(&flnk, visited, depth + 1).await;
        }

        // 6. CP link targets — process records that have CP input links from this record
        {
            let cp_targets = self.get_cp_targets(name).await;
            for target in cp_targets {
                if !visited.contains(&target) {
                    let _ = self.process_record_with_links(&target, visited, depth + 1).await;
                }
            }
        }

        Ok(())
    }

    /// Complete an asynchronous record's post-process steps.
    /// Call after device support signals completion (clears PACT, runs alarms, snapshot, OUT, FLNK).
    pub fn complete_async_record<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CaResult<()>> + Send + 'a>> {
        Box::pin(async move {
            let mut visited = HashSet::new();
            self.complete_async_record_inner(name, &mut visited, 0).await
        })
    }

    async fn complete_async_record_inner(
        &self,
        name: &str,
        visited: &mut HashSet<String>,
        depth: usize,
    ) -> CaResult<()> {
        let rec = {
            let records = self.inner.records.read().await;
            records.get(name).cloned()
                .ok_or_else(|| CaError::ChannelNotFound(name.to_string()))?
        };

        let (snapshot, out_info, flnk_name) = {
            let mut instance = rec.write().await;

            // Evaluate alarms
            instance.evaluate_alarms();

            let is_soft = instance.common.dtyp.is_empty()
                || instance.common.dtyp == "Soft Channel";

            // Device support alarm/timestamp override
            if !is_soft {
                let (dev_alarm, dev_ts) = if let Some(ref dev) = instance.device {
                    (dev.last_alarm(), dev.last_timestamp())
                } else {
                    (None, None)
                };
                if let Some((stat, sevr)) = dev_alarm {
                    crate::server::recgbl::rec_gbl_set_sevr(
                        &mut instance.common, stat,
                        super::record::AlarmSeverity::from_u16(sevr),
                    );
                }
                if let Some(ts) = dev_ts {
                    instance.common.time = ts;
                }
            }

            let alarm_result = crate::server::recgbl::rec_gbl_reset_alarms(&mut instance.common);

            if is_soft || instance.common.time == std::time::SystemTime::UNIX_EPOCH {
                instance.common.time = crate::runtime::time::now_wall();
            }
            if instance.record.clears_udf() {
                instance.common.udf = false;
            }

            // Clear PACT
            instance.processing.store(false, std::sync::atomic::Ordering::Release);

            use crate::server::recgbl::EventMask;
            let mut event_mask = EventMask::NONE;
            let (include_val, include_archive) = instance.check_deadband_ext();
            if include_val { event_mask |= EventMask::VALUE; }
            if include_archive { event_mask |= EventMask::LOG; }
            if alarm_result.alarm_changed { event_mask |= EventMask::ALARM; }

            let mut changed_fields = Vec::new();
            if include_val {
                if let Some(val) = instance.record.val() {
                    changed_fields.push(("VAL".to_string(), val));
                }
            }
            changed_fields.push(("SEVR".to_string(), EpicsValue::Short(instance.common.sevr as i16)));
            changed_fields.push(("STAT".to_string(), EpicsValue::Short(instance.common.stat as i16)));
            changed_fields.push(("UDF".to_string(), EpicsValue::Char(if instance.common.udf { 1 } else { 0 })));
            for (field, subs) in &instance.subscribers {
                if !subs.is_empty() && field != "VAL" && field != "SEVR" && field != "STAT" && field != "UDF" {
                    if let Some(val) = instance.resolve_field(field) {
                        changed_fields.push((field.clone(), val));
                    }
                }
            }
            let snapshot = super::record::ProcessSnapshot { changed_fields, event_mask };

            // IVOA check
            let skip_out = if instance.common.sevr == super::record::AlarmSeverity::Invalid {
                let ivoa = instance.record.get_field("IVOA")
                    .and_then(|v| if let EpicsValue::Short(s) = v { Some(s) } else { None })
                    .unwrap_or(0);
                match ivoa {
                    1 => true,
                    2 => {
                        if let Some(ivov) = instance.record.get_field("IVOV") {
                            let _ = instance.record.set_val(ivov);
                        }
                        false
                    }
                    _ => false,
                }
            } else {
                false
            };

            let can_dev_write = instance.record.can_device_write();
            let is_soft_out = instance.common.dtyp.is_empty()
                || instance.common.dtyp == "Soft Channel";
            let record_should_output = instance.record.should_output();
            let out_info = if skip_out {
                None
            } else if !can_dev_write {
                // Non-output records (calcout, etc.) with soft OUT link
                if record_should_output {
                    if let super::record::ParsedLink::Db(ref link) = instance.parsed_out {
                        let out_val = instance.record.get_field("OVAL")
                            .or_else(|| instance.record.val());
                        out_val.map(|v| (link.clone(), v))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else if is_soft_out {
                if let super::record::ParsedLink::Db(ref link) = instance.parsed_out {
                    let out_val = instance.record.get_field("OVAL")
                        .or_else(|| instance.record.val());
                    out_val.map(|v| (link.clone(), v))
                } else {
                    None
                }
            } else {
                // Non-soft output: the async device write already completed
                // (that's why we're in complete_async_record). Don't re-do
                // write_begin — it would start another async cycle.
                None
            };

            let flnk_name = if instance.record.should_fire_forward_link() {
                if let super::record::ParsedLink::Db(ref l) = instance.parsed_flnk {
                    Some(l.record.clone())
                } else {
                    None
                }
            } else {
                None
            };

            (snapshot, out_info, flnk_name)
        };

        // Notify subscribers
        {
            let instance = rec.read().await;
            instance.notify_from_snapshot(&snapshot);
        }

        // OUT link
        if let Some((link, out_val)) = out_info {
            self.write_db_link_value(&link, out_val, visited, depth).await;
        }

        // Multi-output dispatch (fanout/dfanout/seq/sseq)
        self.dispatch_multi_output(&rec, visited, depth).await;

        // Generic multi-output links (transform OUTA..OUTP → A..P)
        {
            let multi_out = {
                let instance = rec.read().await;
                let links = instance.record.multi_output_links();
                if links.is_empty() {
                    None
                } else {
                    let mut pairs = Vec::new();
                    for &(link_field, val_field) in links {
                        let link_str = instance.record.get_field(link_field)
                            .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                            .unwrap_or_default();
                        if link_str.is_empty() { continue; }
                        if let Some(val) = instance.record.get_field(val_field) {
                            pairs.push((link_str, val));
                        }
                    }
                    if pairs.is_empty() { None } else { Some(pairs) }
                }
            };
            if let Some(pairs) = multi_out {
                for (link_str, val) in pairs {
                    let parsed = super::record::parse_link_v2(&link_str);
                    if let super::record::ParsedLink::Db(ref db) = parsed {
                        self.write_db_link_value(db, val, visited, depth).await;
                    }
                }
            }
        }

        // FLNK
        if let Some(flnk) = flnk_name {
            let _ = self.process_record_with_links(&flnk, visited, depth + 1).await;
        }

        // CP link targets
        {
            let cp_targets = self.get_cp_targets(name).await;
            for target in cp_targets {
                if !visited.contains(&target) {
                    let _ = self.process_record_with_links(&target, visited, depth + 1).await;
                }
            }
        }

        Ok(())
    }

    /// Check simulation mode for a record. Returns Some(Ok(())) if simulation handled processing,
    /// None if normal processing should proceed.
    async fn check_simulation_mode(
        &self,
        rec: &Arc<RwLock<RecordInstance>>,
    ) -> Option<CaResult<()>> {
        // Read SIML, SIMM, SIOL, SIMS from the record
        let (siml_link, siol_link, sims, _rtype, is_input) = {
            let instance = rec.read().await;
            let rtype = instance.record.record_type().to_string();
            let is_input = matches!(rtype.as_str(), "ai" | "bi" | "longin" | "stringin");

            let siml = instance.record.get_field("SIML")
                .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                .unwrap_or_default();
            let siol = instance.record.get_field("SIOL")
                .and_then(|v| if let EpicsValue::String(s) = v { Some(s) } else { None })
                .unwrap_or_default();
            let sims = instance.record.get_field("SIMS")
                .and_then(|v| if let EpicsValue::Short(s) = v { Some(s) } else { None })
                .unwrap_or(0);

            if siml.is_empty() && siol.is_empty() {
                return None; // No simulation configured
            }

            let siml_parsed = crate::server::record::parse_link_v2(&siml);
            let siol_parsed = crate::server::record::parse_link_v2(&siol);

            (siml_parsed, siol_parsed, sims, rtype, is_input)
        };

        // Read SIML → update SIMM
        if let super::record::ParsedLink::Db(ref link) = siml_link {
            let pv_name = if link.field == "VAL" {
                link.record.clone()
            } else {
                format!("{}.{}", link.record, link.field)
            };
            if let Ok(val) = self.get_pv(&pv_name).await {
                let simm_val = val.to_f64().unwrap_or(0.0) as i16;
                let mut instance = rec.write().await;
                let _ = instance.record.put_field("SIMM", EpicsValue::Short(simm_val));
            }
        }

        // Check SIMM
        let simm = {
            let instance = rec.read().await;
            instance.record.get_field("SIMM")
                .and_then(|v| if let EpicsValue::Short(s) = v { Some(s) } else { None })
                .unwrap_or(0)
        };

        if simm == 0 {
            return None; // NO simulation, proceed normally
        }

        // SIMM=YES(1): handle simulation
        if let super::record::ParsedLink::Db(ref link) = siol_link {
            let pv_name = if link.field == "VAL" {
                link.record.clone()
            } else {
                format!("{}.{}", link.record, link.field)
            };

            if is_input {
                // Input record: read from SIOL → set VAL directly (skip conversion)
                if let Ok(siol_val) = self.get_pv(&pv_name).await {
                    let mut instance = rec.write().await;
                    let _ = instance.record.set_val(siol_val);
                    instance.common.time = crate::runtime::time::now_wall();
                    instance.common.udf = false;

                    // Set simulation alarm
                    let sev = super::record::AlarmSeverity::from_u16(sims as u16);
                    if sev != super::record::AlarmSeverity::NoAlarm {
                        instance.common.sevr = sev;
                        instance.common.stat = crate::server::recgbl::alarm_status::SIMM_ALARM;
                    }

                    // Build snapshot and notify
                    let mut changed_fields = Vec::new();
                    if let Some(val) = instance.record.val() {
                        changed_fields.push(("VAL".to_string(), val));
                    }
                    changed_fields.push(("SEVR".to_string(), EpicsValue::Short(instance.common.sevr as i16)));
                    changed_fields.push(("STAT".to_string(), EpicsValue::Short(instance.common.stat as i16)));
                    let snapshot = super::record::ProcessSnapshot {
                        changed_fields,
                        event_mask: crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::ALARM,
                    };
                    instance.notify_from_snapshot(&snapshot);
                }
            } else {
                // Output record: write VAL to SIOL (skip device write)
                let out_val = {
                    let instance = rec.read().await;
                    instance.record.val()
                };
                if let Some(val) = out_val {
                    let _ = self.put_pv(&pv_name, val).await;
                }

                let mut instance = rec.write().await;
                instance.common.time = crate::runtime::time::now_wall();
                instance.common.udf = false;

                let sev = super::record::AlarmSeverity::from_u16(sims as u16);
                if sev != super::record::AlarmSeverity::NoAlarm {
                    instance.common.sevr = sev;
                    instance.common.stat = crate::server::recgbl::alarm_status::SIMM_ALARM;
                }

                // Notify subscribers of simulation output
                let mut changed_fields = Vec::new();
                if let Some(val) = instance.record.val() {
                    changed_fields.push(("VAL".to_string(), val));
                }
                changed_fields.push(("SEVR".to_string(), EpicsValue::Short(instance.common.sevr as i16)));
                changed_fields.push(("STAT".to_string(), EpicsValue::Short(instance.common.stat as i16)));
                let snapshot = super::record::ProcessSnapshot {
                    changed_fields,
                    event_mask: crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::ALARM,
                };
                instance.notify_from_snapshot(&snapshot);
            }
        }

        Some(Ok(()))
    }

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

    /// Register a CP link: when source_record changes, process target_record.
    pub async fn register_cp_link(&self, source_record: &str, target_record: &str) {
        let mut cp = self.inner.cp_links.write().await;
        let targets = cp.entry(source_record.to_string()).or_default();
        if !targets.contains(&target_record.to_string()) {
            targets.push(target_record.to_string());
        }
    }

    /// Get target records that should be processed when source_record changes (CP links).
    pub async fn get_cp_targets(&self, source_record: &str) -> Vec<String> {
        self.inner.cp_links
            .read()
            .await
            .get(source_record)
            .cloned()
            .unwrap_or_default()
    }

    /// Scan all records for CP input links and register them.
    pub async fn setup_cp_links(&self) {
        let names = self.all_record_names().await;
        let mut links_to_register: Vec<(String, String)> = Vec::new();

        for target_name in &names {
            if let Some(rec_arc) = self.get_record(target_name).await {
                let instance = rec_arc.read().await;
                // Check common INP link
                let inp_str = &instance.common.inp;
                if !inp_str.is_empty() {
                    let parsed = super::record::parse_link_v2(inp_str);
                    if let super::record::ParsedLink::Db(ref db) = parsed {
                        if db.policy == super::record::LinkProcessPolicy::ChannelProcess {
                            links_to_register.push((db.record.clone(), target_name.clone()));
                        }
                    }
                }
                // Check multi-input links (INPA..INPL for calc/calcout/sel/sub)
                for (lf, _vf) in instance.record.multi_input_links() {
                    if let Some(EpicsValue::String(link_str)) = instance.record.get_field(lf) {
                        if !link_str.is_empty() {
                            let parsed = super::record::parse_link_v2(&link_str);
                            if let super::record::ParsedLink::Db(ref db) = parsed {
                                if db.policy == super::record::LinkProcessPolicy::ChannelProcess {
                                    links_to_register.push((db.record.clone(), target_name.clone()));
                                }
                            }
                        }
                    }
                }
                // Check DOL link for output records (ao, longout, bo, mbbo, etc.)
                if let Some(EpicsValue::String(dol_str)) = instance.record.get_field("DOL") {
                    if !dol_str.is_empty() {
                        let parsed = super::record::parse_link_v2(&dol_str);
                        if let super::record::ParsedLink::Db(ref db) = parsed {
                            if db.policy == super::record::LinkProcessPolicy::ChannelProcess {
                                links_to_register.push((db.record.clone(), target_name.clone()));
                            }
                        }
                    }
                }
            }
        }

        let count = links_to_register.len();
        for (source, target) in links_to_register {
            self.register_cp_link(&source, &target).await;
        }
        if count > 0 {
            eprintln!("iocInit: {count} CP link subscriptions");
        }
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

    /// Get a record Arc by name.
    pub async fn get_record(&self, name: &str) -> Option<Arc<RwLock<RecordInstance>>> {
        self.inner.records.read().await.get(name).cloned()
    }

    /// Get all record names.
    pub async fn all_record_names(&self) -> Vec<String> {
        self.inner.records.read().await.keys().cloned().collect()
    }

    /// Put a PV value without triggering process (for restore).
    pub async fn put_pv_no_process(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        let (base, field) = parse_pv_name(name);
        let field = field.to_ascii_uppercase();

        if let Some(pv) = self.inner.simple_pvs.read().await.get(name) {
            pv.set(value).await;
            return Ok(());
        }

        if let Some(rec) = self.inner.records.read().await.get(base) {
            let mut instance = rec.write().await;
            match instance.record.put_field(&field, value.clone()) {
                Ok(()) => {}
                Err(CaError::FieldNotFound(_)) => {
                    instance.put_common_field(&field, value)?;
                }
                Err(e) => return Err(e),
            }
            return Ok(());
        }

        Err(CaError::ChannelNotFound(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    async fn test_non_passive_output_ca_put_no_immediate_write() {
        // Non-Passive ao with mock device: CA put should NOT trigger device.write()
        // (no process happens because SCAN != Passive)
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

        assert_eq!(write_count.load(Ordering::SeqCst), 0, "device.write() should NOT be called for non-Passive CA put");
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
}
