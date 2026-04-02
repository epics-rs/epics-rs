use std::collections::HashSet;
use std::sync::Arc;

use crate::error::{CaError, CaResult};
use crate::runtime::sync::RwLock;
use crate::server::record::RecordInstance;
use crate::types::EpicsValue;

use super::{PvDatabase, apply_timestamp};

impl PvDatabase {
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

    /// Process a record with full link handling (INP -> process -> alarms -> OUT -> FLNK).
    /// Uses visited set for cycle detection and depth limit.
    pub fn process_record_with_links<'a>(
        &'a self,
        name: &'a str,
        visited: &'a mut HashSet<String>,
        depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CaResult<()>> + Send + 'a>> {
        Box::pin(async move {
            self.process_record_with_links_inner(name, visited, depth)
                .await
        })
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
                (
                    instance.parsed_sdis.clone(),
                    instance.common.disv,
                    instance.common.diss,
                )
            };

            if let crate::server::record::ParsedLink::Db(ref link) = sdis_link {
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
                    changed_fields.push((
                        "SEVR".to_string(),
                        EpicsValue::Short(instance.common.sevr as i16),
                    ));
                    changed_fields.push((
                        "STAT".to_string(),
                        EpicsValue::Short(instance.common.stat as i16),
                    ));
                    if let Some(val) = instance.record.val() {
                        changed_fields.push(("VAL".to_string(), val));
                    }
                    let snapshot = crate::server::record::ProcessSnapshot {
                        changed_fields,
                        event_mask: crate::server::recgbl::EventMask::ALARM,
                    };
                    instance.notify_from_snapshot(&snapshot);
                }
                return Ok(());
            }
        }

        // 0.3. TSEL link: read TSE value from another record
        {
            let tsel_link = {
                let instance = rec.read().await;
                instance.parsed_tsel.clone()
            };
            if let crate::server::record::ParsedLink::Db(ref link) = tsel_link {
                let pv_name = if link.field == "VAL" {
                    link.record.clone()
                } else {
                    format!("{}.{}", link.record, link.field)
                };
                if let Ok(val) = self.get_pv(&pv_name).await {
                    let tse_val = val.to_f64().unwrap_or(0.0) as i16;
                    let mut instance = rec.write().await;
                    instance.common.tse = tse_val;
                }
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
            let is_soft = crate::server::device_support::is_soft_dtyp(&instance.common.dtyp);

            // DOL link info for output records with OMSL=CLOSED_LOOP
            let dol = match rtype {
                "ao" | "longout" | "bo" | "mbbo" | "stringout" => {
                    let omsl = instance
                        .record
                        .get_field("OMSL")
                        .and_then(|v| {
                            if let EpicsValue::Short(s) = v {
                                Some(s)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    let oif = instance
                        .record
                        .get_field("OIF")
                        .and_then(|v| {
                            if let EpicsValue::Short(s) = v {
                                Some(s)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    if omsl == 1 {
                        let dol_parsed = instance
                            .record
                            .get_field("DOL")
                            .and_then(|v| {
                                if let EpicsValue::String(s) = v {
                                    Some(s)
                                } else {
                                    None
                                }
                            })
                            .map(|s| crate::server::record::parse_link_v2(&s))
                            .unwrap_or(crate::server::record::ParsedLink::None);
                        Some((dol_parsed, oif))
                    } else {
                        None
                    }
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
                instance
                    .record
                    .multi_input_links()
                    .iter()
                    .map(|(lf, vf)| {
                        let link_str = instance
                            .record
                            .get_field(lf)
                            .and_then(|v| {
                                if let EpicsValue::String(s) = v {
                                    Some(s)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default();
                        (link_str, vf.to_string())
                    })
                    .collect()
            }; // read lock dropped
            let mut results = Vec::new();
            for (link_str, val_field) in &link_info {
                if !link_str.is_empty() {
                    let parsed = crate::server::record::parse_link_v2(link_str);
                    if let Some(value) = self.read_link_value(&parsed).await {
                        results.push((val_field.clone(), value));
                    }
                }
            }
            results
        };

        // 1.6. Sel NVL link: resolve NVL -> SELN
        let sel_nvl_value: Option<EpicsValue> = {
            let instance = rec.read().await;
            if instance.record.record_type() == "sel" {
                let nvl_str = instance
                    .record
                    .get_field("NVL")
                    .and_then(|v| {
                        if let EpicsValue::String(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                if !nvl_str.is_empty() {
                    drop(instance); // release read lock before async read
                    let parsed = crate::server::record::parse_link_v2(&nvl_str);
                    self.read_link_value(&parsed).await
                } else {
                    None
                }
            } else {
                None
            }
        };

        // 2. Lock record, apply INP/DOL, process, evaluate alarms, build snapshot
        let (snapshot, out_info, flnk_name, process_actions, put_notify_tx) = {
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

            // Apply multi-input values (INPA..INPL -> A..L)
            for (val_field, value) in &multi_input_values {
                if let Some(f) = value.to_f64() {
                    let _ = instance.record.put_field(val_field, EpicsValue::Double(f));
                }
            }

            // Apply sel NVL -> SELN
            if let Some(nvl_val) = sel_nvl_value {
                if let Some(f) = nvl_val.to_f64() {
                    let _ = instance
                        .record
                        .put_field("SELN", EpicsValue::Short(f as i16));
                }
            }

            // Device support read (input records only, not output records)
            let is_soft = instance.common.dtyp.is_empty() || instance.common.dtyp == "Soft Channel";
            let is_output = instance.record.can_device_write();
            let mut device_actions: Vec<crate::server::record::ProcessAction> = Vec::new();
            let mut device_did_compute = false;
            if !is_soft && !is_output {
                if let Some(mut dev) = instance.device.take() {
                    match dev.read(&mut *instance.record) {
                        Ok(read_outcome) => {
                            device_did_compute = read_outcome.did_compute;
                            device_actions = read_outcome.actions;
                        }
                        Err(e) => {
                            eprintln!("device read error on {}: {e}", instance.name);
                            use crate::server::recgbl::{alarm_status, rec_gbl_set_sevr};
                            rec_gbl_set_sevr(
                                &mut instance.common,
                                alarm_status::READ_ALARM,
                                crate::server::record::AlarmSeverity::Invalid,
                            );
                        }
                    }
                    instance.device = Some(dev);
                }
            }

            // Pre-process actions: execute ReadDbLink from device support and
            // record's pre_process_actions() BEFORE process() so the values
            // are immediately available. Matches C dbGetLink() semantics.
            let mut pre_actions = instance.record.pre_process_actions();
            // Also collect ReadDbLink from device actions
            let mut deferred_device_actions = Vec::new();
            for action in device_actions {
                if matches!(
                    action,
                    crate::server::record::ProcessAction::ReadDbLink { .. }
                ) {
                    pre_actions.push(action);
                } else {
                    deferred_device_actions.push(action);
                }
            }
            if !pre_actions.is_empty() {
                let rec_name = instance.name.clone();
                drop(instance);
                self.execute_read_db_links(&rec_name, &rec, &pre_actions)
                    .await;
                instance = rec.write().await;
            }

            // Note: C EPICS LCNT prevents reentrant processing of the same
            // record within a single processing chain. In Rust, this is handled
            // by the `visited` HashSet (cycle detection) and the `processing`
            // AtomicBool guard. LCNT is not needed as a separate mechanism
            // because async processing with visited sets already prevents
            // the runaway loops that LCNT guards against in C.

            // Tell the record whether device support already computed.
            // Records that override set_device_did_compute() use this to
            // skip their built-in computation (e.g., epid PID).
            if device_did_compute {
                instance.record.set_device_did_compute(true);
            }

            // TPRO: trace processing (C EPICS dbProcess prints context when TPRO>0)
            if instance.common.tpro {
                eprintln!(
                    "[TPRO] {}: process (SCAN={:?}, PACT={})",
                    instance.name,
                    instance.common.scan,
                    instance
                        .processing
                        .load(std::sync::atomic::Ordering::Relaxed)
                );
            }

            // Process
            let mut outcome = instance.record.process()?;
            // Merge deferred device actions into process outcome actions
            outcome.actions.extend(deferred_device_actions);
            let process_result = outcome.result;
            let process_actions = outcome.actions;

            if process_result == crate::server::record::RecordProcessResult::AsyncPending {
                // PACT stays set; skip alarm/timestamp/snapshot/OUT/FLNK.
                // But still execute any actions (e.g., ReprocessAfter for delayed re-entry).
                let rec_name = instance.name.clone();
                drop(instance);
                self.execute_process_actions(&rec_name, &rec, process_actions, visited, depth)
                    .await;
                return Ok(());
            }
            if let crate::server::record::RecordProcessResult::AsyncPendingNotify(fields) =
                process_result
            {
                // Intermediate notification (e.g. DMOV=0 at move start).
                // Execute device write first so the move command reaches the driver,
                // then flush DMOV=0 etc. to monitors.
                if !is_soft {
                    if let Some(mut dev) = instance.device.take() {
                        let _ = dev.write(&mut *instance.record);
                        instance.device = Some(dev);
                    }
                }
                apply_timestamp(&mut instance.common, is_soft);
                // Filter out fields that haven't changed, update MLST/last_posted.
                let mut changed_fields = Vec::new();
                for (name, val) in fields {
                    let changed = match instance.last_posted.get(&name) {
                        Some(prev) => prev != &val,
                        None => true,
                    };
                    if changed {
                        if name == "VAL" {
                            if let Some(f) = val.to_f64() {
                                if instance
                                    .record
                                    .put_field("MLST", EpicsValue::Double(f))
                                    .is_err()
                                {
                                    instance.common.mlst = Some(f);
                                }
                            }
                        }
                        instance.last_posted.insert(name.clone(), val.clone());
                        changed_fields.push((name, val));
                    }
                }
                let event_mask = if changed_fields.is_empty() {
                    crate::server::recgbl::EventMask::NONE
                } else {
                    crate::server::recgbl::EventMask::VALUE
                        | crate::server::recgbl::EventMask::ALARM
                };
                let snapshot = crate::server::record::ProcessSnapshot {
                    changed_fields,
                    event_mask,
                };
                let rec_clone = rec.clone();
                drop(instance);
                {
                    let inst = rec_clone.read().await;
                    inst.notify_from_snapshot(&snapshot);
                }
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
                    rec_gbl_set_sevr(
                        &mut instance.common,
                        stat,
                        crate::server::record::AlarmSeverity::from_u16(sevr),
                    );
                }
                if let Some(ts) = dev_ts {
                    instance.common.time = ts;
                }
            }

            // Transfer nsta/nsev -> sevr/stat, detect alarm change
            let alarm_result = crate::server::recgbl::rec_gbl_reset_alarms(&mut instance.common);

            // Apply timestamp based on TSE
            apply_timestamp(&mut instance.common, is_soft);
            if instance.record.clears_udf() {
                instance.common.udf = false;
            }

            // IVOA check for output records with INVALID alarm
            let skip_out = if instance.common.sevr == crate::server::record::AlarmSeverity::Invalid
            {
                let ivoa = instance
                    .record
                    .get_field("IVOA")
                    .and_then(|v| {
                        if let EpicsValue::Short(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
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

            // OUT stage: soft channel -> link put, non-soft -> device.write()
            // Must run BEFORE check_deadband_ext so MLST is not prematurely
            // updated for async writes that return early.
            let can_dev_write = instance.record.can_device_write();
            let is_soft_out =
                instance.common.dtyp.is_empty() || instance.common.dtyp == "Soft Channel";
            let record_should_output = instance.record.should_output();
            let out_info = if skip_out {
                None
            } else if !can_dev_write {
                // Non-output records (calcout, etc.) may still have a soft OUT link.
                // Write OVAL to OUT when the record says should_output().
                if record_should_output {
                    if let crate::server::record::ParsedLink::Db(ref link) = instance.parsed_out {
                        let out_val = instance
                            .record
                            .get_field("OVAL")
                            .or_else(|| instance.record.val());
                        out_val.map(|v| (link.clone(), v))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else if is_soft_out {
                if let crate::server::record::ParsedLink::Db(ref link) = instance.parsed_out {
                    let out_val = instance
                        .record
                        .get_field("OVAL")
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
                            // Async write submitted -- set PACT, return early.
                            // complete_async_record will handle deadband, snapshot,
                            // notification, and FLNK when the write completes.
                            instance
                                .processing
                                .store(true, std::sync::atomic::Ordering::Release);
                            instance.device = Some(dev);
                            let rec_name = instance.name.clone();
                            let timeout = std::time::Duration::from_secs(5);
                            let db = self.clone();
                            tokio::spawn(async move {
                                let _ =
                                    tokio::task::spawn_blocking(move || completion.wait(timeout))
                                        .await;
                                let _ = db.complete_async_record(&rec_name).await;
                            });
                            return Ok(());
                        }
                        Ok(None) => {
                            // No async support -- fall back to synchronous write
                            if let Err(e) = dev.write(&mut *instance.record) {
                                eprintln!("device write error on {}: {e}", instance.name);
                                instance.common.stat =
                                    crate::server::recgbl::alarm_status::WRITE_ALARM;
                                instance.common.sevr =
                                    crate::server::record::AlarmSeverity::Invalid;
                            }
                        }
                        Err(e) => {
                            eprintln!("device write_begin error on {}: {e}", instance.name);
                            instance.common.stat = crate::server::recgbl::alarm_status::WRITE_ALARM;
                            instance.common.sevr = crate::server::record::AlarmSeverity::Invalid;
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

            // Build snapshot
            let mut changed_fields = Vec::new();
            if include_val {
                if let Some(val) = instance.record.val() {
                    changed_fields.push(("VAL".to_string(), val));
                }
            }
            // Add subscribed fields that actually changed since last notification.
            let mut sub_updates: Vec<(String, EpicsValue)> = Vec::new();
            for (field, subs) in &instance.subscribers {
                if !subs.is_empty()
                    && field != "VAL"
                    && field != "SEVR"
                    && field != "STAT"
                    && field != "UDF"
                {
                    if let Some(val) = instance.resolve_field(field) {
                        let changed = match instance.last_posted.get(field) {
                            Some(prev) => prev != &val,
                            None => true,
                        };
                        if changed {
                            sub_updates.push((field.clone(), val));
                        }
                    }
                }
            }
            if !sub_updates.is_empty() {
                for (field, val) in &sub_updates {
                    instance.last_posted.insert(field.clone(), val.clone());
                }
                changed_fields.extend(sub_updates);
                event_mask |= crate::server::recgbl::EventMask::VALUE;
            }
            if alarm_result.alarm_changed {
                changed_fields.push((
                    "SEVR".to_string(),
                    EpicsValue::Short(instance.common.sevr as i16),
                ));
                changed_fields.push((
                    "STAT".to_string(),
                    EpicsValue::Short(instance.common.stat as i16),
                ));
            }
            if !event_mask.is_empty() {
                changed_fields.push((
                    "UDF".to_string(),
                    EpicsValue::Char(if instance.common.udf { 1 } else { 0 }),
                ));
            }
            let snapshot = crate::server::record::ProcessSnapshot {
                changed_fields,
                event_mask,
            };

            let flnk_name = if instance.record.should_fire_forward_link() {
                if let crate::server::record::ParsedLink::Db(ref l) = instance.parsed_flnk {
                    Some(l.record.clone())
                } else {
                    None
                }
            } else {
                None
            };

            let put_notify_tx = if instance.record.is_put_complete() {
                instance.put_notify_tx.take()
            } else {
                None
            };

            (
                snapshot,
                out_info,
                flnk_name,
                process_actions,
                put_notify_tx,
            )
        };

        // 3. Notify subscribers (outside lock)
        {
            let instance = rec.read().await;
            instance.notify_from_snapshot(&snapshot);
        }

        // 4. OUT link
        if let Some((link, out_val)) = out_info {
            self.write_db_link_value(&link, out_val, visited, depth)
                .await;
        }

        // 4.5. Multi-output dispatch (fanout/dfanout/seq)
        self.dispatch_multi_output(&rec, visited, depth).await;

        // 4.6. Generic multi-output links (transform OUTA..OUTP -> A..P)
        {
            let multi_out = {
                let instance = rec.read().await;
                let links = instance.record.multi_output_links();
                if links.is_empty() {
                    None
                } else {
                    let mut pairs = Vec::new();
                    for &(link_field, val_field) in links {
                        let link_str = instance
                            .record
                            .get_field(link_field)
                            .and_then(|v| {
                                if let EpicsValue::String(s) = v {
                                    Some(s)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default();
                        if link_str.is_empty() {
                            continue;
                        }
                        if let Some(val) = instance.record.get_field(val_field) {
                            pairs.push((link_str, val));
                        }
                    }
                    if pairs.is_empty() { None } else { Some(pairs) }
                }
            };
            if let Some(pairs) = multi_out {
                for (link_str, val) in pairs {
                    let parsed = crate::server::record::parse_link_v2(&link_str);
                    if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                        self.write_db_link_value(db, val, visited, depth).await;
                    }
                }
            }
        }

        // 5. FLNK
        if let Some(flnk) = flnk_name {
            let _ = self
                .process_record_with_links(&flnk, visited, depth + 1)
                .await;
        }

        // 6. CP link targets -- process records that have CP input links from this record
        {
            let cp_targets = self.get_cp_targets(name).await;
            for target in cp_targets {
                if !visited.contains(&target) {
                    let _ = self
                        .process_record_with_links(&target, visited, depth + 1)
                        .await;
                }
            }
        }

        if let Some(tx) = put_notify_tx {
            // Let any immediately-triggered I/O Intr tasks run before
            // completing WRITE_NOTIFY, so dependent reads observe fresh state.
            tokio::task::yield_now().await;
            let _ = tx.send(());
        }

        // 7. RPRO: if reprocess requested, clear flag and reprocess
        {
            let needs_rpro = {
                let mut instance = rec.write().await;
                if instance.common.rpro {
                    instance.common.rpro = false;
                    true
                } else {
                    false
                }
            };
            if needs_rpro {
                visited.remove(name);
                let _ = self
                    .process_record_with_links(name, visited, depth + 1)
                    .await;
            }
        }

        // 8. Execute ProcessActions from the record's process() outcome.
        // This handles WriteDbLink, ReadDbLink, and ReprocessAfter actions.
        self.execute_process_actions(name, &rec, process_actions, visited, depth)
            .await;

        Ok(())
    }

    /// Execute ReadDbLink actions before process().
    /// Reads linked PV values and writes them into record fields via put_field_internal.
    async fn execute_read_db_links(
        &self,
        _record_name: &str,
        rec: &Arc<crate::runtime::sync::RwLock<RecordInstance>>,
        actions: &[crate::server::record::ProcessAction],
    ) {
        use crate::server::record::ProcessAction;
        for action in actions {
            if let ProcessAction::ReadDbLink {
                link_field,
                target_field,
            } = action
            {
                let link_str = {
                    let instance = rec.read().await;
                    instance
                        .record
                        .get_field(link_field)
                        .and_then(|v| {
                            if let EpicsValue::String(s) = v {
                                Some(s)
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default()
                };
                if link_str.is_empty() {
                    continue;
                }
                let parsed = crate::server::record::parse_link_v2(&link_str);
                if let Some(value) = self.read_link_value(&parsed).await {
                    let mut instance = rec.write().await;
                    let _ = instance.record.put_field_internal(target_field, value);
                }
            }
        }
    }

    /// Execute ProcessActions returned by a record's process() call.
    ///
    /// Actions are executed in order:
    /// - ReadDbLink: reads a linked PV value and writes it into a record field
    ///   (bypasses read-only checks via put_field_internal)
    /// - WriteDbLink: writes a value to a linked PV
    /// - ReprocessAfter: schedules a delayed re-process via tokio::spawn
    async fn execute_process_actions(
        &self,
        record_name: &str,
        rec: &Arc<crate::runtime::sync::RwLock<RecordInstance>>,
        actions: Vec<crate::server::record::ProcessAction>,
        visited: &mut HashSet<String>,
        depth: usize,
    ) {
        use crate::server::record::ProcessAction;

        for action in actions {
            match action {
                ProcessAction::ReadDbLink {
                    link_field,
                    target_field,
                } => {
                    // 1. Get the link string from the record
                    let link_str = {
                        let instance = rec.read().await;
                        instance
                            .record
                            .get_field(link_field)
                            .and_then(|v| {
                                if let EpicsValue::String(s) = v {
                                    Some(s)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default()
                    };
                    if link_str.is_empty() {
                        continue;
                    }
                    // 2. Parse and read the linked PV
                    let parsed = crate::server::record::parse_link_v2(&link_str);
                    if let Some(value) = self.read_link_value(&parsed).await {
                        // 3. Write into the record field (internal put bypasses read-only)
                        let mut instance = rec.write().await;
                        let _ = instance.record.put_field_internal(target_field, value);
                    }
                }
                ProcessAction::WriteDbLink { link_field, value } => {
                    // 1. Get the link string from the record
                    let link_str = {
                        let instance = rec.read().await;
                        instance
                            .record
                            .get_field(link_field)
                            .and_then(|v| {
                                if let EpicsValue::String(s) = v {
                                    Some(s)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default()
                    };
                    if link_str.is_empty() {
                        continue;
                    }
                    // 2. Parse and write to the linked PV
                    let parsed = crate::server::record::parse_link_v2(&link_str);
                    if let crate::server::record::ParsedLink::Db(ref db_link) = parsed {
                        self.write_db_link_value(db_link, value, visited, depth)
                            .await;
                    }
                }
                ProcessAction::DeviceCommand { command, ref args } => {
                    let mut instance = rec.write().await;
                    if let Some(mut dev) = instance.device.take() {
                        let _ = dev.handle_command(&mut *instance.record, command, args);
                        instance.device = Some(dev);
                    }
                }
                ProcessAction::ReprocessAfter(delay) => {
                    // Use generation counter for timer cancellation.
                    // Bump generation now; the spawned task only fires if
                    // the generation hasn't been bumped again (i.e., no newer
                    // timer replaced this one).
                    let (gen_counter, gen_val) = {
                        let instance = rec.read().await;
                        let val = instance
                            .reprocess_generation
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        (instance.reprocess_generation.clone(), val)
                    };
                    let db = self.clone();
                    let rec_name = record_name.to_string();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        // Only fire if no newer timer has been scheduled
                        let current = gen_counter.load(std::sync::atomic::Ordering::Relaxed);
                        if current == gen_val {
                            let mut visited = HashSet::new();
                            let _ = db
                                .process_record_with_links(&rec_name, &mut visited, 0)
                                .await;
                        }
                    });
                }
            }
        }
    }

    /// Complete an asynchronous record's post-process steps.
    /// Call after device support signals completion (clears PACT, runs alarms, snapshot, OUT, FLNK).
    pub fn complete_async_record<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CaResult<()>> + Send + 'a>> {
        Box::pin(async move {
            let mut visited = HashSet::new();
            self.complete_async_record_inner(name, &mut visited, 0)
                .await
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
            records
                .get(name)
                .cloned()
                .ok_or_else(|| CaError::ChannelNotFound(name.to_string()))?
        };

        let (snapshot, out_info, flnk_name, put_notify_tx) = {
            let mut instance = rec.write().await;

            // Evaluate alarms
            instance.evaluate_alarms();

            let is_soft = instance.common.dtyp.is_empty() || instance.common.dtyp == "Soft Channel";

            // Device support alarm/timestamp override
            if !is_soft {
                let (dev_alarm, dev_ts) = if let Some(ref dev) = instance.device {
                    (dev.last_alarm(), dev.last_timestamp())
                } else {
                    (None, None)
                };
                if let Some((stat, sevr)) = dev_alarm {
                    crate::server::recgbl::rec_gbl_set_sevr(
                        &mut instance.common,
                        stat,
                        crate::server::record::AlarmSeverity::from_u16(sevr),
                    );
                }
                if let Some(ts) = dev_ts {
                    instance.common.time = ts;
                }
            }

            let alarm_result = crate::server::recgbl::rec_gbl_reset_alarms(&mut instance.common);

            apply_timestamp(&mut instance.common, is_soft);
            if instance.record.clears_udf() {
                instance.common.udf = false;
            }

            // Clear PACT
            instance
                .processing
                .store(false, std::sync::atomic::Ordering::Release);

            let put_notify_tx = instance.put_notify_tx.take();

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

            let mut changed_fields = Vec::new();
            if include_val {
                if let Some(val) = instance.record.val() {
                    changed_fields.push(("VAL".to_string(), val));
                }
            }
            changed_fields.push((
                "SEVR".to_string(),
                EpicsValue::Short(instance.common.sevr as i16),
            ));
            changed_fields.push((
                "STAT".to_string(),
                EpicsValue::Short(instance.common.stat as i16),
            ));
            changed_fields.push((
                "UDF".to_string(),
                EpicsValue::Char(if instance.common.udf { 1 } else { 0 }),
            ));
            for (field, subs) in &instance.subscribers {
                if !subs.is_empty()
                    && field != "VAL"
                    && field != "SEVR"
                    && field != "STAT"
                    && field != "UDF"
                {
                    if let Some(val) = instance.resolve_field(field) {
                        changed_fields.push((field.clone(), val));
                    }
                }
            }
            let snapshot = crate::server::record::ProcessSnapshot {
                changed_fields,
                event_mask,
            };

            // IVOA check
            let skip_out = if instance.common.sevr == crate::server::record::AlarmSeverity::Invalid
            {
                let ivoa = instance
                    .record
                    .get_field("IVOA")
                    .and_then(|v| {
                        if let EpicsValue::Short(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    })
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
            let is_soft_out =
                instance.common.dtyp.is_empty() || instance.common.dtyp == "Soft Channel";
            let record_should_output = instance.record.should_output();
            let out_info = if skip_out {
                None
            } else if !can_dev_write {
                // Non-output records (calcout, etc.) with soft OUT link
                if record_should_output {
                    if let crate::server::record::ParsedLink::Db(ref link) = instance.parsed_out {
                        let out_val = instance
                            .record
                            .get_field("OVAL")
                            .or_else(|| instance.record.val());
                        out_val.map(|v| (link.clone(), v))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else if is_soft_out {
                if let crate::server::record::ParsedLink::Db(ref link) = instance.parsed_out {
                    let out_val = instance
                        .record
                        .get_field("OVAL")
                        .or_else(|| instance.record.val());
                    out_val.map(|v| (link.clone(), v))
                } else {
                    None
                }
            } else {
                // Non-soft output: the async device write already completed
                // (that's why we're in complete_async_record). Don't re-do
                // write_begin -- it would start another async cycle.
                None
            };

            let flnk_name = if instance.record.should_fire_forward_link() {
                if let crate::server::record::ParsedLink::Db(ref l) = instance.parsed_flnk {
                    Some(l.record.clone())
                } else {
                    None
                }
            } else {
                None
            };

            (snapshot, out_info, flnk_name, put_notify_tx)
        };

        // Notify subscribers
        {
            let instance = rec.read().await;
            instance.notify_from_snapshot(&snapshot);
        }

        // OUT link
        if let Some((link, out_val)) = out_info {
            self.write_db_link_value(&link, out_val, visited, depth)
                .await;
        }

        // Multi-output dispatch (fanout/dfanout/seq/sseq)
        self.dispatch_multi_output(&rec, visited, depth).await;

        // Generic multi-output links (transform OUTA..OUTP -> A..P)
        {
            let multi_out = {
                let instance = rec.read().await;
                let links = instance.record.multi_output_links();
                if links.is_empty() {
                    None
                } else {
                    let mut pairs = Vec::new();
                    for &(link_field, val_field) in links {
                        let link_str = instance
                            .record
                            .get_field(link_field)
                            .and_then(|v| {
                                if let EpicsValue::String(s) = v {
                                    Some(s)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default();
                        if link_str.is_empty() {
                            continue;
                        }
                        if let Some(val) = instance.record.get_field(val_field) {
                            pairs.push((link_str, val));
                        }
                    }
                    if pairs.is_empty() { None } else { Some(pairs) }
                }
            };
            if let Some(pairs) = multi_out {
                for (link_str, val) in pairs {
                    let parsed = crate::server::record::parse_link_v2(&link_str);
                    if let crate::server::record::ParsedLink::Db(ref db) = parsed {
                        self.write_db_link_value(db, val, visited, depth).await;
                    }
                }
            }
        }

        // FLNK
        if let Some(flnk) = flnk_name {
            let _ = self
                .process_record_with_links(&flnk, visited, depth + 1)
                .await;
        }

        // CP link targets
        {
            let cp_targets = self.get_cp_targets(name).await;
            for target in cp_targets {
                if !visited.contains(&target) {
                    let _ = self
                        .process_record_with_links(&target, visited, depth + 1)
                        .await;
                }
            }
        }

        if let Some(tx) = put_notify_tx {
            // Let any immediately-triggered I/O Intr tasks run before
            // completing WRITE_NOTIFY, so dependent reads observe fresh state.
            tokio::task::yield_now().await;
            let _ = tx.send(());
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

            let siml = instance
                .record
                .get_field("SIML")
                .and_then(|v| {
                    if let EpicsValue::String(s) = v {
                        Some(s)
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let siol = instance
                .record
                .get_field("SIOL")
                .and_then(|v| {
                    if let EpicsValue::String(s) = v {
                        Some(s)
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let sims = instance
                .record
                .get_field("SIMS")
                .and_then(|v| {
                    if let EpicsValue::Short(s) = v {
                        Some(s)
                    } else {
                        None
                    }
                })
                .unwrap_or(0);

            if siml.is_empty() && siol.is_empty() {
                return None; // No simulation configured
            }

            let siml_parsed = crate::server::record::parse_link_v2(&siml);
            let siol_parsed = crate::server::record::parse_link_v2(&siol);

            (siml_parsed, siol_parsed, sims, rtype, is_input)
        };

        // Read SIML -> update SIMM
        if let crate::server::record::ParsedLink::Db(ref link) = siml_link {
            let pv_name = if link.field == "VAL" {
                link.record.clone()
            } else {
                format!("{}.{}", link.record, link.field)
            };
            if let Ok(val) = self.get_pv(&pv_name).await {
                let simm_val = val.to_f64().unwrap_or(0.0) as i16;
                let mut instance = rec.write().await;
                let _ = instance
                    .record
                    .put_field("SIMM", EpicsValue::Short(simm_val));
            }
        }

        // Check SIMM
        let simm = {
            let instance = rec.read().await;
            instance
                .record
                .get_field("SIMM")
                .and_then(|v| {
                    if let EpicsValue::Short(s) = v {
                        Some(s)
                    } else {
                        None
                    }
                })
                .unwrap_or(0)
        };

        if simm == 0 {
            return None; // NO simulation, proceed normally
        }

        // SIMM=YES(1): handle simulation
        if let crate::server::record::ParsedLink::Db(ref link) = siol_link {
            let pv_name = if link.field == "VAL" {
                link.record.clone()
            } else {
                format!("{}.{}", link.record, link.field)
            };

            if is_input {
                // Input record: read from SIOL -> set VAL directly (skip conversion)
                if let Ok(siol_val) = self.get_pv(&pv_name).await {
                    let mut instance = rec.write().await;
                    let _ = instance.record.set_val(siol_val);
                    apply_timestamp(&mut instance.common, true);
                    instance.common.udf = false;

                    // Set simulation alarm
                    let sev = crate::server::record::AlarmSeverity::from_u16(sims as u16);
                    if sev != crate::server::record::AlarmSeverity::NoAlarm {
                        instance.common.sevr = sev;
                        instance.common.stat = crate::server::recgbl::alarm_status::SIMM_ALARM;
                    }

                    // Build snapshot and notify
                    let mut changed_fields = Vec::new();
                    if let Some(val) = instance.record.val() {
                        changed_fields.push(("VAL".to_string(), val));
                    }
                    changed_fields.push((
                        "SEVR".to_string(),
                        EpicsValue::Short(instance.common.sevr as i16),
                    ));
                    changed_fields.push((
                        "STAT".to_string(),
                        EpicsValue::Short(instance.common.stat as i16),
                    ));
                    let snapshot = crate::server::record::ProcessSnapshot {
                        changed_fields,
                        event_mask: crate::server::recgbl::EventMask::VALUE
                            | crate::server::recgbl::EventMask::ALARM,
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
                apply_timestamp(&mut instance.common, true);
                instance.common.udf = false;

                let sev = crate::server::record::AlarmSeverity::from_u16(sims as u16);
                if sev != crate::server::record::AlarmSeverity::NoAlarm {
                    instance.common.sevr = sev;
                    instance.common.stat = crate::server::recgbl::alarm_status::SIMM_ALARM;
                }

                // Notify subscribers of simulation output
                let mut changed_fields = Vec::new();
                if let Some(val) = instance.record.val() {
                    changed_fields.push(("VAL".to_string(), val));
                }
                changed_fields.push((
                    "SEVR".to_string(),
                    EpicsValue::Short(instance.common.sevr as i16),
                ));
                changed_fields.push((
                    "STAT".to_string(),
                    EpicsValue::Short(instance.common.stat as i16),
                ));
                let snapshot = crate::server::record::ProcessSnapshot {
                    changed_fields,
                    event_mask: crate::server::recgbl::EventMask::VALUE
                        | crate::server::recgbl::EventMask::ALARM,
                };
                instance.notify_from_snapshot(&snapshot);
            }
        }

        Some(Ok(()))
    }
}
