use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::motor::AsynMotor;
use asyn_rs::user::AsynUser;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::record::{Record, ScanType};
use tokio::sync::mpsc;

use crate::device_state::*;
use crate::flags::*;
use crate::poll_loop::PollCommand;

/// Motor device support — bridges MotorRecord to AsynMotor driver.
pub struct MotorDeviceSupport {
    motor: Arc<Mutex<dyn AsynMotor>>,
    _addr: i32,
    _timeout: Duration,
    poll_cmd_tx: mpsc::Sender<PollCommand>,
    io_intr_tx: mpsc::Sender<()>,
    io_intr_rx: Option<mpsc::Receiver<()>>,
    device_state: SharedDeviceState,
    initialized: bool,
    dtyp_name: String,
}

impl MotorDeviceSupport {
    pub fn new(
        motor: Arc<Mutex<dyn AsynMotor>>,
        addr: i32,
        timeout: Duration,
        poll_cmd_tx: mpsc::Sender<PollCommand>,
        device_state: SharedDeviceState,
    ) -> Self {
        let (io_intr_tx, io_intr_rx) = mpsc::channel(16);
        Self {
            motor,
            _addr: addr,
            _timeout: timeout,
            poll_cmd_tx,
            io_intr_tx,
            io_intr_rx: Some(io_intr_rx),
            device_state,
            initialized: false,
            dtyp_name: "asynMotor".to_string(),
        }
    }

    /// Set a custom DTYP name (for simMotorCreate-based registration).
    pub fn with_dtyp_name(mut self, name: String) -> Self {
        self.dtyp_name = name;
        self
    }

    /// Get the io_intr sender (for poll loop to trigger record re-processing).
    pub fn io_intr_sender(&self) -> mpsc::Sender<()> {
        self.io_intr_tx.clone()
    }

    fn make_user(&self) -> AsynUser {
        AsynUser::new(0)
    }

    /// Execute motor commands and manage poll loop from DeviceActions.
    fn execute_actions(&self, actions: &DeviceActions) {
        let user = self.make_user();
        let mut motor = match self.motor.lock() {
            Ok(m) => m,
            Err(e) => {
                tracing::error!("motor lock poisoned: {e}");
                return;
            }
        };

        for cmd in &actions.commands {
            let result = match cmd {
                MotorCommand::MoveAbsolute {
                    position,
                    velocity,
                    acceleration,
                } => {
                    tracing::info!("motor command: MoveAbsolute pos={position}, vel={velocity}");
                    motor.move_absolute(&user, *position, *velocity, *acceleration)
                }
                MotorCommand::MoveVelocity {
                    direction,
                    velocity,
                    acceleration,
                } => {
                    let target = if *direction { 1e9 } else { -1e9 };
                    tracing::info!("motor command: MoveVelocity dir={direction}, vel={velocity}");
                    motor.move_absolute(&user, target, *velocity, *acceleration)
                }
                MotorCommand::Home {
                    forward,
                    velocity,
                    acceleration: _,
                } => {
                    tracing::info!("motor command: Home forward={forward}");
                    motor.home(&user, *velocity, *forward)
                }
                MotorCommand::Stop { acceleration } => {
                    tracing::info!("motor command: Stop");
                    motor.stop(&user, *acceleration)
                }
                MotorCommand::SetPosition { position } => {
                    tracing::info!("motor command: SetPosition pos={position}");
                    motor.set_position(&user, *position)
                }
                MotorCommand::SetClosedLoop { enable } => {
                    tracing::info!("motor command: SetClosedLoop enable={enable}");
                    motor.set_closed_loop(&user, *enable)
                }
                MotorCommand::Poll => Ok(()),
            };

            if let Err(e) = result {
                tracing::error!("motor command error: {e}");
            }
        }
        drop(motor);

        // Manage poll loop
        match actions.poll {
            PollDirective::Start => {
                let _ = self.poll_cmd_tx.try_send(PollCommand::StartPolling);
            }
            PollDirective::Stop => {
                let _ = self.poll_cmd_tx.try_send(PollCommand::StopPolling);
            }
            PollDirective::None => {}
        }
        if let Some(ref delay) = actions.schedule_delay {
            let _ = self
                .poll_cmd_tx
                .try_send(PollCommand::ScheduleDelay(delay.id, delay.duration));
        }
    }
}

impl DeviceSupport for MotorDeviceSupport {
    fn init(&mut self, record: &mut dyn Record) -> CaResult<()> {
        // Inject device_state into MotorRecord (for template-created records)
        let motor_rec = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<crate::record::MotorRecord>());

        if let Some(motor_rec) = motor_rec {
            motor_rec.set_device_state(self.device_state.clone());
        }

        let user = self.make_user();
        let status = {
            let mut motor = self.motor.lock().map_err(|e| {
                epics_base_rs::error::CaError::InvalidValue(format!("motor lock: {e}"))
            })?;
            motor.poll(&user).map_err(|e| {
                epics_base_rs::error::CaError::InvalidValue(format!("motor poll: {e}"))
            })?
        };

        let mut ds = self.device_state.lock().map_err(|e| {
            epics_base_rs::error::CaError::InvalidValue(format!("device state lock: {e}"))
        })?;
        ds.latest_status = Some(StampedStatus {
            seq: 1,
            status: status.clone(),
        });
        drop(ds);

        // Apply initial status to record (sets RBV, clears LVIO, etc.)
        if let Some(motor_rec) = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<crate::record::MotorRecord>())
        {
            motor_rec.process_motor_info(&status);
        }

        self.initialized = true;
        Ok(())
    }

    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::ok())
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        // Extract actions atomically from shared state
        let actions = {
            let mut ds = self.device_state.lock().map_err(|e| {
                epics_base_rs::error::CaError::InvalidValue(format!("device state lock: {e}"))
            })?;
            ds.pending_actions.take()
        };
        let Some(actions) = actions else {
            return Ok(());
        };

        self.execute_actions(&actions);
        Ok(())
    }

    fn dtyp(&self) -> &str {
        &self.dtyp_name
    }

    fn set_record_info(&mut self, _name: &str, _scan: ScanType) {}

    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        self.io_intr_rx.take()
    }
}
