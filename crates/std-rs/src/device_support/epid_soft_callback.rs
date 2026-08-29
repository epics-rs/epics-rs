use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::record::Record;

use crate::records::epid::EpidRecord;

/// Async Soft Channel device support for the epid record
/// (`devEpidSoftCallback.c`, DSET `devEpidSoftCB`).
///
/// Same PID algorithm as `EpidSoftDeviceSupport`, plus the asynchronous
/// readback trigger on the TRIG link. The trigger itself is NOT emitted
/// from here: `stdSupport.dbd:14` selects this DSET with DTYP
/// `"Async Soft Channel"`, which `is_soft_dtyp` classifies as a soft
/// channel, so the framework never attaches a device for it and this
/// `read()` never runs for a record loaded from a `.db`. Both TRIG link
/// types are therefore fired by [`EpidRecord::pre_input_link_actions`],
/// which is reached on the record-internal soft path — one owner, one
/// push site. This impl stays as the record type's declared DSET (the
/// same vestigial role `EpidSoftDeviceSupport` plays for
/// `"Soft Channel"`) and runs the PID if something does attach it.
pub struct EpidSoftCallbackDeviceSupport;

impl Default for EpidSoftCallbackDeviceSupport {
    fn default() -> Self {
        Self::new()
    }
}

impl EpidSoftCallbackDeviceSupport {
    pub fn new() -> Self {
        Self
    }
}

impl DeviceSupport for EpidSoftCallbackDeviceSupport {
    fn dtyp(&self) -> &str {
        "Async Soft Channel"
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let epid = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<EpidRecord>())
            .expect("EpidSoftCallbackDeviceSupport requires an EpidRecord");
        super::epid_soft::EpidSoftDeviceSupport::do_pid(epid);
        // Same as the synchronous soft dset: C `devEpidSoft.c` writes no
        // `pepid->udf` (see `epid_soft.rs`).
        Ok(DeviceReadOutcome::computed(DeviceUdf::Untouched))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}
