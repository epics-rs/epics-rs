use std::collections::HashSet;

use crate::error::{CaError, CaResult};
use crate::server::snapshot::Snapshot;
use crate::types::EpicsValue;

use super::PvDatabase;

impl PvDatabase {
    /// Get a PV value synchronously from a blocking thread.
    ///
    /// Uses `block_in_place` + `Handle::block_on` to bridge the async
    /// `get_pv` call. Safe to call from std::threads spawned within
    /// a tokio runtime context.
    pub fn get_pv_blocking(&self, name: &str) -> CaResult<EpicsValue> {
        let db = self.clone();
        let name = name.to_string();
        if crate::runtime::task::RuntimeHandle::try_current().is_ok() {
            crate::__tokio::task::block_in_place(|| {
                crate::runtime::task::RuntimeHandle::current().block_on(db.get_pv(&name))
            })
        } else {
            Err(CaError::InvalidValue(
                "no runtime for get_pv_blocking".into(),
            ))
        }
    }

    /// Get the current value of a PV or record field.
    /// Uses resolve_field for records (3-level priority).
    pub async fn get_pv(&self, name: &str) -> CaResult<EpicsValue> {
        let (base, field) = super::parse_pv_name(name);
        let field = field.to_ascii_uppercase();

        // Check simple PVs first (exact match)
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name) {
            return Ok(pv.get().await);
        }

        // Records — alias-aware via `get_record` (epics-base PR #336).
        if let Some(rec) = self.get_record(base).await {
            let instance = rec.read().await;
            return instance
                .resolve_field(&field)
                .ok_or_else(|| CaError::ChannelNotFound(name.to_string()));
        }

        Err(CaError::ChannelNotFound(name.to_string()))
    }

    /// Set a PV value or record field, notifying subscribers.
    /// Tries record put_field first, then put_common_field as fallback.
    ///
    /// Acquires the record's advisory write gate.
    pub async fn put_pv(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.put_pv_inner(name, value, true).await
    }

    /// `put_pv` variant for a caller already holding the
    /// record's advisory write gate (QSRV atomic group PUT). See
    /// [`Self::put_record_field_from_ca_already_locked`].
    pub async fn put_pv_already_locked(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.put_pv_inner(name, value, false).await
    }

    async fn put_pv_inner(
        &self,
        name: &str,
        value: EpicsValue,
        acquire_gate: bool,
    ) -> CaResult<()> {
        let (base, field) = super::parse_pv_name(name);
        let field = field.to_ascii_uppercase();

        // Check simple PVs first
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name) {
            pv.set(value).await;
            return Ok(());
        }

        // Records — alias-aware (epics-base PR #336).
        if let Some(rec) = self.get_record(base).await {
            // `base` may be an alias; resolve to the canonical record
            // name so scan-index updates target the right entry.
            let canonical_base: String = self
                .resolve_alias(base)
                .await
                .unwrap_or_else(|| base.to_string());
            // advisory write gate (`dbScanLock` analogue) so a
            // plain `put_pv` to a backing record cannot interleave
            // with an atomic group transaction holding the same gate.
            // Skipped when the caller already owns the gate.
            let _record_gate = if acquire_gate {
                Some(self.lock_record(&canonical_base).await)
            } else {
                None
            };
            let mut instance = rec.write().await;

            // Coerce value to field's native type
            let value = {
                let target_type = instance
                    .record
                    .field_list()
                    .iter()
                    .find(|f| f.name.eq_ignore_ascii_case(&field))
                    .map(|f| f.dbf_type);
                if let Some(target) = target_type {
                    if value.db_field_type() != target {
                        // C EPICS dbPut (12cfd41): nRequest=0 into a scalar
                        // field must NOT silently coerce. `convert_to` on an
                        // empty array calls `to_f64().unwrap_or(0.0)` and
                        // would produce a scalar zero — the same garbage-
                        // value bug the C fix raised LINK_ALARM for.
                        if value.is_empty_array() {
                            return Err(CaError::InvalidValue(format!(
                                "empty array cannot be coerced to scalar field {field}"
                            )));
                        }
                        value.convert_to(target)
                    } else {
                        value
                    }
                } else {
                    value
                }
            };

            // Capture the pre-put value so the metadata-cache
            // invalidation (and the downstream `DBE_PROPERTY`
            // emission) can be skipped when the put is a no-op —
            // epics-base faac1df1.
            let prev_value = instance.record.get_field(&field);

            // put_pv is C EPICS dbPut: write value + special/on_put.
            // Does NOT post monitor events (use put_pv_and_post for that).
            // Does NOT clear UDF or trigger processing.
            use crate::server::record::CommonFieldPutResult;
            let common_result = match instance.record.put_field(&field, value.clone()) {
                Ok(()) => {
                    instance.record.on_put(&field);
                    let _ = instance.record.special(&field, true);
                    CommonFieldPutResult::NoChange
                }
                Err(CaError::FieldNotFound(_)) => instance.put_common_field(&field, value)?,
                Err(e) => return Err(e),
            };

            // Invalidate metadata cache only if the metadata-class
            // field's value actually changed (faac1df1).
            instance.notify_field_written_if_changed(&field, prev_value.as_ref());

            // Update scan index if SCAN or PHAS changed
            match common_result {
                CommonFieldPutResult::ScanChanged {
                    old_scan,
                    new_scan,
                    phas,
                } => {
                    drop(instance);
                    self.update_scan_index(&canonical_base, old_scan, new_scan, phas, phas)
                        .await;
                }
                CommonFieldPutResult::PhasChanged {
                    scan: s,
                    old_phas,
                    new_phas,
                } => {
                    drop(instance);
                    self.update_scan_index(&canonical_base, s, s, old_phas, new_phas)
                        .await;
                }
                CommonFieldPutResult::NoChange => {}
            }

            // mirror the CA-write path's ASG-field notifier so
            // restore scripts / autosave / admin tools that go via
            // `put_pv` (not `put_record_field_from_ca`) also trigger
            // per-client `reeval_access_rights`. C `dbAccess.c::
            // dbPutSpecial` invokes the SPC_AS callback from dbPut
            // regardless of caller entry path.
            if field == "ASG" {
                crate::server::access_security::notify_asg_field_changed();
            }

            return Ok(());
        }

        Err(CaError::ChannelNotFound(name.to_string()))
    }

    /// Write a value and post monitor events if changed.
    /// Equivalent to C EPICS `dbPut` + `db_post_events(DBE_VALUE|DBE_LOG)`.
    ///
    /// Use for readback/status mirror PVs that are written by sequencer-style
    /// code and need to be visible to CA monitors without triggering record
    /// processing. Clears UDF/UDF_ALARM on primary field write.
    ///
    /// `origin`: writer ID for self-write filtering. Subscribers with the
    /// same `ignore_origin` will skip this event. Pass 0 to disable.
    pub async fn put_pv_and_post(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.put_pv_and_post_with_origin(name, value, 0).await
    }

    /// Push a monitor event holding the simple PV's *current* value
    /// but with explicit alarm severity/status. Used by the gateway
    /// to surface upstream-disconnect to downstream monitor
    /// subscribers without dropping the shadow PV (which would force
    /// downstream clients into ECA_DISCONN reconnect storms on every
    /// transient hiccup). Returns `ChannelNotFound` for record-backed
    /// PVs — those carry their own `common.sevr/stat` in record
    /// processing.
    pub async fn post_alarm(&self, name: &str, severity: u16, status: u16) -> CaResult<()> {
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name).cloned() {
            pv.post_alarm(severity, status).await;
            return Ok(());
        }
        Err(crate::error::CaError::ChannelNotFound(name.to_string()))
    }

    /// Propagate a full upstream snapshot (value + alarm status/severity +
    /// IOC timestamp) to a simple shadow PV and fan out to downstream
    /// monitor subscribers. Used by the CA gateway forwarding task to avoid
    /// discarding the upstream alarm and timestamp decoded from the incoming
    /// `DBR_TIME_*` frame. Returns `ChannelNotFound` for record-backed PVs
    /// (those carry their own alarm engine and are not shadow PVs).
    pub async fn put_pv_and_post_snapshot(&self, name: &str, snapshot: Snapshot) -> CaResult<()> {
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name).cloned() {
            pv.set_snapshot(snapshot).await;
            return Ok(());
        }
        Err(CaError::ChannelNotFound(name.to_string()))
    }

    /// Like `put_pv_and_post` but with explicit origin tag.
    pub async fn put_pv_and_post_with_origin(
        &self,
        name: &str,
        value: EpicsValue,
        origin: u64,
    ) -> CaResult<()> {
        let (base, field) = super::parse_pv_name(name);
        let field = field.to_ascii_uppercase();

        // Simple-PV path: PVs registered via `add_pv` (e.g. CA gateway
        // shadow PVs, IOCsh stats PVs) are stored in `simple_pvs`,
        // not `records`. Without this branch the function would
        // silently return `ChannelNotFound` for every gateway-mirrored
        // PV — `ProcessVariable::set` already does the
        // notify-subscribers fan-out internally so all we need here is
        // to delegate. The `origin` tag is a no-op for simple PVs
        // because they don't yet plumb origin through `set`.
        if let Some(pv) = self.inner.simple_pvs.read().await.get(name).cloned() {
            let _ = origin; // simple PVs don't currently honor origin tagging
            pv.set(value).await;
            return Ok(());
        }

        if let Some(rec) = self.get_record(base).await {
            // `put_pv_and_post` is a public record-write API —
            // it must take the same advisory write gate
            // (`dbScanLock` analogue) as `put_pv` /
            // `put_record_field_from_ca`, or a gateway/sequencer
            // write through this helper can still land between the
            // member writes of a QSRV atomic group or a pvalink
            // atomic scan epoch holding `lock_records`. `base` is
            // alias-resolved to the canonical record name so an alias
            // and its target share one gate. Held until return.
            let canonical_base: String = self
                .resolve_alias(base)
                .await
                .unwrap_or_else(|| base.to_string());
            let _record_gate = self.lock_record(&canonical_base).await;

            let mut instance = rec.write().await;

            // Type coercion
            let value = {
                let target_type = instance
                    .record
                    .field_list()
                    .iter()
                    .find(|f| f.name.eq_ignore_ascii_case(&field))
                    .map(|f| f.dbf_type);
                if let Some(target) = target_type {
                    if value.db_field_type() != target {
                        // C EPICS dbPut (12cfd41): empty-array → scalar
                        // coercion would produce silent zero; reject.
                        if value.is_empty_array() {
                            return Err(CaError::InvalidValue(format!(
                                "empty array cannot be coerced to scalar field {field}"
                            )));
                        }
                        value.convert_to(target)
                    } else {
                        value
                    }
                } else {
                    value
                }
            };

            let old_value = instance.record.get_field(&field);
            let old_stat = instance.common.stat;
            let old_sevr = instance.common.sevr;
            // Snapshot side-effect-prone fields BEFORE the put. The
            // array-family records (waveform/aai/aao/subArray) update
            // NORD as a side-effect of put_field("VAL"); other record
            // types return None for "NORD" and the comparison reduces
            // to None==None → unchanged.
            let old_nord = if field == "VAL" {
                instance.record.get_field("NORD")
            } else {
                None
            };

            // Write value + special/on_put
            match instance.record.put_field(&field, value.clone()) {
                Ok(()) => {
                    instance.record.on_put(&field);
                    let _ = instance.record.special(&field, true);
                    // Clear UDF/UDF_ALARM on primary field write
                    if field == instance.record.primary_field() {
                        instance.common.udf = false;
                        if instance.common.stat == crate::server::recgbl::alarm_status::UDF_ALARM {
                            instance.common.stat = 0;
                            instance.common.sevr = crate::server::record::AlarmSeverity::NoAlarm;
                        }
                    }
                }
                Err(CaError::FieldNotFound(_)) => {
                    instance.put_common_field(&field, value)?;
                }
                Err(e) => return Err(e),
            }

            // Invalidate metadata cache only if a metadata-class
            // field actually changed value (faac1df1 — DBE_PROPERTY
            // fires on real changes, not no-op writes).
            instance.notify_field_written_if_changed(&field, old_value.as_ref());

            // Post monitor events if value or alarm changed
            let new_value = instance.record.get_field(&field);
            let value_changed = old_value != new_value;
            let alarm_changed =
                old_stat != instance.common.stat || old_sevr != instance.common.sevr;
            let new_nord = if field == "VAL" {
                instance.record.get_field("NORD")
            } else {
                None
            };
            let nord_changed = field == "VAL" && old_nord != new_nord && new_nord.is_some();
            if value_changed || alarm_changed || nord_changed {
                // Update timestamp so the snapshot carries current time
                instance.common.time = crate::runtime::general_time::get_current();
                instance.cleanup_subscribers();
                if value_changed || alarm_changed {
                    instance.notify_field_with_origin(
                        &field,
                        crate::server::recgbl::EventMask::VALUE
                            | crate::server::recgbl::EventMask::LOG
                            | crate::server::recgbl::EventMask::ALARM,
                        origin,
                    );
                }
                // Surface the implicit NORD update to NORD subscribers
                // for waveform/aai/aao/subArray. Without this, a CA
                // gateway forwarding upstream waveform monitors via
                // put_pv_and_post would update VAL on the shadow PV
                // but leave downstream NORD subscribers stuck at their
                // last seen length — a frozen-element-count bug that
                // surfaces in PyDM image views and similar consumers
                // that compute height = element_count / width.
                if nord_changed {
                    instance.notify_field_with_origin(
                        "NORD",
                        crate::server::recgbl::EventMask::VALUE
                            | crate::server::recgbl::EventMask::LOG,
                        origin,
                    );
                }
            }

            // same SPC_AS parity as `put_pv` / `put_pv_no_process`
            // / the CA-write path — a gateway mirroring `.ASG` via
            // `put_pv_and_post` must still trigger per-client
            // re-eval.
            if field == "ASG" {
                crate::server::access_security::notify_asg_field_changed();
            }

            return Ok(());
        }

        Err(CaError::ChannelNotFound(name.to_string()))
    }

    /// CA client's unified entry point for record field put.
    /// Handles DISP/PROC/PACT/LCNT checks, field put, device write, and Passive process.
    ///
    /// Acquires the record's advisory write gate
    /// (`dbScanLock` analogue) for the duration of the write.
    pub async fn put_record_field_from_ca(
        &self,
        record_name: &str,
        field: &str,
        value: EpicsValue,
    ) -> CaResult<Option<crate::runtime::sync::oneshot::Receiver<()>>> {
        self.put_record_field_from_ca_inner(record_name, field, value, true)
            .await
    }

    /// Variant for a caller that already owns the target
    /// record's advisory write gate — the QSRV atomic group PUT,
    /// which acquired every member-record gate up-front via
    /// [`Self::lock_records`]. The per-record `tokio::sync::Mutex`
    /// gate is NOT reentrant, so the atomic group path MUST use this
    /// `_already_locked` entry to avoid dead-locking on its own
    /// `ManyRecordWriteGuard`.
    pub async fn put_record_field_from_ca_already_locked(
        &self,
        record_name: &str,
        field: &str,
        value: EpicsValue,
    ) -> CaResult<Option<crate::runtime::sync::oneshot::Receiver<()>>> {
        self.put_record_field_from_ca_inner(record_name, field, value, false)
            .await
    }

    async fn put_record_field_from_ca_inner(
        &self,
        record_name: &str,
        field: &str,
        value: EpicsValue,
        acquire_gate: bool,
    ) -> CaResult<Option<crate::runtime::sync::oneshot::Receiver<()>>> {
        let field = field.to_ascii_uppercase();

        // Get record Arc — alias-aware (epics-base PR #336) so a CA
        // client that connects via an alias name can put fields on
        // the canonical record.
        let rec = self
            .get_record(record_name)
            .await
            .ok_or_else(|| CaError::ChannelNotFound(record_name.to_string()))?;
        // Normalise to the canonical name for the rest of this
        // function — every subsequent call (PACT/LCNT lookup,
        // `process_record_with_links`, `update_scan_index`) uses the
        // raw records map and would miss when `record_name` is an
        // alias. Resolve once up front.
        let canonical_owned;
        let record_name: &str = if let Some(target) = self.resolve_alias(record_name).await {
            canonical_owned = target;
            &canonical_owned
        } else {
            record_name
        };

        // take the record's advisory write gate — the
        // `dbScanLock(precord)` analogue. While a QSRV atomic group
        // PUT/GET holds this record's gate via `lock_records`, this
        // plain write blocks here, so a direct backing-record write
        // can no longer land between member writes of an atomic group
        // transaction. Held until the function returns. Skipped when
        // the caller (atomic group PUT) already owns the gate — the
        // gate `Mutex` is not reentrant.
        let _record_gate = if acquire_gate {
            Some(self.lock_record(record_name).await)
        } else {
            None
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

            // PROC intercept: trigger processing regardless of DISP.
            // Falls through to the put_notify_tx registration below
            // so async records (motor, asyn-backed AO) signal real
            // completion; otherwise WRITE_NOTIFY would return ECA_NORMAL
            // before the device move actually finished.
            if field == "PROC" {
                let is_nonzero = match &value {
                    EpicsValue::Char(v) => *v != 0,
                    EpicsValue::Short(v) => *v != 0,
                    EpicsValue::Long(v) => *v != 0,
                    EpicsValue::Int64(v) => *v != 0,
                    EpicsValue::Double(v) => *v != 0.0,
                    _ => true,
                };
                drop(instance);
                if !is_nonzero {
                    return Ok(None);
                }
                // Continue to the put-notify setup + process below
                // by jumping past the field-write step (the value
                // itself isn't stored; PROC is a trigger).
                let (completion_tx, completion_rx) = crate::runtime::sync::oneshot::channel();
                let notify = crate::server::record::NotifyWaitSet::new(completion_tx);
                {
                    let rec = self.inner.records.read().await;
                    if let Some(rec_arc) = rec.get(record_name) {
                        let mut guard = rec_arc.write().await;
                        if guard.notify.is_some() {
                            return Err(CaError::PutCallbackInProgress(record_name.to_string()));
                        }
                        guard.notify = Some(notify.clone());
                    }
                }
                let mut visited = HashSet::new();
                // this PROC trigger already holds `record_name`'s
                // advisory write gate — either `_record_gate` above, or
                // the QSRV atomic group's `lock_records` epoch when
                // entered via `put_record_field_from_ca_already_locked`.
                // The gate `Mutex` is not reentrant, so the processing
                // call MUST use the `_already_locked` variant.
                let _ = self
                    .process_record_with_links_already_locked(record_name, &mut visited, 0)
                    .await;
                // The wait-set fires the oneshot only after the whole
                // FLNK/OUT chain (sync + async) settles. If it has
                // already completed the chain was fully synchronous —
                // report immediate success; otherwise hand the receiver
                // to the CA layer to await the deferred completion.
                return if notify.completed() {
                    Ok(None)
                } else {
                    Ok(Some(completion_rx))
                };
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
                let target_type = instance
                    .record
                    .field_list()
                    .iter()
                    .find(|f| f.name.eq_ignore_ascii_case(&field))
                    .map(|f| f.dbf_type);
                if let Some(target) = target_type {
                    if value.db_field_type() != target {
                        // C EPICS dbPut (12cfd41): empty-array → scalar
                        // coercion would produce silent zero; reject.
                        if value.is_empty_array() {
                            instance.common.putf = false;
                            return Err(CaError::InvalidValue(format!(
                                "empty array cannot be coerced to scalar field {field}"
                            )));
                        }
                        value.convert_to(target)
                    } else {
                        value
                    }
                } else {
                    value
                }
            };

            // SPC_NOMOD: reject writes to read-only fields (C EPICS S_db_noMod)
            let is_read_only = instance
                .record
                .field_list()
                .iter()
                .find(|f| f.name.eq_ignore_ascii_case(&field))
                .is_some_and(|f| f.read_only);
            if is_read_only {
                instance.common.putf = false;
                return Err(CaError::ReadOnlyField(field));
            }

            // Pre-write special hook (C EPICS dbPutSpecial pass=0)
            if let Err(e) = instance.record.special(&field, false) {
                instance.common.putf = false;
                return Err(e);
            }

            // Capture pre-put value for faac1df1 idempotent-write suppression.
            let prev_value = instance.record.get_field(&field);

            // Try record-specific field first; fall back to common on FieldNotFound.
            // For record-owned fields, call on_put() and special() after successful put,
            // matching what put_common_field() does for common fields.
            use crate::server::record::CommonFieldPutResult;
            let common_result = match instance.record.put_field(&field, value.clone()) {
                Ok(()) => {
                    instance.record.on_put(&field);
                    let _ = instance.record.special(&field, true);
                    // C `dbAccess.c::dbPut:1410-1411` clears
                    // `precord->udf = FALSE` synchronously when the
                    // put target is the record-type's primary value
                    // field (`dbIsValueField`). The clear happens
                    // BEFORE `dbProcess` runs, so any reader between
                    // the put and the process-cycle's own clear sees
                    // the new value with a consistent UDF=false.
                    //
                    // Rust's processing path also clears UDF via
                    // `clears_udf()` in process/complete_async_record,
                    // but that runs AFTER the put lock drops and the
                    // process re-acquires — leaving a small window
                    // where another reader can observe (new VAL,
                    // udf=true). For async records the window spans
                    // the entire device round trip. Clear here to
                    // close the window. The same clear already exists
                    // in `put_pv_and_post` (line 256-262); mirror it.
                    if field == instance.record.primary_field() {
                        instance.common.udf = false;
                        if instance.common.stat == crate::server::recgbl::alarm_status::UDF_ALARM {
                            instance.common.stat = 0;
                            instance.common.sevr = crate::server::record::AlarmSeverity::NoAlarm;
                        }
                    }
                    CommonFieldPutResult::NoChange
                }
                Err(CaError::FieldNotFound(_)) => instance.put_common_field(&field, value)?,
                Err(e) => {
                    instance.common.putf = false;
                    return Err(e);
                }
            };

            // Invalidate metadata cache only if the metadata-class
            // field's value actually changed (faac1df1).
            instance.notify_field_written_if_changed(&field, prev_value.as_ref());

            // C `dbAccess.c::dbPutField:1276` sets `precord->putf = TRUE`
            // immediately before calling `dbProcess`, and the flag stays
            // TRUE through the entire process cycle. It is cleared only
            // in `recGblFwdLink` (recGbl.c:302) after FLNK fires, OR in
            // the disable-alarm bail (dbAccess.c:576). The Rust port
            // previously cleared `putf` here — BEFORE the
            // `process_record_with_links` call below — so any code
            // path (TPRO trace, async-completion logic, monitor on
            // .PUTF) observing the bit during the process cycle saw
            // `putf=0` and could not distinguish put-driven vs
            // scan-driven processing.
            //
            // DO NOT clear `putf` here. The clearing now happens after
            // the process call returns (synchronous completion) or in
            // `complete_async_record` (async completion).

            instance.cleanup_subscribers();
            // C `dbPut:1408-1414` posts DBE_VALUE|DBE_LOG for the put field
            // unless `(isValueField && pfldDes->process_passive)` — the
            // immediate post is suppressed for the value field ONLY when that
            // field is `pp(TRUE)`, because then the reprocess cycle
            // (`dbPutField:1265-1268`) re-posts it via the deadband snapshot.
            // For a value field that is NOT `pp` (calc/calcout/aSub VAL), C
            // posts here and does not reprocess; the port must do the same,
            // because the `should_process` gate below skips the cycle for a
            // non-`pp` value field — without this post a direct VAL put would
            // fire no monitor at all.
            let suppress_value_field_post = field == instance.record.primary_field()
                && match instance.record.process_passive_fields() {
                    Some(pp) => pp.iter().any(|f| f.eq_ignore_ascii_case(&field)),
                    // Un-modeled record types keep the legacy "process on every
                    // put" behavior (`should_process = true` below), so the
                    // reprocess cycle posts the value field — suppress the
                    // immediate post here to avoid a duplicate event.
                    None => true,
                };
            if !suppress_value_field_post {
                instance.notify_field(
                    &field,
                    crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG,
                );
            }

            // Fields a `special()` changed as a side effect of this put
            // (e.g. compress RES reset zeroing NUSE/VAL) get their monitors
            // posted here, mirroring the explicit `db_post_events` a C
            // `special()` makes — these fields are not pp(TRUE), so no
            // process cycle would otherwise post them.
            for sf in instance.record.monitor_side_effect_fields(&field) {
                instance.notify_field(
                    sf,
                    crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG,
                );
            }

            common_result
        };
        // ASG-field change re-evaluation hook. C
        // `asDbLib.c:107-110,144` `asSpcAsCallback` invokes
        // `asChangeGroup` → `asAddMemberPvt` → `asComputePvt` for
        // every `ASGCLIENT` on `dbPut record.ASG NEW_ASG`. Pre-fix
        // Rust mutated `common.asg` directly with no notification,
        // so the wire ACCESS_RIGHTS the client saw still reflected
        // the OLD ASG until something else triggered re-eval. Now we
        // fire a process-wide notifier that the CA server folds into
        // its per-client `reeval_access_rights` path.
        if field == "ASG" {
            crate::server::access_security::notify_asg_field_changed();
        }
        // record lock released

        // Update scan index if SCAN or PHAS changed
        match common_result {
            crate::server::record::CommonFieldPutResult::ScanChanged {
                old_scan,
                new_scan,
                phas,
            } => {
                self.update_scan_index(record_name, old_scan, new_scan, phas, phas)
                    .await;
            }
            crate::server::record::CommonFieldPutResult::PhasChanged {
                scan: s,
                old_phas,
                new_phas,
            } => {
                self.update_scan_index(record_name, s, s, old_phas, new_phas)
                    .await;
            }
            crate::server::record::CommonFieldPutResult::NoChange => {}
        }

        // C `dbAccess.c::dbPutField:1263-1268` re-processes the
        // record on a put only when the put field is `pp(TRUE)` AND the
        // record is Passive (`SCAN == 0`). (The `PROC` field has its own
        // always-process intercept above, matching C's
        // `pfield == &precord->proc`; alarm-ack fields like ACKT/ACKS are
        // not `pp(TRUE)` so they fall out here, matching C's
        // `dbrType < DBR_PUT_ACKT`.) Processing on every put would
        // double-process scanned records and spuriously process puts to
        // non-`pp` fields (extra FLNK / monitors / device writes /
        // timestamps). A record type whose DBD pp-flags are not modeled
        // returns `None` and keeps the legacy "process on every put"
        // behavior so un-modeled types (other crates, tests) are unchanged.
        let should_process = {
            let instance = rec.read().await;
            match instance.record.process_passive_fields() {
                Some(pp) => {
                    instance.common.scan == crate::server::record::ScanType::Passive
                        && pp.iter().any(|f| f.eq_ignore_ascii_case(&field))
                }
                None => true,
            }
        };

        if !should_process {
            // No processing cycle. C never sets `putf` on this path, so
            // clear the flag the field-put set at entry, and report
            // immediate (synchronous) completion to a WRITE_NOTIFY caller.
            let recs = self.inner.records.read().await;
            if let Some(rec_arc) = recs.get(record_name) {
                let mut guard = rec_arc.write().await;
                if !guard.is_processing() {
                    guard.common.putf = false;
                }
            }
            return Ok(None);
        }

        // Set up the put-notify wait-set BEFORE processing. The wait-set
        // fires `completion_tx` only after the originating record AND
        // every FLNK/OUT chain target it triggers (sync or async) has
        // completed — C `dbNotify.c` `processNotify`/`dbNotifyCompletion`.
        // Refuse a second concurrent WRITE_NOTIFY on the same record:
        // C EPICS returns S_db_Blocked / ECA_PUTCBINPROG, and silently
        // overwriting the wait-set would drop the prior Sender, waking
        // the prior caller's rx with RecvError that the CA dispatcher
        // treats as success.
        let (completion_tx, completion_rx) = crate::runtime::sync::oneshot::channel();
        let notify = crate::server::record::NotifyWaitSet::new(completion_tx);
        {
            let rec = self.inner.records.read().await;
            if let Some(rec_arc) = rec.get(record_name) {
                let mut guard = rec_arc.write().await;
                if guard.notify.is_some() {
                    return Err(CaError::PutCallbackInProgress(record_name.to_string()));
                }
                guard.notify = Some(notify.clone());
            }
        }

        // When a CA put writes directly to VAL on an INPUT record whose
        // VAL is the engineering value, the built-in `RVAL → VAL`
        // `convert()` must be suppressed for the put-driven process —
        // re-deriving VAL from a stale RVAL would clobber the value the
        // operator just wrote (the soft ai preset-NaN case, processing.rs
        // ~line 677). The framework expresses this by calling
        // `set_device_did_compute(true)`.
        //
        // This MUST be gated on `soft_channel_skips_convert()`. Output
        // records (mbbo/mbbo_direct/bo/ao) implement
        // `set_device_did_compute` as "skip the VAL → RVAL output
        // convert" — the OPPOSITE direction. C `mbboRecord.c::process`
        // (line 217), `mbboDirectRecord.c::process` (line 198) and
        // `boRecord.c::process` (line 207) call `convert()`
        // unconditionally on every non-pact process; a CA VAL-put on an
        // output record MUST recompute RVAL/ORAW. Suppressing it there
        // left RVAL/ORAW/ORBV stale. Output records return the default
        // `false` from `soft_channel_skips_convert()`, so this gate
        // matches the identical gates in processing.rs (line 694) and
        // record_instance.rs (line 1381).
        if field == "VAL" {
            let recs = self.inner.records.read().await;
            if let Some(rec_arc) = recs.get(record_name) {
                let mut guard = rec_arc.write().await;
                if guard.record.soft_channel_skips_convert() {
                    guard.record.set_device_did_compute(true);
                }
            }
        }

        // Process the record after field put.
        {
            let mut visited = HashSet::new();
            // `record_name`'s advisory write gate is already
            // held by this `put` (the `_record_gate` taken above, or
            // the QSRV atomic group's `lock_records` epoch via
            // `put_record_field_from_ca_already_locked`). The gate
            // `Mutex` is not reentrant — use the `_already_locked`
            // processing entry.
            let _ = self
                .process_record_with_links_already_locked(record_name, &mut visited, 0)
                .await;
        }

        // Is the ORIGINATING record itself still async-pending? Its
        // wait-set membership is taken + `leave`d at its own completion
        // (sync-end, or later in `complete_async_record_inner`), so a
        // lingering `notify` on its instance means its device round-trip
        // is still in flight. This gates only the originating record's
        // PUTF clear — independent of whether downstream chain targets
        // are still pending.
        let originating_pending = {
            let rec = self.inner.records.read().await;
            if let Some(rec_arc) = rec.get(record_name) {
                rec_arc.read().await.notify.is_some()
            } else {
                false
            }
        };

        // C `recGbl.c::recGblFwdLink:302` clears `putf = FALSE` after
        // the forward-link dispatch — the marker only lives for the
        // duration of the put's processing cycle. For SYNCHRONOUS
        // completions (PACT was cleared by the time
        // `process_record_with_links` returns) clear it here. For
        // async-pending records, the clearing happens later in
        // `complete_async_record_inner` (which runs FLNK as part of
        // the completion path) so the PUTF marker survives the
        // device-write round trip.
        if !originating_pending {
            let rec = self.inner.records.read().await;
            if let Some(rec_arc) = rec.get(record_name) {
                let mut guard = rec_arc.write().await;
                if !guard.is_processing() {
                    guard.common.putf = false;
                }
            }
        }

        // CA completion gates on the WHOLE chain, not just the
        // originating record: the put-notify must not report
        // done until every FLNK/OUT target it drove — including an async
        // FLNK target that the originating record's sync cycle merely
        // kicked off — has settled. `completed()` is true iff the
        // wait-set drained to zero during this call (fully synchronous
        // chain); otherwise the receiver fires later from the last
        // chain member's `leave`.
        if notify.completed() {
            Ok(None)
        } else {
            Ok(Some(completion_rx))
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

        // Records — alias-aware (epics-base PR #336).
        if let Some(rec) = self.get_record(base).await {
            // `put_pv_no_process` is a public record-write API
            // (autosave restore). It must take the advisory write gate
            // (`dbScanLock` analogue) so an autosave restore cannot
            // land between the member writes of a QSRV atomic group or
            // a pvalink atomic scan epoch holding `lock_records`.
            // `base` is alias-resolved so an alias and its target
            // share one gate. Held until return.
            let canonical_base: String = self
                .resolve_alias(base)
                .await
                .unwrap_or_else(|| base.to_string());
            let _record_gate = self.lock_record(&canonical_base).await;

            let mut instance = rec.write().await;
            let prev_value = instance.record.get_field(&field);
            match instance.record.put_field(&field, value.clone()) {
                Ok(()) => {}
                Err(CaError::FieldNotFound(_)) => {
                    instance.put_common_field(&field, value)?;
                }
                Err(e) => return Err(e),
            }
            // Invalidate metadata cache only if the metadata-class
            // field actually changed (faac1df1).
            instance.notify_field_written_if_changed(&field, prev_value.as_ref());
            // same SPC_AS parity as `put_pv` / the CA-write
            // path — autosave-style restores writing `.ASG` at IOC
            // startup must still trigger per-client re-eval.
            if field == "ASG" {
                crate::server::access_security::notify_asg_field_changed();
            }
            return Ok(());
        }

        Err(CaError::ChannelNotFound(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::PvDatabase;
    use crate::types::EpicsValue;

    /// Regression: prior to fixing B1, `put_pv_and_post` walked only
    /// `inner.records` and returned `ChannelNotFound` for everything
    /// `add_pv`-registered. The CA gateway's monitor forwarder uses
    /// `add_pv` then expects `put_pv_and_post` to fan-out to
    /// downstream subscribers — without the simple-PV branch, every
    /// upstream event was silently dropped and the gateway delivered
    /// no monitors.
    #[tokio::test]
    async fn put_pv_and_post_handles_simple_pv() {
        let db = PvDatabase::new();
        db.add_pv("gw:test", EpicsValue::Double(0.0)).await.unwrap();

        // Should NOT return ChannelNotFound.
        db.put_pv_and_post("gw:test", EpicsValue::Double(42.0))
            .await
            .expect("simple PV put_pv_and_post must succeed");

        // Value actually landed.
        let pv = db.find_pv("gw:test").await.expect("PV exists");
        assert!(matches!(pv.get().await, EpicsValue::Double(v) if v == 42.0));
    }

    /// Regression: `get_pv`, `put_pv`, `put_pv_and_post`,
    /// and `put_pv_no_process` all bypassed `get_record` and walked
    /// `self.inner.records` directly, so alias names from epics-base
    /// PR #336 silently returned `ChannelNotFound`. A later fix closed
    /// `get_record` but the same defect was hiding in field_io.rs.
    /// All four CA-server-and-bridge entry points must accept aliases.
    #[tokio::test]
    async fn field_io_entry_points_accept_aliases() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_record("CANON", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_alias("ALT", "CANON").await.unwrap();

        // get_pv via alias
        db.put_pv("CANON.VAL", EpicsValue::Double(1.5))
            .await
            .unwrap();
        let v = db.get_pv("ALT.VAL").await.unwrap();
        assert!(matches!(v, EpicsValue::Double(x) if x == 1.5));

        // put_pv via alias
        db.put_pv("ALT.VAL", EpicsValue::Double(7.0)).await.unwrap();
        let v = db.get_pv("CANON.VAL").await.unwrap();
        assert!(matches!(v, EpicsValue::Double(x) if x == 7.0));

        // put_pv_and_post via alias
        db.put_pv_and_post("ALT.VAL", EpicsValue::Double(11.0))
            .await
            .unwrap();
        let v = db.get_pv("CANON.VAL").await.unwrap();
        assert!(matches!(v, EpicsValue::Double(x) if x == 11.0));

        // put_pv_no_process via alias
        db.put_pv_no_process("ALT.VAL", EpicsValue::Double(13.0))
            .await
            .unwrap();
        let v = db.get_pv("ALT.VAL").await.unwrap();
        assert!(matches!(v, EpicsValue::Double(x) if x == 13.0));
    }

    /// Regression: `put_record_field_from_ca` (the CA
    /// server's main put fast path) must accept aliases. Pre-fix it
    /// only consulted `inner.records` directly. Also exercises the
    /// canonical-name normalisation that protects subsequent
    /// `process_record_with_links` / `update_scan_index` calls.
    #[tokio::test]
    async fn put_record_field_from_ca_accepts_alias() {
        use crate::server::records::ai::AiRecord;

        let db = PvDatabase::new();
        db.add_record("CANON", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_alias("ALT", "CANON").await.unwrap();

        // Put VAL via the alias name.
        let _ = db
            .put_record_field_from_ca("ALT", "VAL", EpicsValue::Double(2.5))
            .await
            .expect("put via alias must succeed");

        // Read back via canonical to confirm the value landed on the
        // right record.
        let v = db.get_pv("CANON.VAL").await.unwrap();
        assert!(matches!(v, EpicsValue::Double(x) if x == 2.5));
    }

    /// Regression: a direct CA put to a record whose value field VAL is NOT
    /// `pp(TRUE)` (calc / calcout / aSub) must still fire a DBE_VALUE monitor.
    /// C `dbAccess.c::dbPut:1408-1414` posts the value field immediately
    /// unless it is `pp(TRUE)`. The port previously suppressed the immediate
    /// post for every `VAL` and — with the `should_process` gate — skipped
    /// the reprocess cycle for a non-`pp` VAL, so the operator's write fired
    /// no monitor at all. calc's VAL is not in its `pp` field set, so the
    /// immediate post is the only event that can fire.
    #[tokio::test]
    async fn ca_put_to_non_pp_val_posts_monitor() {
        use crate::server::database::db_access::DbSubscription;
        use crate::server::records::calc::CalcRecord;

        let db = PvDatabase::new();
        db.add_record("CALC1", Box::new(CalcRecord::new("0")))
            .await
            .unwrap();

        let mut sub = DbSubscription::subscribe(&db, "CALC1.VAL")
            .await
            .expect("subscribe to CALC1.VAL");

        db.put_record_field_from_ca("CALC1", "VAL", EpicsValue::Double(5.0))
            .await
            .expect("CA put to CALC1.VAL must succeed");

        let got = tokio::time::timeout(std::time::Duration::from_secs(1), sub.recv_f64())
            .await
            .expect("a DBE_VALUE monitor must fire for a direct VAL put to a non-pp record");
        assert_eq!(got, Some(5.0));
    }
}
