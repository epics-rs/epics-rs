use std::collections::HashSet;
use std::sync::Arc;

use crate::error::{CaError, CaResult};
use crate::runtime::sync::RwLock;
use crate::server::record::RecordInstance;
use crate::types::EpicsValue;

use super::{PvDatabase, apply_timestamp};

impl PvDatabase {
    /// Process a record by name (process_local + notify).
    /// Alias-aware (epics-base PR #336).
    pub async fn process_record(&self, name: &str) -> CaResult<()> {
        let rec = self.get_record(name).await;

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

        // Normalise to the canonical record name once at entry — both
        // for cycle-detection (`visited` would otherwise treat alias
        // and canonical as distinct entries) and for the records-map
        // lookup below. Mirrors epics-base PR #336.
        let canonical_owned;
        let name: &str = if let Some(target) = self.resolve_alias(name).await {
            canonical_owned = target;
            &canonical_owned
        } else {
            name
        };

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
                // Reset nsta/nsev to prevent stale alarm from bleeding into next cycle
                instance.common.nsta = 0;
                instance.common.nsev = crate::server::record::AlarmSeverity::NoAlarm;
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
                "ao" | "longout" | "int64out" | "bo" | "mbbo" | "mbboDirect" | "stringout"
                | "lso" => {
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

        // epics-base PR #d0cf47c: single-INP MS-class link must also
        // propagate the source record's STAT/SEVR/AMSG just like the
        // multi-input fetch loop below does. Previously the INPA..L
        // path (calc/sub/aSub/sel) propagated alarms but plain single
        // INP (ai/bi/longin/mbbi/stringin) silently dropped them —
        // downstream MSS readers saw NoAlarm even when the source was
        // INVALID. Only fires for soft-channel records: hardware-driver
        // alarms travel through device-support's own last_alarm path.
        let inp_link_alarm: Option<(
            crate::server::record::MonitorSwitch,
            super::links::LinkAlarm,
        )> = if is_soft {
            if let crate::server::record::ParsedLink::Db(ref db) = inp_parsed {
                let (_v, alarm) = self.read_link_with_alarm(&inp_parsed).await;
                alarm.map(|a| (db.monitor_switch, a))
            } else {
                None
            }
        } else {
            None
        };

        // Read DOL value
        let dol_value = if let Some((ref dol_parsed, _oif)) = dol_info {
            self.read_link_value(dol_parsed).await
        } else {
            None
        };

        // 1.5. Multi-input link fetch (calc/calcout/sel/sub)
        // Also collect alarm info from source records for MS/NMS propagation.
        let multi_input_values: Vec<(String, EpicsValue)>;
        let mut link_alarms: Vec<(
            crate::server::record::MonitorSwitch,
            super::links::LinkAlarm,
        )> = Vec::new();
        {
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
                    let (value, alarm) = self.read_link_with_alarm(&parsed).await;
                    if let Some(value) = value {
                        results.push((val_field.clone(), value));
                    }
                    if let (Some(alarm), crate::server::record::ParsedLink::Db(db)) =
                        (alarm, &parsed)
                    {
                        link_alarms.push((db.monitor_switch, alarm));
                    }
                }
            }
            multi_input_values = results;
        }
        // PR #d0cf47c continued: feed the INP alarm (if any) into the
        // same `link_alarms` list the lock-section iterates over. Order
        // doesn't matter — `rec_gbl_set_sevr_msg` takes the maximum
        // severity across all sources.
        if let Some(pair) = inp_link_alarm {
            link_alarms.push(pair);
        }

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
        let (snapshot, out_info, flnk_name, process_actions) = {
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

            // Apply INP value. "Soft Channel" sets VAL directly
            // (C `read_xxx` return 2, skip RVAL→VAL conversion).
            // "Raw Soft Channel" routes the value into RVAL and lets
            // the record's RVAL→VAL convert run (epics-base
            // f2fe9d12: devBiSoftRaw applies MASK after the read).
            // Records opt into the raw path via
            // `Record::accepts_raw_soft_input` so DTYPs on records
            // that haven't wired raw soft channel stay on the legacy
            // VAL-direct path.
            let is_raw_soft = instance.common.dtyp == "Raw Soft Channel"
                && instance.record.accepts_raw_soft_input();
            let soft_inp_applied = inp_value.is_some() && !is_raw_soft;
            if let Some(inp_val) = inp_value {
                if is_raw_soft {
                    let _ = instance.record.apply_raw_input(inp_val);
                } else {
                    let _ = instance.record.set_val(inp_val);
                }
            } else if is_soft
                && matches!(
                    inp_parsed,
                    crate::server::record::ParsedLink::Db(_)
                        | crate::server::record::ParsedLink::Ca(_)
                        | crate::server::record::ParsedLink::Pva(_)
                )
            {
                // epics-base PR #4737901: soft-channel `read_xxx` must
                // surface link-read failures via the alarm tree, not
                // silently succeed. When the INP link is a real
                // Db/Ca/Pva link (i.e. operator expected a value) and
                // the read returned None, attach LINK_ALARM/INVALID
                // so downstream consumers can react. ParsedLink::None
                // and Constant don't fall into this branch — the
                // former is "no link configured", the latter has its
                // own None-as-no-value semantics.
                use crate::server::recgbl::{alarm_status, rec_gbl_set_sevr};
                rec_gbl_set_sevr(
                    &mut instance.common,
                    alarm_status::LINK_ALARM,
                    crate::server::record::AlarmSeverity::Invalid,
                );
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
            let mut device_did_compute = soft_inp_applied && is_soft;
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
            // skip their built-in computation (e.g., ai skips RVAL->VAL).
            // Note: field_io.rs may have already called set_device_did_compute(true)
            // for CA puts to VAL. We only set true here, never reset to false.
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
                                instance.put_coerced("MLST", f);
                                instance.common.mlst = Some(f);
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

            // MS/NMS alarm propagation from input links. Mirrors
            // epics-base PR #568: alarm message string (`amsg`)
            // travels alongside stat/sevr through MS-class links.
            for (ms, alarm) in &link_alarms {
                use crate::server::recgbl::rec_gbl_set_sevr_msg;
                use crate::server::record::MonitorSwitch;
                match ms {
                    MonitorSwitch::Maximize | MonitorSwitch::MaximizeStatus => {
                        rec_gbl_set_sevr_msg(
                            &mut instance.common,
                            alarm.stat,
                            alarm.sevr,
                            alarm.amsg.clone(),
                        );
                    }
                    MonitorSwitch::MaximizeIfInvalid => {
                        if alarm.sevr == crate::server::record::AlarmSeverity::Invalid {
                            rec_gbl_set_sevr_msg(
                                &mut instance.common,
                                alarm.stat,
                                alarm.sevr,
                                alarm.amsg.clone(),
                            );
                        }
                    }
                    MonitorSwitch::NoMaximize => {} // NMS: do not propagate
                }
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
                        // Set output to IVOV. Each record type knows
                        // which field its OUT writeback consumes — see
                        // [`Record::apply_invalid_output_value`]. The
                        // pre-Round-30C path special-cased `calcout`
                        // (OVAL) and fell back to `set_val` (VAL) for
                        // every other record. That hid a real bug:
                        // ao/lso/bo/mbbo/busy left their OVAL/RVAL
                        // staging field stale, so the OUT writeback —
                        // which reads `OVAL.or(VAL)` — sent the
                        // pre-IVOA value to the linked record. Per-type
                        // overrides now apply IVOV to the field that
                        // matches the C convention.
                        if let Some(ivov) = instance.record.get_field("IVOV") {
                            let _ = instance.record.apply_invalid_output_value(ivov);
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
                        let oval = instance.record.get_field("OVAL");
                        let val = instance.record.val();
                        let out_val = oval.or(val);
                        out_val.map(|v| (link.clone(), v))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else if is_soft_out {
                if !record_should_output {
                    // epics-base 7.0.8 OOPT: gate the soft OUT-link
                    // write on the record's `should_output()`. For
                    // longout/calcout with OOPT != 0 this lets a
                    // condition-not-met cycle silently skip the link
                    // write without disturbing alarms / monitors.
                    None
                } else if let crate::server::record::ParsedLink::Db(ref link) = instance.parsed_out
                {
                    let out_val = instance
                        .record
                        .get_field("OVAL")
                        .or_else(|| instance.record.val());
                    out_val.map(|v| (link.clone(), v))
                } else {
                    None
                }
            } else if !record_should_output {
                // OOPT gating for hardware outputs (longout DTYP=...).
                // Skip the device write when the OOPT predicate is
                // not satisfied; the record's val/timestamp/snapshot
                // path still runs so monitor consumers see the value
                // change even on a non-output cycle.
                None
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
                            } else {
                                // OOPT 7.0.8: notify the record so it can
                                // latch transition state (e.g. longout.pval)
                                // for the next cycle.
                                instance.record.on_output_complete();
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

            let (include_val, include_archive) = if instance.record.uses_monitor_deadband() {
                instance.check_deadband_ext()
            } else {
                // Binary records (bi/bo/busy/mbbi/mbbo): always post monitors
                (true, true)
            };
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

            // Fire deferred put_notify completion when the record reports
            // that async work is done (e.g. motor: DMOV=1).
            if instance.put_notify_tx.is_some() && instance.record.is_put_complete() {
                if let Some(tx) = instance.put_notify_tx.take() {
                    let _ = tx.send(());
                }
            }

            (snapshot, out_info, flnk_name, process_actions)
        };

        // 3. Notify subscribers (outside lock)
        {
            let instance = rec.read().await;
            instance.notify_from_snapshot(&snapshot);
        }

        // 4. OUT link
        if let Some((ref link, ref out_val)) = out_info {
            self.write_db_link_value(link, out_val.clone(), visited, depth)
                .await;
            // OOPT 7.0.8: latch the record's post-output state so the
            // next cycle's `should_output` sees the right pval.
            {
                let mut instance = rec.write().await;
                instance.record.on_output_complete();
            }
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

        // 5. FLNK -- only process if target is Passive (like C dbScanFwdLink)
        if let Some(ref flnk) = flnk_name {
            let is_passive = if let Some(rec) = self.get_record(flnk).await {
                rec.read().await.common.scan == crate::server::record::ScanType::Passive
            } else {
                false
            };
            if is_passive {
                let _ = self
                    .process_record_with_links(flnk, visited, depth + 1)
                    .await;
            }
        }

        // 6. CP link targets -- process records that have CP input links from this record
        self.dispatch_cp_targets(name, visited, depth).await;

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
                    // 1. Get the link string (record fields → common fields)
                    let link_str = {
                        let instance = rec.read().await;
                        instance
                            .resolve_field(link_field)
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
        // Alias-aware entry — same pattern as
        // `process_record_with_links_inner`. `name` may arrive as an
        // alias from an async device-support callback that captured
        // the original record name; normalise to canonical so the
        // records-map lookup, the `visited` cycle set, and downstream
        // FLNK/OUT dispatches all see the same canonical name.
        let canonical_owned;
        let name: &str = if let Some(target) = self.resolve_alias(name).await {
            canonical_owned = target;
            &canonical_owned
        } else {
            name
        };

        let rec = {
            let records = self.inner.records.read().await;
            records
                .get(name)
                .cloned()
                .ok_or_else(|| CaError::ChannelNotFound(name.to_string()))?
        };

        let (snapshot, out_info, flnk_name) = {
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

            // Fire put_notify completion (CA WRITE_NOTIFY response)
            if let Some(tx) = instance.put_notify_tx.take() {
                let _ = tx.send(());
            }

            use crate::server::recgbl::EventMask;
            let mut event_mask = EventMask::NONE;
            let (include_val, include_archive) = if instance.record.uses_monitor_deadband() {
                instance.check_deadband_ext()
            } else {
                // Binary records (bi/bo/busy/mbbi/mbbo): always post monitors
                (true, true)
            };
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
            // Add subscribed non-{VAL,SEVR,STAT,UDF} fields that
            // actually changed since last notification — mirrors the
            // main-path snapshot gate (process_record_with_links_inner
            // L794-820). Without this, every async-completion cycle
            // re-sends every subscribed auxiliary field even when its
            // value is unchanged, multiplying the monitor traffic for
            // any record that pairs an async write with a sticky
            // metadata field.
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
                        // See Round-30C comment in
                        // `process_record_with_links_inner` — IVOA=2
                        // delegates to the per-record
                        // `apply_invalid_output_value` so OVAL/RVAL/VAL
                        // get the C-convention values.
                        if let Some(ivov) = instance.record.get_field("IVOV") {
                            let _ = instance.record.apply_invalid_output_value(ivov);
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

            (snapshot, out_info, flnk_name)
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

        // FLNK -- only process if target is Passive
        if let Some(ref flnk) = flnk_name {
            let is_passive = if let Some(rec) = self.get_record(flnk).await {
                rec.read().await.common.scan == crate::server::record::ScanType::Passive
            } else {
                false
            };
            if is_passive {
                let _ = self
                    .process_record_with_links(flnk, visited, depth + 1)
                    .await;
            }
        }

        // CP link targets
        self.dispatch_cp_targets(name, visited, depth).await;

        Ok(())
    }

    /// Dispatch CP-link targets that take a CP/CPP input link from `name`.
    ///
    /// C parity (a4bc0db): the CP-driven dispatch is the moral equivalent of
    /// dbCaTask's CA_DBPROCESS handler invoking `db_process(prec)`. Before
    /// processing each target, set PUTF=true; if the target is already
    /// processing (async record mid-flight), set RPRO=true instead so the
    /// in-flight pass reprocesses on completion. Already-visited targets
    /// (current process chain) are skipped via the `visited` cycle guard.
    async fn dispatch_cp_targets(
        &self,
        name: &str,
        visited: &mut std::collections::HashSet<String>,
        depth: usize,
    ) {
        let cp_targets = self.get_cp_targets(name).await;
        for target in cp_targets {
            if visited.contains(&target) {
                continue;
            }
            let target_rec = {
                let records = self.inner.records.read().await;
                records.get(&target).cloned()
            };
            let already_active = if let Some(ref t) = target_rec {
                let mut tg = t.write().await;
                if tg.processing.load(std::sync::atomic::Ordering::Acquire) {
                    tg.common.rpro = true;
                    true
                } else {
                    // epics-base PR #3fb10b6: PUTF must remain
                    // false on CP-driven targets — only the record
                    // directly receiving the dbPut should report
                    // PUTF=1 to dbNotify/onChange observers. The
                    // pre-fix C path (and this Rust port until now)
                    // wrongly propagated PUTF=true onto every CP
                    // target, so a downstream OPI reading PUTF on
                    // chained records saw the put attribution
                    // smeared across the entire chain.
                    false
                }
            } else {
                false
            };
            if already_active {
                continue;
            }
            let _ = self
                .process_record_with_links(&target, visited, depth + 1)
                .await;
        }
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
            let is_input = matches!(
                rtype.as_str(),
                "ai" | "bi" | "longin" | "int64in" | "stringin" | "lsi" | "event"
            );

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

        // epics-base 7.0.7 (SIMM menu):
        //   1 = YES — read/write via SIOL using the cooked VAL
        //   2 = RAW — read/write via SIOL using the raw RVAL when the
        //             record carries one (ai/ao only); falls back to
        //             VAL when no RVAL is present. Mirrors the C
        //             implementation, which treats records lacking
        //             a raw value as "YES" since there's nothing
        //             else to copy.
        let raw_mode = simm == 2;
        let raw_field = if raw_mode { "RVAL" } else { "VAL" };

        // SIMM=YES(1) / SIMM=RAW(2): handle simulation
        if let crate::server::record::ParsedLink::Db(ref link) = siol_link {
            let pv_name = if link.field == "VAL" {
                link.record.clone()
            } else {
                format!("{}.{}", link.record, link.field)
            };

            if is_input {
                // Input record: read from SIOL -> set VAL/RVAL.
                if let Ok(siol_val) = self.get_pv(&pv_name).await {
                    let mut instance = rec.write().await;
                    let target_supports_raw =
                        raw_mode && instance.record.get_field("RVAL").is_some();
                    if target_supports_raw {
                        // PR #ac92e3e follow-up: SIMM=RAW on records
                        // with RVAL (ai/ao/etc.) writes the raw value
                        // into RVAL and runs the record's own
                        // process() so the LINR / ESLO / EOFF / ASLO
                        // / AOFF conversion chain computes VAL. The
                        // pre-fix path additionally called set_val
                        // here, which overwrote VAL with the raw
                        // count and silently bypassed conversion —
                        // the visible failure mode was "SIMM=RAW
                        // simulation returns counts instead of EGU".
                        //
                        // Coerce to RVAL's native DBR type before
                        // put_field — ai.RVAL is Long, but SIOL on a
                        // soft channel typically yields Double. Without
                        // the coerce step the put_field rejects with
                        // TypeMismatch and leaves RVAL at 0, so
                        // process() computes VAL = 0*ESLO + EOFF
                        // (the offset only), not the intended
                        // RAW*ESLO + EOFF.
                        let rval_type = instance
                            .record
                            .field_list()
                            .iter()
                            .find(|f| f.name == "RVAL")
                            .map(|f| f.dbf_type)
                            .unwrap_or(crate::types::DbFieldType::Long);
                        // C parity (aiRecord.c:495): `rval = (long)floor(sval)`.
                        // Rust `convert_to(Long)` truncates toward zero,
                        // diverging for negative bipolar-ADC raw values
                        // (sval=-1.5 → C: -2, Rust as-cast: -1).
                        // Floor explicitly when narrowing a float to
                        // an integer RVAL.
                        let coerced = match (&siol_val, rval_type) {
                            (EpicsValue::Double(d), crate::types::DbFieldType::Long) => {
                                EpicsValue::Long(d.floor() as i32)
                            }
                            (EpicsValue::Double(d), crate::types::DbFieldType::Int64) => {
                                EpicsValue::Int64(d.floor() as i64)
                            }
                            (EpicsValue::Float(d), crate::types::DbFieldType::Long) => {
                                EpicsValue::Long((*d as f64).floor() as i32)
                            }
                            (EpicsValue::Float(d), crate::types::DbFieldType::Int64) => {
                                EpicsValue::Int64((*d as f64).floor() as i64)
                            }
                            _ if siol_val.db_field_type() != rval_type => {
                                siol_val.convert_to(rval_type)
                            }
                            _ => siol_val,
                        };
                        let _ = instance.record.put_field("RVAL", coerced);
                        let _ = instance.record.process();
                    } else {
                        // Records without RVAL fall back to SIMM=YES
                        // semantics: the SIOL value goes straight into
                        // VAL; no conversion to run.
                        let _ = instance.record.set_val(siol_val);
                    }
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
                // Output record: write VAL (or RVAL for SIMM=RAW) to
                // SIOL (skip device write).
                let out_val = {
                    let instance = rec.read().await;
                    if raw_mode {
                        // RAW path: prefer RVAL when the record has
                        // one. Otherwise fall through to VAL.
                        instance
                            .record
                            .get_field(raw_field)
                            .or_else(|| instance.record.val())
                    } else {
                        instance.record.val()
                    }
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
