mod command_planner;
mod field_access;
mod state_machine;
mod status_update;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record, RecordProcessResult};
use epics_base_rs::types::EpicsValue;

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
            Ok(ProcessOutcome::complete())
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
}
