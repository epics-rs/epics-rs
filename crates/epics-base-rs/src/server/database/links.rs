use std::collections::HashSet;
use std::sync::Arc;

use crate::runtime::sync::RwLock;
use crate::server::record::{RecordInstance, ScanType};
use crate::types::EpicsValue;

use super::{select_link_indices, PvDatabase};

impl PvDatabase {
    /// Read a value from a parsed link (DB or Constant). Returns None for None/Ca/Pva.
    pub(crate) async fn read_link_value(&self, link: &crate::server::record::ParsedLink) -> Option<EpicsValue> {
        match link {
            crate::server::record::ParsedLink::None
            | crate::server::record::ParsedLink::Ca(_)
            | crate::server::record::ParsedLink::Pva(_) => None,
            crate::server::record::ParsedLink::Constant(_) => link.constant_value(),
            crate::server::record::ParsedLink::Db(db) => {
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
    pub(crate) async fn read_link_value_soft(
        &self,
        link: &crate::server::record::ParsedLink,
        is_soft: bool,
    ) -> Option<EpicsValue> {
        match link {
            crate::server::record::ParsedLink::Constant(_) => link.constant_value(),
            crate::server::record::ParsedLink::Db(db) if is_soft => {
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
    pub(crate) async fn write_db_link_value(
        &self,
        link: &crate::server::record::DbLink,
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

        if link.policy == crate::server::record::LinkProcessPolicy::ProcessPassive {
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
    pub(crate) async fn dispatch_multi_output(
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
                    let parsed = crate::server::record::parse_link_v2(link_str);
                    if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                        let _ = self.process_record_with_links(&db.record, visited, depth + 1).await;
                    }
                }
            }
            "dfanout" => {
                if let Some(ref val) = val {
                    for idx in indices {
                        let link_str = &links[idx];
                        if link_str.is_empty() { continue; }
                        let parsed = crate::server::record::parse_link_v2(link_str);
                        if let crate::server::record::ParsedLink::Db(ref db) = parsed {
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
                        let dol_parsed = crate::server::record::parse_link_v2(dol_str);
                        self.read_link_value(&dol_parsed).await
                    } else {
                        None
                    };
                    if let Some(value) = dol_val {
                        let lnk_parsed = crate::server::record::parse_link_v2(lnk_str);
                        if let crate::server::record::ParsedLink::Db(ref db) = lnk_parsed {
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
                        let dol_parsed = crate::server::record::parse_link_v2(dol_str);
                        self.read_link_value(&dol_parsed).await
                    } else if !str_val.is_empty() {
                        Some(EpicsValue::String(str_val.to_string()))
                    } else {
                        do_val_str.parse::<f64>().ok().map(EpicsValue::Double)
                    };
                    if let Some(value) = value {
                        let lnk_parsed = crate::server::record::parse_link_v2(lnk_str);
                        if let crate::server::record::ParsedLink::Db(ref db) = lnk_parsed {
                            self.write_db_link_value(db, value, visited, depth).await;
                        }
                    }
                }
            }
            _ => {}
        }
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
                    let parsed = crate::server::record::parse_link_v2(inp_str);
                    if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                        if db.policy == crate::server::record::LinkProcessPolicy::ChannelProcess {
                            links_to_register.push((db.record.clone(), target_name.clone()));
                        }
                    }
                }
                // Check multi-input links (INPA..INPL for calc/calcout/sel/sub)
                for (lf, _vf) in instance.record.multi_input_links() {
                    if let Some(EpicsValue::String(link_str)) = instance.record.get_field(lf) {
                        if !link_str.is_empty() {
                            let parsed = crate::server::record::parse_link_v2(&link_str);
                            if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                                if db.policy == crate::server::record::LinkProcessPolicy::ChannelProcess {
                                    links_to_register.push((db.record.clone(), target_name.clone()));
                                }
                            }
                        }
                    }
                }
                // Check additional input link fields that may use CP:
                // DOL (ao/bo/longout/mbbo), DOL1-DOLA (seq/sseq),
                // NVL (sel), SELL (sseq), SDIS (common), SGNL (histogram)
                const CP_INPUT_LINK_FIELDS: &[&str] = &[
                    "DOL",
                    "DOL1","DOL2","DOL3","DOL4","DOL5","DOL6","DOL7","DOL8","DOL9","DOLA",
                    "NVL", "SELL", "SGNL",
                ];
                for field_name in CP_INPUT_LINK_FIELDS {
                    if let Some(EpicsValue::String(link_str)) = instance.record.get_field(field_name) {
                        if !link_str.is_empty() {
                            let parsed = crate::server::record::parse_link_v2(&link_str);
                            if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                                if db.policy == crate::server::record::LinkProcessPolicy::ChannelProcess {
                                    links_to_register.push((db.record.clone(), target_name.clone()));
                                }
                            }
                        }
                    }
                }
                // Check TSEL in common fields
                let tsel_str = &instance.common.tsel;
                if !tsel_str.is_empty() {
                    let parsed = crate::server::record::parse_link_v2(tsel_str);
                    if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                        if db.policy == crate::server::record::LinkProcessPolicy::ChannelProcess {
                            links_to_register.push((db.record.clone(), target_name.clone()));
                        }
                    }
                }
                // Check SDIS in common fields
                let sdis_str = &instance.common.sdis;
                if !sdis_str.is_empty() {
                    let parsed = crate::server::record::parse_link_v2(sdis_str);
                    if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                        if db.policy == crate::server::record::LinkProcessPolicy::ChannelProcess {
                            links_to_register.push((db.record.clone(), target_name.clone()));
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
}
