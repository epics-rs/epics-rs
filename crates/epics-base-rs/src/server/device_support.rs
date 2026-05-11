use crate::error::CaResult;
use crate::server::record::{ProcessAction, Record, ScanType};

/// Check if a DTYP string represents a soft/built-in device support
/// that doesn't require an explicit device support registration.
/// Matches C EPICS built-in soft device support names.
pub fn is_soft_dtyp(dtyp: &str) -> bool {
    dtyp.is_empty()
        || dtyp == "Soft Channel"
        || dtyp == "Raw Soft Channel"
        || dtyp == "Async Soft Channel"
        || dtyp == "Soft Timestamp"
        || dtyp == "Sec Past Epoch"
}

/// Handle for waiting on asynchronous write completion.
/// Returned by [`DeviceSupport::write_begin`] when the write is submitted
/// to a worker queue rather than executed synchronously.
pub trait WriteCompletion: Send + 'static {
    /// Block until the write completes or timeout expires.
    fn wait(&self, timeout: std::time::Duration) -> CaResult<()>;
}

/// Outcome of a device support read() call.
///
/// Allows device support to return side-effect actions (link writes,
/// delayed reprocess) and signal that it has already performed the
/// Result of a device support `read()` call.
///
/// # `ok()` vs `computed()`
///
/// This mirrors the C EPICS `read_ai()` return convention:
///
/// - **`ok()`** (C return 0): Device support wrote to RVAL. The record's
///   `process()` will run its built-in conversion (e.g., ai applies
///   `ROFF → ASLO/AOFF → LINR/ESLO/EOFF → smoothing` to produce VAL
///   from RVAL).
///
/// - **`computed()`** (C return 2): Device support wrote to VAL directly.
///   The record's `process()` will **skip** its conversion and use the
///   VAL as-is. Use this when the device support provides engineering
///   units directly (e.g., soft channel, asyn, custom drivers that
///   call `record.put_field("VAL", ...)`).
///
/// **Common mistake:** returning `ok()` when VAL is set directly causes
/// the record's conversion to overwrite VAL with a value derived from
/// RVAL (typically 0), making the read appear broken.
#[derive(Default)]
pub struct DeviceReadOutcome {
    /// Actions for the framework to execute (WriteDbLink, ReprocessAfter, etc.)
    pub actions: Vec<ProcessAction>,
    /// If true, the record's built-in conversion (e.g., ai RVAL→VAL)
    /// is skipped. Set this when device support writes VAL directly.
    pub did_compute: bool,
}

impl DeviceReadOutcome {
    /// Device support wrote RVAL; record will run its conversion to produce VAL.
    ///
    /// C equivalent: `read_ai()` returns 0.
    pub fn ok() -> Self {
        Self::default()
    }

    /// Device support wrote VAL directly; record will skip conversion.
    ///
    /// C equivalent: `read_ai()` returns 2.
    pub fn computed() -> Self {
        Self {
            did_compute: true,
            actions: Vec::new(),
        }
    }

    /// Shorthand for a computed read with actions.
    pub fn computed_with(actions: Vec<ProcessAction>) -> Self {
        Self {
            did_compute: true,
            actions,
        }
    }
}

/// Trait for custom device support implementations.
/// When DTYP is set to something other than "" or "Soft Channel",
/// the registered DeviceSupport is used instead of link resolution.
pub trait DeviceSupport: Send + Sync + 'static {
    fn init(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    /// Read from hardware into the record.
    ///
    /// Returns a `DeviceReadOutcome` containing:
    /// - `actions`: side-effect actions (link writes, delayed reprocess)
    ///   that the framework will execute after process()
    /// - `did_compute`: if true, the record's built-in compute was already
    ///   performed (e.g., device support ran PID), so process() should skip it
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let _ = record;
        Ok(DeviceReadOutcome::ok())
    }

    fn write(&mut self, record: &mut dyn Record) -> CaResult<()>;
    fn dtyp(&self) -> &str;

    /// Return the last alarm (status, severity) from the driver.
    /// None means the driver does not override alarms.
    fn last_alarm(&self) -> Option<(u16, u16)> {
        None
    }

    /// Return the last timestamp from the driver.
    /// None means the driver does not override timestamps.
    fn last_timestamp(&self) -> Option<std::time::SystemTime> {
        None
    }

    /// Called after init() with the record name and scan type.
    fn set_record_info(&mut self, _name: &str, _scan: ScanType) {}

    /// Forward parsed `info("key", "value")` directives from the .db
    /// file to the device support. Default is a no-op; drivers that
    /// react to specific tags (asyn `asyn:READBACK`, EtherCAT terminal
    /// hints, etc.) override this. Called once after `set_record_info`
    /// during builder wiring; not called again at runtime.
    fn apply_record_info(&mut self, _info: &std::collections::HashMap<String, String>) {}

    /// Return a receiver for I/O Intr scan notifications.
    /// Only called for records with SCAN=I/O Intr.
    fn io_intr_receiver(&mut self) -> Option<crate::runtime::sync::mpsc::Receiver<()>> {
        None
    }

    /// Begin an asynchronous write (submit only, no blocking).
    /// Returns `Some(handle)` if the write was submitted to a worker queue —
    /// the caller should wait on the handle outside any record lock.
    /// Returns `None` to fall back to synchronous [`write()`](DeviceSupport::write).
    fn write_begin(
        &mut self,
        _record: &mut dyn Record,
    ) -> CaResult<Option<Box<dyn WriteCompletion>>> {
        Ok(None)
    }

    /// Handle a named command from the record's process() via
    /// `ProcessAction::DeviceCommand`. This allows records to request
    /// driver operations (e.g., scaler reset/arm/write_preset) without
    /// holding a direct driver reference.
    ///
    /// Default: ignore.
    fn handle_command(
        &mut self,
        _record: &mut dyn Record,
        _command: &str,
        _args: &[crate::types::EpicsValue],
    ) -> CaResult<()> {
        Ok(())
    }
}
