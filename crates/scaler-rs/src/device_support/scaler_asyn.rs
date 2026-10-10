use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceInitOutcome, DeviceSupport};
use epics_base_rs::server::record::Record;

use crate::records::scaler::{MAX_SCALER_CHANNELS, ScalerRecord};

/// Asyn command strings for scaler drivers.
pub const SCALER_RESET_COMMAND: &str = "SCALER_RESET";
pub const SCALER_CHANNELS_COMMAND: &str = "SCALER_CHANNELS";
pub const SCALER_READ_COMMAND: &str = "SCALER_READ";
pub const SCALER_READ_SINGLE_COMMAND: &str = "SCALER_READ_SINGLE";
pub const SCALER_PRESET_COMMAND: &str = "SCALER_PRESET";
pub const SCALER_ARM_COMMAND: &str = "SCALER_ARM";
pub const SCALER_DONE_COMMAND: &str = "SCALER_DONE";

/// Trait for scaler hardware drivers.
pub trait ScalerDriver: Send + Sync + 'static {
    fn reset(&mut self) -> CaResult<()>;
    fn read(&mut self, counts: &mut [u32; MAX_SCALER_CHANNELS]) -> CaResult<()>;
    /// Program `preset` for `channel` and return the count the hardware
    /// will actually use.
    ///
    /// C parity: `scalerRecord.c:397-404` documents that some scalers
    /// (the Joerger VS64) set their own clock frequency and adjust the
    /// preset to match — "if device support has changed the preset, we
    /// recalc the preset from tp ... and call write_preset again". In C
    /// the dset `write_preset(scalerRecord *psr, ...)` mutates
    /// `psr->pr1` in place; the record then reads it back. The Rust
    /// trait makes that explicit by returning the programmed value.
    /// A driver that does not quantize simply returns `preset`
    /// unchanged (see `SoftScalerDriver`).
    fn write_preset(&mut self, channel: usize, preset: u32) -> CaResult<u32>;
    fn arm(&mut self, start: bool) -> CaResult<()>;
    /// The clock frequency (Hz) the driver is actually running at.
    ///
    /// C parity: `scalerRecord.c:399-403` — the VS64 "sets its own
    /// clock frequency"; `updateCounts` (`scalerRecord.c:585-587`) and
    /// the REQSTART reconciliation (`scalerRecord.c:424-430`) use the
    /// frequency the driver chose, not the one originally requested.
    /// A driver that does not own the clock returns `None`, leaving
    /// `FREQ` untouched.
    fn actual_frequency(&self) -> Option<f64> {
        None
    }
    /// Read-and-clear the "counting done" flag.
    ///
    /// C `devScalerAsyn.c:292-301` `scaler_done()` is the dset entry the
    /// record polls each process cycle; it returns 1 exactly once per
    /// completed count and clears `pPvt->done` on that read so the next
    /// poll returns 0. This signature takes `&mut self` so the flag can
    /// be consumed — a `&self` version cannot replicate the clear.
    fn done(&mut self) -> bool;
    fn num_channels(&self) -> usize;
}

/// Asyn-based device support for the scaler record — C `devScalerAsyn`.
///
/// C's dset is a set of synchronous entries the record calls from inside
/// `process()`; `init` binds the driver to the record as that dset
/// ([`ScalerRecord::bind_dset`]), so nothing is read before `process()` or
/// commanded after it.
pub struct ScalerAsynDeviceSupport {
    driver: Arc<Mutex<Box<dyn ScalerDriver>>>,
}

impl ScalerAsynDeviceSupport {
    pub fn new(driver: Box<dyn ScalerDriver>) -> Self {
        Self {
            driver: Arc::new(Mutex::new(driver)),
        }
    }

    /// Device support on a driver other records may share — one asyn port
    /// serving several scaler records.
    pub fn shared(driver: Arc<Mutex<Box<dyn ScalerDriver>>>) -> Self {
        Self { driver }
    }

    pub fn driver(&self) -> Arc<Mutex<Box<dyn ScalerDriver>>> {
        Arc::clone(&self.driver)
    }
}

impl DeviceSupport for ScalerAsynDeviceSupport {
    fn dtyp(&self) -> &str {
        "Asyn Scaler"
    }

    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        let scaler = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<ScalerRecord>())
            .expect("ScalerAsynDeviceSupport requires a ScalerRecord");

        // A custom `ScalerDriver` may report any channel count. Clamp to the
        // physical array bound so the record's fixed 64-element arrays are
        // never indexed out of range.
        scaler.nch = self
            .driver
            .lock()
            .unwrap()
            .num_channels()
            .min(MAX_SCALER_CHANNELS) as i16;
        scaler.bind_dset(self.driver());
        Ok(DeviceInitOutcome::Live)
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}
