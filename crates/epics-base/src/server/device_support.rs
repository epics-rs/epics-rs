use crate::error::CaResult;
use crate::server::record::{Record, ScanType};

/// Handle for waiting on asynchronous write completion.
/// Returned by [`DeviceSupport::write_begin`] when the write is submitted
/// to a worker queue rather than executed synchronously.
pub trait WriteCompletion: Send + 'static {
    /// Block until the write completes or timeout expires.
    fn wait(&self, timeout: std::time::Duration) -> CaResult<()>;
}

/// Trait for custom device support implementations.
/// When DTYP is set to something other than "" or "Soft Channel",
/// the registered DeviceSupport is used instead of link resolution.
pub trait DeviceSupport: Send + Sync + 'static {
    fn init(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
    fn read(&mut self, record: &mut dyn Record) -> CaResult<()>;
    fn write(&mut self, record: &mut dyn Record) -> CaResult<()>;
    fn dtyp(&self) -> &str;

    /// Return the last alarm (status, severity) from the driver.
    /// None means the driver does not override alarms.
    fn last_alarm(&self) -> Option<(u16, u16)> { None }

    /// Return the last timestamp from the driver.
    /// None means the driver does not override timestamps.
    fn last_timestamp(&self) -> Option<std::time::SystemTime> { None }

    /// Called after init() with the record name and scan type.
    fn set_record_info(&mut self, _name: &str, _scan: ScanType) {}

    /// Return a receiver for I/O Intr scan notifications.
    /// Only called for records with SCAN=I/O Intr.
    fn io_intr_receiver(&mut self) -> Option<crate::runtime::sync::mpsc::Receiver<()>> { None }

    /// Begin an asynchronous write (submit only, no blocking).
    /// Returns `Some(handle)` if the write was submitted to a worker queue —
    /// the caller should wait on the handle outside any record lock.
    /// Returns `None` to fall back to synchronous [`write()`](DeviceSupport::write).
    fn write_begin(&mut self, _record: &mut dyn Record) -> CaResult<Option<Box<dyn WriteCompletion>>> {
        Ok(None)
    }
}
