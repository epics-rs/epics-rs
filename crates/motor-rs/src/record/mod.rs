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
        // If wired to device state, determine event from shared mailbox
        if self.device_state.is_some() {
            if let Some(event) = self.determine_event() {
                self.pending_event = Some(event);
            }
        }

        let effects = self.do_process();
        let move_started = !self.stat.dmov && !effects.commands.is_empty();

        // Write effects to shared mailbox for DeviceSupport.write() to consume
        if let Some(state) = self.device_state.clone() {
            self.suppress_flnk = effects.suppress_forward_link;
            let actions = self.effects_to_actions(&effects);
            match state.lock() {
                Ok(mut ds) => {
                    ds.pending_actions = Some(actions);
                }
                Err(e) => {
                    tracing::error!("device state lock poisoned in process: {e}");
                }
            }
        }

        if move_started {
            // Flush DMOV=0 immediately so monitors see the 1->0 transition
            // before the move completes. The next I/O Intr cycle will
            // process again and eventually notify DMOV=1.
            use epics_base_rs::types::EpicsValue;
            let fields = vec![
                ("DMOV".to_string(), EpicsValue::Short(0)),
                ("MOVN".to_string(), EpicsValue::Short(1)),
                ("VAL".to_string(), EpicsValue::Double(self.pos.val)),
                ("DVAL".to_string(), EpicsValue::Double(self.pos.dval)),
                ("RVAL".to_string(), EpicsValue::Long(self.pos.rval)),
            ];
            Ok(ProcessOutcome {
                result: RecordProcessResult::AsyncPendingNotify(fields),
                actions: Vec::new(),
                device_did_compute: false,
            })
        } else {
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
}
