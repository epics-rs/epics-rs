use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::motor::AsynMotor;
use asyn_rs::user::AsynUser;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceInitOutcome, DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::types::EpicsValue;
use tokio::sync::mpsc;

use crate::device_state::*;
use crate::poll_loop::{MotorCommandSender, PollCommand};

/// Motor device support — bridges MotorRecord to AsynMotor driver.
pub struct MotorDeviceSupport {
    /// The driver, for the synchronous `init` readback only (C init_record);
    /// taken there, so later record passes can only queue commands on the
    /// poller thread.
    motor: Option<Arc<Mutex<dyn AsynMotor>>>,
    motor_cmd_tx: MotorCommandSender,
    _addr: i32,
    _timeout: Duration,
    poll_cmd_tx: mpsc::Sender<PollCommand>,
    io_intr_tx: mpsc::Sender<()>,
    io_intr_rx: Option<mpsc::Receiver<()>>,
    device_state: SharedDeviceState,
    initialized: bool,
    dtyp_name: String,
    polling_active: bool,
}

impl MotorDeviceSupport {
    pub fn new(
        motor: Arc<Mutex<dyn AsynMotor>>,
        addr: i32,
        timeout: Duration,
        poll_cmd_tx: mpsc::Sender<PollCommand>,
        motor_cmd_tx: MotorCommandSender,
        device_state: SharedDeviceState,
    ) -> Self {
        let (io_intr_tx, io_intr_rx) = mpsc::channel(16);
        Self {
            motor: Some(motor),
            motor_cmd_tx,
            _addr: addr,
            _timeout: timeout,
            poll_cmd_tx,
            io_intr_tx,
            io_intr_rx: Some(io_intr_rx),
            device_state,
            initialized: false,
            dtyp_name: "asynMotor".to_string(),
            polling_active: false,
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

    /// Queue the commands on the poller thread and manage the poll loop.
    fn execute_actions(&mut self, mut actions: DeviceActions) {
        let commands = std::mem::take(&mut actions.commands);
        if !commands.is_empty() && self.motor_cmd_tx.send(commands).is_err() {
            tracing::error!("motor: poller gone, commands dropped");
        }

        // Manage poll loop — only send StartPolling when transitioning
        // idle → active to avoid redundant messages while already polling.
        // The polling_active tracking flag is updated only when the command
        // is actually delivered; otherwise record state and poller state
        // would diverge (record thinks it polls, poller is idle). A failed
        // send leaves the flag unchanged so the next process() retries.
        match actions.poll {
            PollDirective::Start => {
                if !self.polling_active {
                    match self.poll_cmd_tx.try_send(PollCommand::StartPolling) {
                        Ok(()) => self.polling_active = true,
                        Err(e) => {
                            tracing::error!("motor: failed to send StartPolling: {e}")
                        }
                    }
                }
            }
            // Forced refresh (request_poll / status_refresh): send StartPolling
            // UNCONDITIONALLY — the dedup is bypassed so the forced status post
            // reaches the loop even while it is already polling (C
            // motorUpdateStatus_ forces a poll+callback regardless of the
            // poller's running state). The poll loop force-notifies on a
            // StartPolling-triggered poll, so STUP=BUSY clears on a stationary
            // axis. request_poll / status_refresh are discrete events (not set
            // every in-motion pass), so this does not flood the poller.
            PollDirective::Refresh => match self.poll_cmd_tx.try_send(PollCommand::StartPolling) {
                Ok(()) => self.polling_active = true,
                Err(e) => tracing::error!("motor: failed to send forced poll: {e}"),
            },
            PollDirective::Stop => match self.poll_cmd_tx.try_send(PollCommand::StopPolling) {
                Ok(()) => self.polling_active = false,
                Err(e) => tracing::error!("motor: failed to send StopPolling: {e}"),
            },
            PollDirective::None => {}
        }
        if let Some(ref delay) = actions.schedule_delay {
            match self
                .poll_cmd_tx
                .try_send(PollCommand::ScheduleDelay(delay.id, delay.duration))
            {
                // Poll loop goes idle during the delay — sync the flag.
                Ok(()) => self.polling_active = false,
                Err(e) => tracing::error!("motor: failed to schedule settle delay: {e}"),
            }
        }
    }
}

impl DeviceSupport for MotorDeviceSupport {
    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        // Inject device_state into MotorRecord (for template-created records)
        let motor_rec = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<crate::record::MotorRecord>());

        if let Some(motor_rec) = motor_rec {
            motor_rec.set_device_state(self.device_state.clone());
        }

        // NO controller reseed here. The RSTM/loadpos(#231)/MRES(#196)
        // restore decision is owned entirely by `initial_readback`
        // (status_update.rs), which fires on the record `Startup` event and
        // emits `MotorCommand::SetPosition` through the normal command path
        // (effects → pending_actions → DeviceSupport::write → set_position).
        //
        // C `devMotorAsyn.c::init_controller` (166-239) reseeds only when
        // `initPos == 1`, decided by the RSTM switch testing the controller's
        // *actual current position* (`pPvt->status.position`, fetched before
        // init_controller). RSTM=Never never reseeds; NearZero reseeds only
        // when the controller currently sits near zero (`dval_non_zero_pos_
        // near_zero`) while DVAL is meaningful. An earlier Rust version
        // reseeded *unconditionally* on any pass0 restore, before the first
        // poll — so a controller that kept its true absolute position across
        // an IOC restart (default RSTM=NearZero, or RSTM=Never, or an
        // absolute encoder with LOADPOS_BLOCK) was clobbered with the stale
        // autosaved DVAL every boot — the exact failure RSTM/#231 prevent.
        // The poll below now reads the controller's true position into the
        // shared status so `initial_readback` can apply the RSTM gate to it.
        let user = self.make_user();
        let motor = self.motor.take().ok_or_else(|| {
            epics_base_rs::error::CaError::InvalidValue(
                "motor device support re-initialized".into(),
            )
        })?;

        let status = {
            let mut motor = motor.lock().map_err(|e| {
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
        // Clear last_write so pass0-restored values are not interpreted as
        // move commands during PINI processing (matches C EPICS init_record).
        // This is part of the C init readback, so it uses `initcall = true`
        // (URIP RDBL scaling suppressed, motor position adopted).
        if let Some(motor_rec) = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<crate::record::MotorRecord>())
        {
            motor_rec.process_motor_info_initcall(&status, true);
            motor_rec.clear_last_write();
        }

        // C init_record 717-718 ("Reset limits in case database values
        // are invalid"): both dial limits are forwarded to the driver
        // unconditionally after the initial readback, high first.
        let dial_limits = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<crate::record::MotorRecord>())
            .map(|rec| {
                let read =
                    |rec: &mut crate::record::MotorRecord, name: &str| match rec.get_field(name) {
                        Some(EpicsValue::Double(d)) => d,
                        _ => 0.0,
                    };
                (read(rec, "DHLM"), read(rec, "DLLM"))
            });
        if let Some((dhlm, dllm)) = dial_limits {
            let mut motor = motor.lock().map_err(|e| {
                epics_base_rs::error::CaError::InvalidValue(format!("motor lock: {e}"))
            })?;
            // Errors are non-fatal, as in C (init_record ignores the
            // build_trans return for these two sends).
            let _ = motor.set_high_limit(&user, dhlm);
            let _ = motor.set_low_limit(&user, dllm);
        }

        self.initialized = true;
        Ok(DeviceInitOutcome::ok())
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

        self.execute_actions(actions);
        Ok(())
    }

    fn dtyp(&self) -> &str {
        &self.dtyp_name
    }

    fn set_record_info(&mut self, _name: &str, _scan: ScanType) {}

    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        self.io_intr_rx.take()
    }

    /// The motor's poll `statusCallback` drives `dbProcess` on every readback
    /// regardless of `SCAN` (C `motorRecord` parity), and the record stays
    /// `SCAN="Passive"` so a `dbPutField` to a `pp(TRUE)` motion field
    /// (VAL/DVAL/RVAL/RLV/JOG/HOME/...) still re-processes it. Decouple the
    /// poll-feedback wiring from the `SCAN` menu.
    fn io_intr_scan_independent(&self) -> bool {
        true
    }
}
