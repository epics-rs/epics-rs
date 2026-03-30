use std::collections::HashSet;

use crate::error::{CaError, CaResult};
use crate::server::record::ScanType;
use crate::types::EpicsValue;

use super::PvDatabase;

impl PvDatabase {
    /// Get the current value of a PV or record field.
    /// Uses resolve_field for records (3-level priority).
    pub async fn get_pv(&self, name: &str) -> CaResult<EpicsValue> {
        let (base, field) = super::parse_pv_name(name);
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
        let (base, field) = super::parse_pv_name(name);
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
                CommonFieldPutResult::PhasChanged { scan: s, old_phas, new_phas } => {
                    drop(instance);
                    self.update_scan_index(base, s, s, old_phas, new_phas).await;
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
    ) -> CaResult<Option<crate::runtime::sync::oneshot::Receiver<()>>> {
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
                "PUTF" => return Err(CaError::ReadOnlyField("PUTF".into())),
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
                    return Ok(None);
                }
                return Ok(None);
            }

            // DISP check: block CA puts to non-DISP fields when DISP=1
            if instance.common.disp && field != "DISP" {
                return Err(CaError::PutDisabled(field));
            }
        }

        // Normal field put (write lock)
        let common_result = {
            let mut instance = rec.write().await;
            instance.common.putf = true;

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
                Err(e) => {
                    instance.common.putf = false;
                    return Err(e);
                }
            };

            instance.common.putf = false;

            instance.cleanup_subscribers();
            // For non-Passive non-VAL fields, notify immediately since
            // processing may not post events for auxiliary fields.
            // VAL is always notified via processing (deadband check + snapshot).
            if instance.common.scan != ScanType::Passive && field != "VAL" {
                instance.notify_field(&field, crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG);
            }

            common_result
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

        // Set up put_notify completion channel BEFORE processing.
        // If process returns AsyncPendingNotify, the handler will take
        // the sender and hold it until processing truly completes.
        let (completion_tx, completion_rx) = crate::runtime::sync::oneshot::channel();
        {
            let rec = self.inner.records.read().await;
            if let Some(rec_arc) = rec.get(record_name) {
                rec_arc.write().await.put_notify_tx = Some(completion_tx);
            }
        }

        // Process the record after field put.
        {
            let mut visited = HashSet::new();
            let _ = self.process_record_with_links(record_name, &mut visited, 0).await;
        }

        // Check if sender is still in the record (async processing pending)
        // or was already fired (synchronous completion in Complete path).
        let pending = {
            let rec = self.inner.records.read().await;
            if let Some(rec_arc) = rec.get(record_name) {
                // If sender is still present, async processing is pending.
                // Leave it — it will be fired when processing completes.
                rec_arc.read().await.put_notify_tx.is_some()
            } else {
                false
            }
        };

        if pending {
            Ok(Some(completion_rx))
        } else {
            Ok(None)
        }
    }

    /// Put a PV value without triggering process (for restore).
    pub async fn put_pv_no_process(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        let (base, field) = super::parse_pv_name(name);
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
