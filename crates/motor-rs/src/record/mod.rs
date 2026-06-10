mod command_planner;
mod field_access;
mod state_machine;
mod status_update;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::record::{
    FieldDesc, MENU_ALARM_SEVR, MENU_YES_NO, ProcessAction, ProcessOutcome, Record,
    RecordProcessResult,
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
    /// Suppress FLNK during motion
    suppress_flnk: bool,
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
            suppress_flnk: false,
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
    /// We mirror that by emitting a `WriteDbLink` for every full-snapshot
    /// (`Complete`) cycle; the framework writes it before FLNK, exactly where C
    /// fires it. The empty link is skipped so motors with no RLNK emit nothing.
    ///
    /// The single move-start cycle returns `AsyncPendingNotify` (DMOV 1→0) and
    /// carries no actions; RBV is unchanged there (the prior `Complete` cycle
    /// already fired RLNK with that RBV), so no readback information is lost.
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

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // DMOV state on entry — C: 0ef39053 fires FLNK only on the
        // DMOV false→true transition (motion completion).
        let dmov_before = self.stat.dmov;

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

        // C: 0ef39053 — FLNK fires only when DMOV transitions false→true.
        // An explicit suppression request (NTM, in-flight retarget) still wins.
        let dmov_completed = !dmov_before && self.stat.dmov;
        self.suppress_flnk = effects.suppress_forward_link || !dmov_completed;

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
            ];
            Ok(ProcessOutcome {
                result: RecordProcessResult::AsyncPendingNotify(fields),
                actions: Vec::new(),
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

    fn should_fire_forward_link(&self) -> bool {
        !self.suppress_flnk
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
    /// applied, establish the limit invariant from the loaded DHLM/DLLM
    /// (C `set_dial_highlimit`/`set_dial_lowlimit`). See
    /// [`field_access::motor_sync_limits_at_init`].
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 1 {
            field_access::motor_sync_limits_at_init(self);
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
        // SET mode produces SetPosition command via process path
        assert_eq!(rec.last_write, Some(CommandSource::Set));
    }

    #[test]
    fn test_should_fire_forward_link() {
        let mut rec = MotorRecord::new();
        assert!(rec.should_fire_forward_link());

        rec.suppress_flnk = true;
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

    // C: 0ef39053 — FLNK fires only on the DMOV false→true transition.
    #[test]
    fn test_flnk_suppressed_on_idle_process_without_transition() {
        let mut rec = MotorRecord::new();
        // Already idle (DMOV=true). A bare process() with no motion must
        // not fire FLNK — there is no false→true transition.
        assert!(rec.stat.dmov);
        let _ = rec.process();
        assert!(
            !rec.should_fire_forward_link(),
            "idle process with no DMOV transition must suppress FLNK"
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
