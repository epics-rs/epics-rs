use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::record::{ProcessAction, Record};
use epics_base_rs::types::EpicsValue;

use crate::records::epid::EpidRecord;

/// Async Soft Channel device support for the epid record.
///
/// Same PID algorithm as `EpidSoftDeviceSupport`, but with an
/// asynchronous readback trigger via the TRIG link.
///
/// Processing flow:
/// 1. First pass (triggered=false): Write TVAL to TRIG link via
///    ProcessAction::WriteDbLink, and request a re-process via
///    ProcessAction::ReprocessAfter(1ms). The TRIG write triggers
///    the readback hardware to update the INP PV.
/// 2. Second pass (triggered=true): INP has been updated by the
///    triggered readback. Run PID with the fresh CVAL.
///
/// Ported from `devEpidSoftCallback.c`.
pub struct EpidSoftCallbackDeviceSupport {
    /// Whether the trigger has been sent and we're on the second pass.
    triggered: bool,
}

impl Default for EpidSoftCallbackDeviceSupport {
    fn default() -> Self {
        Self::new()
    }
}

impl EpidSoftCallbackDeviceSupport {
    pub fn new() -> Self {
        Self { triggered: false }
    }
}

impl DeviceSupport for EpidSoftCallbackDeviceSupport {
    fn dtyp(&self) -> &str {
        "Epid Async Soft"
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let epid = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<EpidRecord>())
            .expect("EpidSoftCallbackDeviceSupport requires an EpidRecord");

        if !self.triggered {
            // First pass: queue TRIG write and request re-process.
            if !epid.trig.is_empty() {
                let actions = vec![
                    ProcessAction::WriteDbLink {
                        link_field: "TRIG",
                        value: EpicsValue::Double(epid.tval),
                    },
                    // Re-process after a short delay to allow triggered device to update
                    ProcessAction::ReprocessAfter(std::time::Duration::from_millis(1)),
                ];
                self.triggered = true;
                return Ok(DeviceReadOutcome::computed_with(actions));
            }
            // No TRIG link — fall through to synchronous PID
        }

        // Second pass (or no TRIG link): execute PID
        self.triggered = false;
        super::epid_soft::EpidSoftDeviceSupport::do_pid(epid);
        Ok(DeviceReadOutcome::computed())
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}
