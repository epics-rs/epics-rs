//! devMotorAsyn's init_controller writes run through pasynFloat64SyncIO
//! (devMotorAsyn.c:190, :223): the driver's callback, which statusCallback
//! copies into `pPvt->status` while dbScanLockOK is still false
//! (devMotorAsyn.c:765-767), has run when each write returns, so
//! motorRecord's update_values (motorRecord.cc:690) reads the status those
//! writes posted.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::device_support::DeviceSupport;
use motor_rs::MotorRecord;
use motor_rs::builder::ControllerBuilder;
use motor_rs::device_support::StatusPoster;
use motor_rs::flags::{MstaFlags, RestoreMode};
use motor_rs::poll_loop::PollCommand;
use motor_rs::trace::TraceUser;

const IDLE: Duration = Duration::from_secs(3600);

/// The status a drvEnsembleAsyn axis posts: its motorAxisSetDouble calls
/// the parameter callback after every set, the refused encoder ratio's too
/// (drvEnsembleAsyn.cc:364-368, :405-409).
fn ensemble_status() -> MotorStatus {
    MotorStatus {
        direction: true,
        done: true,
        has_encoder: true,
        gain_support: true,
        ..MotorStatus::default()
    }
}

/// A model-2 axis whose parameter callback runs only inside the encoder
/// ratio write: no poll posts it.
struct Ensemble(Arc<Mutex<Option<StatusPoster>>>);

impl AsynMotor for Ensemble {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn move_relative(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
        Ok(())
    }
    fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_encoder_ratio(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        if let Some(poster) = &*self.0.lock().unwrap() {
            poster.post(ensemble_status());
        }
        Ok(())
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        *status = MotorStatus::default();
        Ok(())
    }
}

#[tokio::test]
async fn a_model2_record_reads_the_status_its_init_write_posted() {
    let poster = Arc::new(Mutex::new(None));
    let mut controller = ControllerBuilder::new();
    let added = controller.axis(
        Arc::new(Mutex::new(Ensemble(poster.clone()))),
        TraceUser::Model2 {
            port: "initWrites2".into(),
            addr: 0,
        },
    );
    *poster.lock().unwrap() = added.axis.status_poster();
    let poller = controller.build(IDLE, IDLE, 0).start();
    let mut support = added.axis.attach();
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    support.init(&mut rec).unwrap();
    assert_eq!(rec.stat.msta, MstaFlags::from_status(&ensemble_status()));
    added.poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}

/// An asynMotorAxis that loads a position as it is told: the controller
/// sets motorPosition_ to it and calls back (asynMotorController.cpp:288,
/// :360-362).
struct Loads;

impl AsynMotor for Loads {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn move_relative(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
        Ok(())
    }
    fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        *status = MotorStatus {
            done: true,
            ..MotorStatus::default()
        };
        Ok(())
    }
}

/// RSTM=Always loads the autosaved DVAL; the readback init_record adopts
/// is the position that load set, not the zero it read before it.
#[tokio::test]
async fn a_restored_position_is_read_back_from_the_load_s_post() {
    let mut controller = ControllerBuilder::new();
    let added = controller.axis(
        Arc::new(Mutex::new(Loads)),
        TraceUser::Asyn {
            port: "initWrites3".into(),
            addr: 0,
        },
    );
    let poller = controller.build(IDLE, IDLE, 0).start();
    let mut support = added.axis.attach();
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.conv.rstm = RestoreMode::Always;
    rec.pos.dval = 5.0;
    support.init(&mut rec).unwrap();
    assert_eq!((rec.pos.rmp, rec.pos.drbv), (5000, 5.0));
    assert_eq!((rec.pos.val, rec.pos.dval, rec.pos.rval), (5.0, 5.0, 5000));
    added.poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}

/// A drvEnsembleAsyn axis whose POSOFFSET the controller refuses: the
/// position write fails and calls nothing back (drvEnsembleAsyn.cc:355-362,
/// :405-409).
struct RefusesLoad;

impl AsynMotor for RefusesLoad {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn move_relative(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
        Ok(())
    }
    fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: String::new(),
        })
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        *status = MotorStatus::default();
        Ok(())
    }
}

/// A load that ran and was refused leaves the readback where it was, and
/// init_record adopts that, not the autosaved DVAL (motorRecord.cc:706-714).
#[tokio::test]
async fn a_refused_position_load_is_read_back_unloaded() {
    let mut controller = ControllerBuilder::new();
    let added = controller.axis(
        Arc::new(Mutex::new(RefusesLoad)),
        TraceUser::Model2 {
            port: "initWrites5".into(),
            addr: 0,
        },
    );
    let poller = controller.build(IDLE, IDLE, 0).start();
    let mut support = added.axis.attach();
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.conv.rstm = RestoreMode::Always;
    rec.pos.dval = 5.0;
    support.init(&mut rec).unwrap();
    assert_eq!((rec.pos.rmp, rec.pos.drbv), (0, 0.0));
    assert_eq!((rec.pos.val, rec.pos.dval, rec.pos.rval), (0.0, 0.0, 0));
    added.poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}

/// A poller that has ended runs no write: init reads the status it had.
#[tokio::test]
async fn init_on_an_ended_poller_returns() {
    let mut controller = ControllerBuilder::new();
    let added = controller.axis(
        Arc::new(Mutex::new(Loads)),
        TraceUser::Asyn {
            port: "initWrites4".into(),
            addr: 0,
        },
    );
    let poller = controller.build(IDLE, IDLE, 0).start();
    added.poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await;
    let mut support = added.axis.attach();
    let mut rec = MotorRecord::new();
    rec.conv.mres = 0.001;
    rec.conv.rstm = RestoreMode::Always;
    rec.pos.dval = 5.0;
    support.init(&mut rec).unwrap();
    assert_eq!((rec.pos.rmp, rec.pos.dval), (0, 5.0));
}
