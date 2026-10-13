//! `asynRecordDevice` — the asyn record's own device support.
//!
//! C `devAsynRecord.dbd`: `device(asyn, INST_IO, asynRecordDevice,
//! "asynRecordDevice")`, and the DSET itself (asynRecord.c:266-267) is
//! `{5, 0, 0, 0, getIoIntInfo, 0}` — no `init_record`, no `read`, no `write`.
//! Its **only** job is to hand `dbScan` the record's I/O Intr scan list, which
//! is why a `record(asyn, ...)` without `field(DTYP, "asynRecordDevice")`
//! cannot use `SCAN="I/O Intr"` at all: `scanAdd` finds `precord->dset == NULL`
//! and demotes it to Passive (dbScan.c:269-274). The port reproduces both halves
//! — this DSET, and the same demotion in `ioc_app::setup_io_intr`.
//!
//! Everything the interrupt mode actually does lives in the record's
//! `IoIntrScan`, exactly as C keeps
//! `registerInterrupts` / the callbacks / `gotValue` in `asynRecPvt`. This type
//! is the seam, not the mechanism.

use std::sync::Arc;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceInitOutcome, DeviceSupport};
use epics_base_rs::server::record::Record;

use super::AsynRecord;
use super::io_intr::IoIntrScan;

/// The DTYP `asynRecord.db` binds (`field(DTYP, "asynRecordDevice")`).
pub const ASYN_RECORD_DTYP: &str = "asynRecordDevice";

pub struct AsynRecordDevice {
    /// The record's own I/O Intr machinery, taken at `init` — the C DSET
    /// reaches it through `pasynRec->dpvt` the same way.
    scan: Option<Arc<IoIntrScan>>,
}

impl Default for AsynRecordDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl AsynRecordDevice {
    pub fn new() -> Self {
        Self { scan: None }
    }
}

impl DeviceSupport for AsynRecordDevice {
    /// C's DSET has no `init_record` slot (asynRecord.c:266). This is not that:
    /// it is where the DSET picks up its `dpvt` — the record's `IoIntrScan`.
    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        self.scan = record
            .as_any_mut()
            .and_then(|any| any.downcast_mut::<AsynRecord>())
            .map(|rec| rec.io_intr_scan());
        Ok(DeviceInitOutcome::Live)
    }

    /// C's DSET has no `write` (slot 5 is 0): the asyn record is not an output
    /// record and the framework never reaches this. Required by the trait.
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        ASYN_RECORD_DTYP
    }

    /// C `getIoIntInfo`'s `*iopvt = pasynRecPvt->ioScanPvt` (asynRecord.c:595) —
    /// the scan list the driver callbacks fire into. Taking it also arms the
    /// record's `interruptAccept` gate: this call happens at `iocInit`, which is
    /// the first moment an interrupt may legally drive a scan (:716,:738,:759,:780).
    fn io_intr_receiver(&mut self) -> Option<tokio::sync::mpsc::Receiver<()>> {
        self.scan.as_ref()?.take_receiver()
    }

    /// The subscription itself is the SCAN gate.
    ///
    /// C registers the driver interrupt callbacks in `getIoIntInfo(0)` and
    /// cancels them in `getIoIntInfo(1)`, so a value is pushed to the record
    /// **only** while it is on the I/O Intr scan list —
    /// `IoIntrScan` holds that invariant by
    /// construction. The framework's alternative gate (re-check `SCAN` on every
    /// wakeup) would be a second, weaker copy of the same rule; declaring the
    /// device SCAN-independent says "the source decides", which is what C does.
    /// No wakeup can exist here without an armed registration, and no
    /// registration can exist with SCAN off `I/O Intr`.
    fn io_intr_scan_independent(&self) -> bool {
        true
    }
}
