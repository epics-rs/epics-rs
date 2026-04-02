use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::physics::{self, BeamCurrentConfig};

/// Shared beam current value, updated by a background thread.
/// Stores f64 bits in AtomicU64 for lock-free access.
pub struct BeamCurrentValue {
    bits: AtomicU64,
}

impl BeamCurrentValue {
    pub fn new() -> Self {
        Self {
            bits: AtomicU64::new(physics::beam_current(0.0).to_bits()),
        }
    }

    pub fn get(&self) -> f64 {
        f64::from_bits(self.bits.load(Ordering::Relaxed))
    }

    fn set(&self, value: f64) {
        self.bits.store(value.to_bits(), Ordering::Relaxed);
    }
}

/// Start a background thread that updates beam current.
/// `update_interval_ms` controls the update rate.
pub fn start_beam_current_thread(
    value: Arc<BeamCurrentValue>,
    config: BeamCurrentConfig,
    update_interval_ms: u64,
) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Receiver<()>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("BeamCurrent".into())
        .spawn(move || {
            let start = Instant::now();
            loop {
                let t = start.elapsed().as_secs_f64();
                value.set(physics::beam_current_with_config(t, &config));
                let _ = tx.send(());
                std::thread::sleep(std::time::Duration::from_millis(update_interval_ms));
            }
        })
        .expect("failed to spawn BeamCurrent thread");
    (handle, rx)
}

#[cfg(feature = "ioc")]
pub mod ioc_support {
    use super::*;
    use epics_base_rs::error::CaResult;
    use epics_base_rs::server::device_support::{
        DeviceReadOutcome, DeviceSupport, WriteCompletion,
    };
    use epics_base_rs::server::record::{Record, ScanType};

    /// Device support for the beam current AI record.
    /// Reads the latest beam current from the shared atomic value.
    pub struct BeamCurrentDeviceSupport {
        value: Arc<BeamCurrentValue>,
        io_intr_rx: Option<epics_base_rs::runtime::sync::mpsc::Receiver<()>>,
        /// Bridge from std::sync::mpsc to epics runtime mpsc
        _bridge_handle: Option<std::thread::JoinHandle<()>>,
    }

    impl BeamCurrentDeviceSupport {
        pub fn new(value: Arc<BeamCurrentValue>, std_rx: std::sync::mpsc::Receiver<()>) -> Self {
            let (tokio_tx, tokio_rx) = epics_base_rs::runtime::sync::mpsc::channel(4);
            let bridge = std::thread::Builder::new()
                .name("BeamCurrentBridge".into())
                .spawn(move || {
                    while std_rx.recv().is_ok() {
                        if tokio_tx.blocking_send(()).is_err() {
                            break;
                        }
                    }
                })
                .expect("failed to spawn bridge thread");

            Self {
                value,
                io_intr_rx: Some(tokio_rx),
                _bridge_handle: Some(bridge),
            }
        }
    }

    impl DeviceSupport for BeamCurrentDeviceSupport {
        fn dtyp(&self) -> &str {
            "miniBeamCurrent"
        }

        fn set_record_info(&mut self, _name: &str, _scan: ScanType) {}

        fn init(&mut self, record: &mut dyn Record) -> CaResult<()> {
            let val = self.value.get();
            record.put_field("VAL", epics_base_rs::types::EpicsValue::Double(val))?;
            Ok(())
        }

        fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
            let val = self.value.get();
            record.put_field("VAL", epics_base_rs::types::EpicsValue::Double(val))?;
            Ok(DeviceReadOutcome::ok())
        }

        fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
            Ok(())
        }

        fn write_begin(
            &mut self,
            _record: &mut dyn Record,
        ) -> CaResult<Option<Box<dyn WriteCompletion>>> {
            Ok(None)
        }

        fn last_alarm(&self) -> Option<(u16, u16)> {
            None
        }

        fn last_timestamp(&self) -> Option<std::time::SystemTime> {
            None
        }

        fn io_intr_receiver(&mut self) -> Option<epics_base_rs::runtime::sync::mpsc::Receiver<()>> {
            self.io_intr_rx.take()
        }
    }
}
