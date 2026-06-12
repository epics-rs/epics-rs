mod command_planner;
mod field_access;
mod state_machine;
mod status_update;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::record::{
    FieldDesc, MENU_ALARM_SEVR, MENU_YES_NO, ParsedLink, ProcessAction, ProcessOutcome, Record,
    RecordProcessResult, parse_link_v2,
};
use epics_base_rs::types::EpicsValue;

// Record-specific `DBF_MENU` choice tables, in `.dbd` value order (the
// index↔string mapping is wire-visible to clients). Source: the C
// `motorRecord.dbd` menu definitions (motor module). The shared menus
// (`HHSV`/`HSV`/`LSV`/`LLSV`/`HLSV` = `menuAlarmSevr`, `OMSL` = `menuOmsl`)
// are resolved by the base record registry and not restated here.
const MOTOR_DIR_CHOICES: &[&str] = &["Pos", "Neg"];
const MOTOR_FOFF_CHOICES: &[&str] = &["Variable", "Frozen"];
const MOTOR_SET_CHOICES: &[&str] = &["Use", "Set"];
const MOTOR_UEIP_CHOICES: &[&str] = &["No", "Yes"];
const MOTOR_RSTM_CHOICES: &[&str] = &["Never", "Always", "NearZero", "Conditional"];
const MOTOR_ACCU_CHOICES: &[&str] = &["Use ACCL", "Use ACCS"];
const MOTOR_RMOD_CHOICES: &[&str] = &["Default", "Arithmetic", "Geometric", "In-Position"];
const MOTOR_SPMG_CHOICES: &[&str] = &["Stop", "Pause", "Move", "Go"];
const MOTOR_TORQ_CHOICES: &[&str] = &["Disable", "Enable"];
const MOTOR_STUP_CHOICES: &[&str] = &["OFF", "ON", "BUSY"];

use crate::coordinate;
use crate::device_state::*;
use crate::fields::*;
use crate::flags::*;

/// EPICS Motor Record implementation.
#[derive(Debug, Clone)]
pub struct MotorRecord {
    pub pos: PositionFields,
    pub conv: ConversionFields,
    pub vel: VelocityFields,
    pub retry: RetryFields,
    pub limits: LimitFields,
    pub ctrl: ControlFields,
    pub stat: StatusFields,
    pub pid: PidFields,
    pub disp: DisplayFields,
    pub timing: TimingFields,
    pub pco: PcoFields,
    pub internal: InternalFields,
    /// Database-link / menu fields imported from the public C motorRecord.dbd
    /// surface (OUT/RDBL/DOL/OMSL/RLNK/STOO/DINP/RINP/POST).
    pub links: LinkFields,
    /// Alarm-limit fields imported from the public C motorRecord.dbd surface
    /// (HIHI/HIGH/LOW/LOLO/HHSV/LLSV).
    pub alarm: AlarmFields,
    /// Pending event for next process() call
    pending_event: Option<MotorEvent>,
    /// Track which field was last written (for process)
    last_write: Option<CommandSource>,
    /// One-shot request from a runtime JVEL put: re-emit the jog command
    /// with the new JVEL/JAR if a jog is active when the put's process
    /// pass runs (C special() motorRecordJVEL, motorRecord.cc:3059-3072).
    jog_retune_pending: bool,
    /// Shared state mailbox for device communication
    device_state: Option<SharedDeviceState>,
    /// Last seen status sequence number
    last_seen_seq: u64,
    /// Whether initial readback has been performed
    initialized: bool,
    /// Monotonic counter for delay request IDs
    next_delay_id: u64,
}

impl Default for MotorRecord {
    fn default() -> Self {
        Self {
            pos: PositionFields::default(),
            conv: ConversionFields::default(),
            vel: VelocityFields::default(),
            retry: RetryFields::default(),
            limits: LimitFields::default(),
            ctrl: ControlFields::default(),
            stat: StatusFields::default(),
            pid: PidFields::default(),
            disp: DisplayFields::default(),
            timing: TimingFields::default(),
            pco: PcoFields::default(),
            internal: InternalFields::default(),
            links: LinkFields::default(),
            alarm: AlarmFields::default(),
            pending_event: None,
            last_write: None,
            jog_retune_pending: false,
            device_state: None,
            last_seen_seq: 0,
            initialized: false,
            next_delay_id: 0,
        }
    }
}

/// Motion direction for hardware limit checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MotionDirection {
    Positive,
    Negative,
}

impl MotorRecord {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a motor record wired to a shared device state mailbox.
    pub fn with_device_state(mut self, state: SharedDeviceState) -> Self {
        self.device_state = Some(state);
        self
    }

    /// Set the shared device state (for late injection by device support init).
    pub fn set_device_state(&mut self, state: SharedDeviceState) {
        self.device_state = Some(state);
    }

    /// Set a pending event for the next process() call.
    pub fn set_event(&mut self, event: MotorEvent) {
        self.pending_event = Some(event);
    }

    /// Clear any pending write command source.
    ///
    /// Called by device support init() so that pass0-restored field values
    /// are not interpreted as move commands during PINI processing.
    pub fn clear_last_write(&mut self) {
        self.last_write = None;
    }

    /// C do_work collection gate (motorRecord.cc:1994): `omsl ==
    /// menuOmslclosed_loop && dol.type == DB_LINK`. While it holds, the
    /// entire button/tweak/relative/raw collection block (2008-2198) is
    /// bypassed and VAL arrives only through the DOL link; a constant or
    /// CA DOL leaves the collection active even under closed loop.
    pub(crate) fn closed_loop_dol_collection(&self) -> bool {
        self.links.omsl == 1 && matches!(parse_link_v2(&self.links.dol), ParsedLink::Db(_))
    }

    /// True when a position field (VAL/DVAL/RVAL/RLV) was written during
    /// pass0 — i.e. autosave restored a saved position.
    ///
    /// Device support `init()` uses this to decide whether to reseed the
    /// controller with the restored DVAL. It MUST be queried before
    /// [`clear_last_write`](Self::clear_last_write), which device support
    /// calls later in `init()`.
    ///
    /// This is the correct "was a position restored" signal: a genuine
    /// restored position of exactly `0.0` is indistinguishable from the
    /// field default if you only inspect the DVAL value, but the pass0
    /// write still records `last_write`.
    pub fn was_position_restored(&self) -> bool {
        matches!(
            self.last_write,
            Some(
                CommandSource::Val | CommandSource::Dval | CommandSource::Rval | CommandSource::Rlv
            )
        )
    }

    /// Signal that the external URIP readback link is in error or recovered.
    /// While `urip` is true and `error` is set, new motions are refused and
    /// in-progress motion is stopped (C: `db5da2f0`, `7493d50b`).
    pub fn set_rdbl_error(&mut self, error: bool) {
        self.conv.rdbl_error = error;
    }

    /// Fire the readback output link (RLNK) with the current RBV.
    ///
    /// C `motorRecord.cc:1495` calls `dbPutLink(&rlnk, DBR_DOUBLE, &rbv, 1)`
    /// unconditionally just before `process_exit` on every process pass, so a
    /// record wired through RLNK re-processes whenever the motor record does.
    /// We mirror that by emitting a `WriteDbLink` on every process cycle,
    /// including the move-start `AsyncPendingNotify` (DMOV 1→0) pass: C fires
    /// `dbPutLink` before `monitor()` (motorRecord.cc:1507) and before the
    /// `recGblFwdLink` gate (motorRecord.cc:1509), so it runs even while the
    /// move is still pending. A `dbPutLink` processes a PP readback target even
    /// when RBV is unchanged, so skipping the pending pass would drop one of
    /// the target's process cycles relative to C. The framework runs the
    /// `WriteDbLink` before the deferred FLNK, exactly where C fires it. The
    /// empty link is skipped so motors with no RLNK emit nothing.
    fn push_readback_link_write(&self, actions: &mut Vec<ProcessAction>) {
        if !self.links.rlnk.is_empty() {
            actions.push(ProcessAction::WriteDbLink {
                link_field: "RLNK",
                value: EpicsValue::Double(self.pos.rbv),
            });
        }
    }
}

impl Record for MotorRecord {
    fn record_type(&self) -> &'static str {
        "motor"
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    fn can_device_write(&self) -> bool {
        true
    }

    fn is_put_complete(&self) -> bool {
        self.stat.dmov
    }

    // C RSET per-field metadata (motorRecord.cc get_units 3156-3208,
    // get_precision 3313-3337, get_graphic_double 3213-3258,
    // get_control_double 3263-3308, get_alarm_double 3344-3361).
    fn field_metadata_override(
        &self,
        field: &str,
    ) -> Option<epics_base_rs::server::record::FieldMetadataOverride> {
        Some(field_access::metadata_override(self, field))
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // If wired to device state, determine event from shared mailbox
        if self.device_state.is_some() {
            if let Some(event) = self.determine_event() {
                self.pending_event = Some(event);
            }
        }

        let effects = self.do_process();
        // DMOV=0 means a move started (or sub-step pulse).
        // Flush DMOV=0 even if no commands were emitted (sub-step case).
        let move_started = !self.stat.dmov;

        // Write effects to shared mailbox for DeviceSupport.write() to consume.
        // If a previous batch has not been consumed yet (two process() cycles
        // without an intervening write()), fold the new batch into it rather
        // than overwriting — otherwise the earlier move command is lost.
        if let Some(state) = self.device_state.clone() {
            let actions = self.effects_to_actions(&effects);
            match state.lock() {
                Ok(mut ds) => match ds.pending_actions.take() {
                    Some(mut prev) => {
                        tracing::warn!("motor: pending_actions not yet consumed — merging batches");
                        prev.merge_newer(actions);
                        ds.pending_actions = Some(prev);
                    }
                    None => ds.pending_actions = Some(actions),
                },
                Err(e) => {
                    tracing::error!("device state lock poisoned in process: {e}");
                }
            }
        }

        if move_started && !self.internal.dmov_notified {
            // First DMOV 1→0 transition: flush immediately so monitors see
            // the transition before the move completes.
            self.internal.dmov_notified = true;
            use epics_base_rs::types::EpicsValue;
            let fields = vec![
                ("DMOV".to_string(), EpicsValue::Short(0)),
                ("MOVN".to_string(), EpicsValue::Short(1)),
                ("VAL".to_string(), EpicsValue::Double(self.pos.val)),
                ("DVAL".to_string(), EpicsValue::Double(self.pos.dval)),
                ("RVAL".to_string(), EpicsValue::Int64(self.pos.rval)),
                ("RBV".to_string(), EpicsValue::Double(self.pos.rbv)),
                ("DRBV".to_string(), EpicsValue::Double(self.pos.drbv)),
                // C do_work marks M_MIP on every move dispatch and
                // M_RCNT when the retry count resets (motorRecord.cc:
                // 1929-1932), so the move-start monitor() pass posts
                // both.
                (
                    "MIP".to_string(),
                    EpicsValue::Short(self.stat.mip.bits() as i16),
                ),
                ("RCNT".to_string(), EpicsValue::Short(self.retry.rcnt)),
            ];
            // C fires `dbPutLink(&rlnk, ...)` (motorRecord.cc:1495) on this
            // move-start pass too — before `monitor()` and the `recGblFwdLink`
            // gate — so the RLNK readback target still re-processes while the
            // move is pending.
            let mut actions = Vec::new();
            self.push_readback_link_write(&mut actions);
            Ok(ProcessOutcome {
                result: RecordProcessResult::AsyncPendingNotify(fields),
                actions,
                device_did_compute: false,
            })
        } else {
            // Ongoing motion or idle: full snapshot so all changed fields
            // (RBV, DRBV, MSTA, limits, etc.) get posted as monitors.
            if !move_started {
                self.internal.dmov_notified = false;
            }
            let mut outcome = ProcessOutcome::complete();
            self.push_readback_link_write(&mut outcome.actions);
            Ok(outcome)
        }
    }

    /// C process_exit (motorRecord.cc:1509-1510): `if (dmov != 0)
    /// recGblFwdLink(pmr)` — FLNK fires on EVERY pass that exits with
    /// DMOV true, and on nothing else. (`0ef39053` gated this on the
    /// DMOV false→true transition; `c970afbf` reverted it: "Except for
    /// the condition that DMOV == FALSE, FLNK processing was
    /// standard.") Reading DMOV directly at FLNK time IS the C gate by
    /// construction — there is no per-path suppression state to track.
    fn should_fire_forward_link(&self) -> bool {
        self.stat.dmov
    }

    /// Input links the framework must resolve via `dbGetLink` BEFORE each
    /// `process()`, so their values are available to the move computation —
    /// exactly the synchronous reads C `motorRecord.cc` performs inside
    /// `do_work`/`process_motor_info`.
    ///
    /// - CLOSED_LOOP DOL → VAL (motorRecord.cc:1994), gated below.
    /// - URIP=Yes RDBL → RRBV: C `process_motor_info` (motorRecord.cc:3687)
    ///   does `dbGetLink(&pmr->rdbl, DBR_DOUBLE, &rdblvalue, ...)` and scales it
    ///   into RRBV (`rrbv = NINT(rdblvalue*rres/mres)`). The scaling lives in
    ///   the status-update path; this just feeds it the raw link value through
    ///   the `RDBL_VAL` carrier. C reads with `dbGetLink` regardless of link
    ///   type, so any RDBL link form is read (the framework skips an empty
    ///   link).
    fn pre_process_actions(&mut self) -> Vec<ProcessAction> {
        let mut actions = Vec::new();

        // CLOSED_LOOP: drive VAL from the DOL link (motorRecord.cc:1994-1999).
        // C reads DOL into .val only when `omsl == closed_loop && dol.type ==
        // DB_LINK`; a constant DOL is initialised once (recGblInitConstantLink,
        // :680) and never re-read, so gate on the link parsing as a local DB
        // link. Writing VAL records a VAL command-source, so the following
        // process() plans the move to the link value.
        //
        // Read only when DMOV (done): C calls do_work — the function holding
        // the DOL read — for `pmr->dmov` or a non-callback process, but NOT on
        // a CALLBACK_DATA poll mid-move (motorRecord.cc:1487-1492). Gating on
        // DMOV mirrors that dominant condition and stops a device poll from
        // re-reading DOL and clobbering the in-flight target every cycle.
        if self.stat.dmov && self.closed_loop_dol_collection() {
            actions.push(ProcessAction::ReadDbLink {
                link_field: "DOL",
                target_field: "VAL",
            });
        }

        // C process_motor_info (3676-3699) is an else-if chain: UEIP=Yes
        // takes the encoder and the RDBL dbGetLink never executes, so a
        // failed RDBL must not stop the axis while the encoder is in use.
        if self.conv.urip && !self.conv.ueip {
            actions.push(ProcessAction::ReadDbLink {
                link_field: "RDBL",
                target_field: "RDBL_VAL",
            });
        }
        actions
    }

    /// Framework report of which requested link reads produced a value
    /// this cycle — the analogue of C inspecting
    /// `RTN_SUCCESS(dbGetLink(&pmr->rdbl, ...))` in `process_motor_info`
    /// (motorRecord.cc:3687-3698). A failed RDBL read latches
    /// `rdbl_error`; the completion/planner paths stop an in-flight
    /// motion and refuse new ones while it is set. An empty RDBL is
    /// skipped by the framework (CONSTANT link in C — a successful
    /// dbGetLink), so it never reports as an error here.
    fn set_resolved_input_links(&mut self, resolved: &[&'static str]) {
        if self.conv.urip && !self.conv.ueip && !self.links.rdbl.is_empty() {
            self.conv.rdbl_error = !resolved.contains(&"RDBL");
        }
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        field_access::motor_get_field(self, name)
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        field_access::motor_put_field(self, name, value)
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        field_access::FIELDS
    }

    /// Record-specific `DBF_MENU` fields, served as `DBR_ENUM` with the
    /// menu's choice labels in `.dbd` index order (C `motorRecord.dbd`).
    /// `NTM` is `menu(menuYesNo)` (the shared base table); `UEIP`/`URIP`
    /// share `menu(motorUEIP)`. `HLSV` ("HW Limit Violation Svr",
    /// `motorRecord.dbd:452`) is `menu(menuAlarmSevr)` but its field *name*
    /// is motor-specific, so the base registry — which keys the shared
    /// severity menu by the standard names `HHSV`/`HSV`/`LSV`/`LLSV` — does
    /// not resolve it; it is mapped here. The standard alarm severities and
    /// `OMSL` are shared menus resolved by the base registry.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "DIR" => Some(MOTOR_DIR_CHOICES),
            "FOFF" => Some(MOTOR_FOFF_CHOICES),
            "SET" => Some(MOTOR_SET_CHOICES),
            "UEIP" | "URIP" => Some(MOTOR_UEIP_CHOICES),
            "RSTM" => Some(MOTOR_RSTM_CHOICES),
            "ACCU" => Some(MOTOR_ACCU_CHOICES),
            "RMOD" => Some(MOTOR_RMOD_CHOICES),
            "SPMG" => Some(MOTOR_SPMG_CHOICES),
            "CNEN" => Some(MOTOR_TORQ_CHOICES),
            "STUP" => Some(MOTOR_STUP_CHOICES),
            "HLSV" => Some(MENU_ALARM_SEVR),
            "NTM" => Some(MENU_YES_NO),
            _ => None,
        }
    }

    /// C `init_record`: on pass 1, once all `field()` values have been
    /// applied, reconcile the rev↔EGU speed pairs (C
    /// `check_speed_and_resolution`) and establish the limit invariant from
    /// the loaded DHLM/DLLM (C `set_dial_highlimit`/`set_dial_lowlimit`),
    /// then enable the runtime cascade semantics in `put_field`. See
    /// [`field_access::motor_sync_speed_at_init`] and
    /// [`field_access::motor_sync_limits_at_init`].
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 1 {
            // C init_record runs check_speed_and_resolution (641) with the
            // resolution triple first (3904-3927), then the speed pairs,
            // then the raw-limit rules.
            field_access::motor_sync_resolution_at_init(self);
            field_access::motor_sync_speed_at_init(self);
            field_access::motor_sync_limits_at_init(self);
            // C 642: the single load-order-independent enforcement of
            // RDBD >= |MRES|, against the reconciled resolution. The RDBD
            // put handler stays inert during load.
            self.enforce_min_retry_deadband();
            self.internal.init_invariants_synced = true;
        }
        Ok(())
    }

    fn primary_field(&self) -> &'static str {
        "VAL"
    }

    /// MDEL/ADEL monitor deadband applies to the readback (RBV), not the
    /// VAL setpoint. C `monitor()` gates RBV value/archive monitors on
    /// MDEL/ADEL; VAL is a setpoint that only changes on a move command.
    fn monitor_deadband_value(&self) -> Option<EpicsValue> {
        Some(EpicsValue::Double(self.pos.rbv))
    }

    /// The deadband gates RBV's monitor delivery (C motorRecord.cc
    /// 3468-3507); with this, the framework routes VAL through generic
    /// change-detection — posted only when the setpoint actually moved
    /// (C M_VAL mark semantics), not on every readback poll.
    fn monitor_deadband_field(&self) -> &'static str {
        "RBV"
    }

    /// C `alarm_sub()` (motorRecord.cc:3367-3406) — motor-specific alarm
    /// severities, raised into `nsta`/`nsev` before the framework's
    /// `evaluate_alarms`.
    fn check_alarms(&mut self, common: &mut epics_base_rs::server::record::CommonFields) {
        use epics_base_rs::server::recgbl::{alarm_status, rec_gbl_set_sevr};
        use epics_base_rs::server::record::AlarmSeverity;

        // C 3372-3376: an undefined VAL short-circuits every other check.
        if common.udf {
            rec_gbl_set_sevr(common, alarm_status::UDF_ALARM, AlarmSeverity::Invalid);
            return;
        }

        // C 3379-3388: limit-switch and soft-limit violations. BOTH the
        // high and low arms gate on HLSV and raise at HLSV — motorRecord
        // has no low-side severity field. Limit switches count even when
        // not in the direction of travel (externally triggered move),
        // and a DVAL outside the dial soft limits alarms too.
        let hlsv = AlarmSeverity::from_u16(self.limits.hlsv as u16);
        if hlsv != AlarmSeverity::NoAlarm {
            if self.limits.hls || self.pos.dval > self.limits.dhlm {
                rec_gbl_set_sevr(common, alarm_status::HIGH_ALARM, hlsv);
                return;
            }
            if self.limits.lls || self.pos.dval < self.limits.dllm {
                rec_gbl_set_sevr(common, alarm_status::LOW_ALARM, hlsv);
                return;
            }
        }

        // C 3392-3398: controller communication error — COMM/INVALID,
        // and the MSTA bit is CLEARED (C `msta.Bits.CNTRL_COMM_ERR = 0`
        // + `MARK(M_MSTA)`) so the alarm is one-shot until the driver
        // re-reports it. The framework builds the monitor snapshot after
        // this hook, so the cleared MSTA posts in the same cycle.
        if self.stat.msta.contains(MstaFlags::COMM_ERR) {
            self.stat.msta.remove(MstaFlags::COMM_ERR);
            rec_gbl_set_sevr(common, alarm_status::COMM_ALARM, AlarmSeverity::Invalid);
        }

        // C 3400-3403: slip/stall or controller problem → STATE/MAJOR.
        // No early return above COMM — C falls through from the comm
        // check, so both can accumulate (set_sevr keeps the higher).
        if self
            .stat
            .msta
            .intersects(MstaFlags::SLIP_STALL | MstaFlags::PROBLEM)
        {
            rec_gbl_set_sevr(common, alarm_status::STATE_ALARM, AlarmSeverity::Major);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_mode_updates_offset() {
        let mut rec = MotorRecord::new();
        rec.conv.mres = 0.01;
        rec.pos.dval = 5.0;
        rec.conv.set = true;
        rec.put_field("VAL", EpicsValue::Double(100.0)).unwrap();
        // Offset should be updated, DVAL unchanged
        assert_eq!(rec.pos.dval, 5.0);
        assert_eq!(rec.pos.off, 95.0); // 100 - 1*5
        // C 2206-2227: the FOFF=Variable redefinition completes on the
        // spot — no controller command is dispatched (LOAD_POS belongs
        // to the Frozen/DVAL/RVAL paths) and DMOV stays TRUE.
        assert_eq!(rec.last_write, None);
        assert!(rec.stat.dmov);
        assert!(rec.stat.mip.is_empty());
    }

    #[test]
    fn test_should_fire_forward_link() {
        let mut rec = MotorRecord::new();
        assert!(rec.should_fire_forward_link());

        // C motorRecord.cc:1509-1510: DMOV is the only FLNK gate.
        rec.stat.dmov = false;
        assert!(!rec.should_fire_forward_link());
    }

    // C motorRecord.cc:1495 — every process pass fires the RLNK readback
    // output link with the current RBV via dbPutLink().
    #[test]
    fn rlnk_readback_link_fired_with_rbv_each_process() {
        let mut rec = MotorRecord::new();
        rec.links.rlnk = "readback_sink.PROC".to_string();
        rec.pos.rbv = 12.5;
        let outcome = rec.process().unwrap();
        let rbv = rec.pos.rbv;
        assert!(
            outcome.actions.iter().any(|a| matches!(
                a,
                ProcessAction::WriteDbLink {
                    link_field: "RLNK",
                    value: EpicsValue::Double(v),
                } if *v == rbv
            )),
            "process() must emit a WriteDbLink to RLNK carrying RBV"
        );
    }

    // C motorRecord.cc:1495 fires `dbPutLink(&rlnk, ...)` on the move-start
    // pass too — before `monitor()` (motorRecord.cc:1507) and the
    // `recGblFwdLink` gate (motorRecord.cc:1509) — so the RLNK readback
    // target re-processes even while the move is still pending. The framework
    // executes the emitted WriteDbLink on the AsyncPendingNotify branch.
    #[test]
    fn rlnk_readback_link_fired_on_move_start_async_pending() {
        let mut rec = MotorRecord::new();
        rec.links.rlnk = "readback_sink.PROC".to_string();
        // Enter a move: DMOV goes true→false. The first process() of a fresh
        // move returns the intermediate DMOV=0 notification.
        rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
        rec.set_event(MotorEvent::UserWrite(CommandSource::Val));
        let outcome = rec.process().unwrap();
        assert!(
            matches!(outcome.result, RecordProcessResult::AsyncPendingNotify(_)),
            "first process() of a fresh move must be the move-start notification"
        );
        let rbv = rec.pos.rbv;
        assert!(
            outcome.actions.iter().any(|a| matches!(
                a,
                ProcessAction::WriteDbLink {
                    link_field: "RLNK",
                    value: EpicsValue::Double(v),
                } if *v == rbv
            )),
            "the move-start AsyncPendingNotify cycle must still emit the RLNK readback write"
        );
    }

    #[test]
    fn rlnk_not_fired_when_link_unset() {
        let mut rec = MotorRecord::new();
        assert!(rec.links.rlnk.is_empty());
        let outcome = rec.process().unwrap();
        assert!(
            !outcome.actions.iter().any(|a| matches!(
                a,
                ProcessAction::WriteDbLink {
                    link_field: "RLNK",
                    ..
                }
            )),
            "an unset RLNK must emit no readback link write"
        );
    }

    // C do_work marks M_MIP on the move dispatch and M_RCNT on the retry
    // counter reset (motorRecord.cc:1929-1932); monitor() posts both on
    // the move-start pass.
    #[test]
    fn move_start_notify_carries_mip_and_rcnt() {
        let mut rec = MotorRecord::new();
        rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
        rec.set_event(MotorEvent::UserWrite(CommandSource::Val));
        let outcome = rec.process().unwrap();
        let RecordProcessResult::AsyncPendingNotify(fields) = outcome.result else {
            panic!("first process() of a fresh move must be the move-start notification");
        };
        let mip = rec.stat.mip.bits() as i16;
        assert!(
            fields
                .iter()
                .any(|(n, v)| n == "MIP" && *v == EpicsValue::Short(mip)),
            "move-start notify must carry MIP: {fields:?}"
        );
        assert!(
            fields
                .iter()
                .any(|(n, v)| n == "RCNT" && *v == EpicsValue::Short(rec.retry.rcnt)),
            "move-start notify must carry RCNT: {fields:?}"
        );
    }

    // C monitor() (motorRecord.cc:3468-3507): the MDEL/ADEL deadband
    // gates the RBV post. The framework keys delivery off this hook
    // pair — both must track RBV.
    #[test]
    fn monitor_deadband_hooks_track_rbv() {
        let mut rec = MotorRecord::new();
        rec.pos.rbv = 7.25;
        assert_eq!(rec.monitor_deadband_field(), "RBV");
        assert_eq!(rec.monitor_deadband_value(), Some(EpicsValue::Double(7.25)));
    }

    // C motorRecord.cc:3687 — URIP=Yes pulls the readback from the RDBL link
    // before process(); the status path then scales it into RRBV.
    #[test]
    fn urip_pulls_rdbl_link_before_process() {
        let mut rec = MotorRecord::new();
        rec.conv.urip = true;
        rec.links.rdbl = "ext_readback.RBV".to_string();
        let actions = rec.pre_process_actions();
        assert!(
            actions.iter().any(|a| matches!(
                a,
                ProcessAction::ReadDbLink {
                    link_field: "RDBL",
                    target_field: "RDBL_VAL",
                }
            )),
            "URIP=Yes must request a pre-process read of RDBL into RDBL_VAL"
        );
    }

    #[test]
    fn no_rdbl_pull_when_urip_off() {
        let mut rec = MotorRecord::new();
        // Link configured, but URIP=No — C reads RRBV from the controller.
        rec.conv.urip = false;
        rec.links.rdbl = "ext_readback.RBV".to_string();
        let actions = rec.pre_process_actions();
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                ProcessAction::ReadDbLink {
                    link_field: "RDBL",
                    ..
                }
            )),
            "URIP=No must not read the RDBL link"
        );
    }

    #[test]
    fn no_rdbl_pull_when_ueip_wins() {
        // C process_motor_info (3676-3699) else-if: UEIP=Yes takes the
        // encoder and the RDBL dbGetLink never executes.
        let mut rec = MotorRecord::new();
        rec.conv.urip = true;
        rec.conv.ueip = true;
        rec.links.rdbl = "ext_readback.RBV".to_string();
        let actions = rec.pre_process_actions();
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                ProcessAction::ReadDbLink {
                    link_field: "RDBL",
                    ..
                }
            )),
            "UEIP=Yes must suppress the RDBL read"
        );
    }

    fn has_dol_read(actions: &[ProcessAction]) -> bool {
        actions.iter().any(|a| {
            matches!(
                a,
                ProcessAction::ReadDbLink {
                    link_field: "DOL",
                    target_field: "VAL",
                }
            )
        })
    }

    // C motorRecord.cc:1994 — in CLOSED_LOOP mode VAL is driven by the DOL DB
    // link each idle pass.
    #[test]
    fn closed_loop_db_dol_drives_val() {
        let mut rec = MotorRecord::new();
        rec.links.omsl = 1; // menuOmsl: closed_loop
        rec.links.dol = "setpoint_src.VAL".to_string(); // resolves to a DB link
        assert!(rec.stat.dmov, "record starts done");
        assert!(
            has_dol_read(&rec.pre_process_actions()),
            "CLOSED_LOOP with a DB DOL must read DOL into VAL"
        );
    }

    #[test]
    fn supervisory_omsl_ignores_dol() {
        let mut rec = MotorRecord::new();
        rec.links.omsl = 0; // supervisory
        rec.links.dol = "setpoint_src.VAL".to_string();
        assert!(
            !has_dol_read(&rec.pre_process_actions()),
            "supervisory OMSL must not read DOL"
        );
    }

    // C reads DOL only when dol.type == DB_LINK; a constant DOL is initialised
    // once and not re-read (motorRecord.cc:1994 vs :680).
    #[test]
    fn closed_loop_constant_dol_not_reread() {
        let mut rec = MotorRecord::new();
        rec.links.omsl = 1;
        rec.links.dol = "5.0".to_string(); // CONSTANT, not a DB link
        assert!(
            !has_dol_read(&rec.pre_process_actions()),
            "a constant DOL must not be re-read each pass"
        );
    }

    // C calls do_work (the DOL read) for pmr->dmov, not on a CALLBACK_DATA
    // poll mid-move (motorRecord.cc:1487-1492).
    #[test]
    fn closed_loop_dol_not_read_mid_move() {
        let mut rec = MotorRecord::new();
        rec.links.omsl = 1;
        rec.links.dol = "setpoint_src.VAL".to_string();
        rec.stat.dmov = false; // a move is in flight
        assert!(
            !has_dol_read(&rec.pre_process_actions()),
            "DOL must not be re-read while a move is in flight"
        );
    }

    // C motorRecord.cc:1509-1510 (restored by c970afbf): FLNK fires on
    // every pass exiting with DMOV true — including a bare idle pass.
    #[test]
    fn test_flnk_fires_on_idle_pass_with_dmov_true() {
        let mut rec = MotorRecord::new();
        assert!(rec.stat.dmov);
        let _ = rec.process();
        assert!(
            rec.should_fire_forward_link(),
            "an idle pass exits with DMOV true — C fires FLNK on it"
        );
    }

    #[test]
    fn test_flnk_fires_on_motion_completion_transition() {
        let mut rec = MotorRecord::new();
        // Enter a move: DMOV goes true→false.
        rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
        rec.set_event(MotorEvent::UserWrite(CommandSource::Val));
        let _ = rec.process();
        assert!(!rec.stat.dmov); // moving
        assert!(!rec.should_fire_forward_link()); // suppressed while moving

        // Driver reports completion; next process finalizes DMOV false→true.
        rec.set_event(MotorEvent::DeviceUpdate(
            asyn_rs::interfaces::motor::MotorStatus {
                position: 10.0,
                encoder_position: 10.0,
                done: true,
                moving: false,
                ..Default::default()
            },
        ));
        let _ = rec.process();
        assert!(rec.stat.dmov); // completed
        assert!(
            rec.should_fire_forward_link(),
            "FLNK must fire on the DMOV false→true completion transition"
        );
    }

    // Public C motorRecord.dbd field surface: every link/menu/alarm/last-value
    // field a CA or PVA client can address must be discoverable through
    // field_list() and round-trip through get/put (writable) or read (NOMOD).
    // Behavioral link routing (DOL closed-loop drive, RDBL readback pull, RLNK
    // forward firing) is deliberately outside this field-presence import.
    #[test]
    fn dbd_link_menu_alarm_fields_are_field_open() {
        use epics_base_rs::types::DbFieldType;

        let mut rec = MotorRecord::new();

        // Writable DBF_INLINK/OUTLINK, DBF_MENU and DBF_DOUBLE alarm fields:
        // present, writable, and surviving a put/get round-trip.
        let writable: &[(&str, DbFieldType, EpicsValue)] = &[
            (
                "RDBL",
                DbFieldType::String,
                EpicsValue::String("$(P)$(R).RBV CP".into()),
            ),
            (
                "DOL",
                DbFieldType::String,
                EpicsValue::String("$(P)setpoint CP".into()),
            ),
            (
                "RLNK",
                DbFieldType::String,
                EpicsValue::String("$(P)done.PROC PP".into()),
            ),
            (
                "OUT",
                DbFieldType::String,
                EpicsValue::String("@asyn(PORT,0)".into()),
            ),
            ("OMSL", DbFieldType::Short, EpicsValue::Short(1)),
            ("HIHI", DbFieldType::Double, EpicsValue::Double(95.0)),
        ];
        for (name, dbf, val) in writable {
            let desc = rec
                .field_list()
                .iter()
                .find(|f| f.name == *name)
                .unwrap_or_else(|| panic!("{name} missing from motor field_list()"));
            assert_eq!(desc.dbf_type, *dbf, "{name} dbf_type");
            assert!(!desc.read_only, "{name} must be writable");
            rec.put_field(name, val.clone())
                .unwrap_or_else(|e| panic!("put {name}: {e:?}"));
            assert_eq!(rec.get_field(name), Some(val.clone()), "{name} round-trip");
        }

        // OMSL/HHSV are menu indices and must clamp to their menu range.
        rec.put_field("OMSL", EpicsValue::Short(7)).unwrap();
        assert_eq!(rec.get_field("OMSL"), Some(EpicsValue::Short(1)));

        // Read-only last-value / monitor-map fields (SPC_NOMOD): present,
        // readable, and flagged read-only so the record layer rejects writes.
        let read_only: &[(&str, DbFieldType)] = &[
            ("ALST", DbFieldType::Double),
            ("MLST", DbFieldType::Double),
            ("MMAP", DbFieldType::Long),
            ("NMAP", DbFieldType::Long),
        ];
        for (name, dbf) in read_only {
            let desc = rec
                .field_list()
                .iter()
                .find(|f| f.name == *name)
                .unwrap_or_else(|| panic!("{name} missing from motor field_list()"));
            assert_eq!(desc.dbf_type, *dbf, "{name} dbf_type");
            assert!(desc.read_only, "{name} must be read-only (SPC_NOMOD)");
            assert!(rec.get_field(name).is_some(), "{name} must be readable");
        }
    }
}

#[cfg(test)]
mod menu_choice_tests {
    use super::MotorRecord;
    use epics_base_rs::server::record::Record;

    // The record-specific motor menus must hand the base snapshot path the
    // exact .dbd choice tables, in value order (wire-visible to clients).
    #[test]
    fn motor_menu_field_choices_match_dbd() {
        let rec = MotorRecord::new();
        assert_eq!(rec.menu_field_choices("DIR"), Some(&["Pos", "Neg"][..]));
        assert_eq!(
            rec.menu_field_choices("FOFF"),
            Some(&["Variable", "Frozen"][..])
        );
        assert_eq!(
            rec.menu_field_choices("SPMG"),
            Some(&["Stop", "Pause", "Move", "Go"][..])
        );
        assert_eq!(
            rec.menu_field_choices("RMOD"),
            Some(&["Default", "Arithmetic", "Geometric", "In-Position"][..])
        );
        assert_eq!(
            rec.menu_field_choices("STUP"),
            Some(&["OFF", "ON", "BUSY"][..])
        );
        // NTM is menu(menuYesNo) — the shared base table.
        assert_eq!(rec.menu_field_choices("NTM"), Some(&["NO", "YES"][..]));
        // HLSV is menu(menuAlarmSevr); its record-specific field name is not
        // in the base registry's standard-severity key set, so it is mapped
        // per record.
        assert_eq!(
            rec.menu_field_choices("HLSV"),
            Some(&["NO_ALARM", "MINOR", "MAJOR", "INVALID"][..])
        );
        // UEIP/URIP share menu(motorUEIP).
        assert_eq!(
            rec.menu_field_choices("UEIP"),
            rec.menu_field_choices("URIP")
        );
        assert_eq!(rec.menu_field_choices("VAL"), None);
    }
}
