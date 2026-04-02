use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

use crate::records::scaler::{
    CMD_ARM, CMD_RESET, CMD_WRITE_PRESET, MAX_SCALER_CHANNELS, ScalerRecord,
};

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
    fn write_preset(&mut self, channel: usize, preset: u32) -> CaResult<()>;
    fn arm(&mut self, start: bool) -> CaResult<()>;
    fn done(&self) -> bool;
    fn num_channels(&self) -> usize;
}

/// Asyn-based device support for the scaler record.
///
/// `read()` performs check_done + read_counts (pre-process data).
/// `handle_command()` executes reset/write_preset/arm (post-process actions).
pub struct ScalerAsynDeviceSupport {
    driver: Arc<Mutex<Box<dyn ScalerDriver>>>,
}

impl ScalerAsynDeviceSupport {
    pub fn new(driver: Box<dyn ScalerDriver>) -> Self {
        Self {
            driver: Arc::new(Mutex::new(driver)),
        }
    }

    pub fn driver(&self) -> Arc<Mutex<Box<dyn ScalerDriver>>> {
        Arc::clone(&self.driver)
    }
}

impl DeviceSupport for ScalerAsynDeviceSupport {
    fn dtyp(&self) -> &str {
        "Asyn Scaler"
    }

    fn init(&mut self, record: &mut dyn Record) -> CaResult<()> {
        let scaler = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<ScalerRecord>())
            .expect("ScalerAsynDeviceSupport requires a ScalerRecord");

        let driver = self.driver.lock().unwrap();
        scaler.nch = driver.num_channels() as i16;
        Ok(())
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let scaler = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<ScalerRecord>())
            .expect("ScalerAsynDeviceSupport requires a ScalerRecord");

        let mut driver = self.driver.lock().unwrap();

        // Check if counting completed
        if driver.done() {
            scaler.done_flag = true;
        }

        // Read all channel counts into the record
        let mut counts = [0u32; MAX_SCALER_CHANNELS];
        if driver.read(&mut counts).is_ok() {
            scaler.s = counts;
        }

        Ok(DeviceReadOutcome::ok())
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn handle_command(
        &mut self,
        _record: &mut dyn Record,
        command: &str,
        args: &[EpicsValue],
    ) -> CaResult<()> {
        let mut driver = self.driver.lock().unwrap();
        match command {
            CMD_RESET => {
                driver.reset()?;
            }
            CMD_ARM => {
                let start = args
                    .first()
                    .and_then(|v| {
                        if let EpicsValue::Long(i) = v {
                            Some(*i != 0)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(false);
                driver.arm(start)?;
            }
            CMD_WRITE_PRESET => {
                let channel = args
                    .first()
                    .and_then(|v| {
                        if let EpicsValue::Long(i) = v {
                            Some(*i as usize)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                let preset = args
                    .get(1)
                    .and_then(|v| {
                        if let EpicsValue::Long(i) = v {
                            Some(*i as u32)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                driver.write_preset(channel, preset)?;
            }
            _ => {}
        }
        Ok(())
    }
}
