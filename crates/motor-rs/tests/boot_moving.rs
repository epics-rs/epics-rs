//! C init_record sets DMOV and clears MOVN (motorRecord.cc:720-724) and
//! queues no GET_INFO for an axis already moving; its poller posts only a
//! changed status, so a move that never changes leaves the record so.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure};
use asyn_rs::registry::open_owner;
use asyn_rs::user::AsynUser;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::{ControllerAxis, ControllerBuilder};
use motor_rs::devices::{device_support_factory, devices, register_axis, register_controller_port};
use motor_rs::poll_loop::PollCommand;
use motor_rs::trace::TraceUser;

/// Moving at a standstill: every poll reads the same status, each after
/// the startup pass's slow enough to land past iocInit.
struct Stuck {
    polls: u32,
}

impl AsynMotor for Stuck {
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
        self.polls += 1;
        if self.polls > 1 {
            std::thread::sleep(Duration::from_millis(300));
        }
        *status = MotorStatus {
            position: 7.0,
            moving: true,
            ..MotorStatus::default()
        };
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_moving_axis_boots_with_dmov_set() {
    register_controller_port(&open_owner(), "BOOTMV", 1, ()).unwrap();
    let mut controller = ControllerBuilder::new();
    let ControllerAxis { axis, poll_cmd_tx } = controller.axis(
        Arc::new(Mutex::new(Stuck { polls: 0 })),
        TraceUser::Asyn {
            port: "BOOTMV".into(),
            addr: 0,
        },
    );
    // No timed pass after the startup pass: only a request polls again.
    let hour = Duration::from_secs(3600);
    let first = controller.axes();
    let poller = controller.build(hour, hour, 0).start();
    let deadline = Instant::now() + Duration::from_secs(5);
    while first.first_polls_pending() > 0 {
        assert!(Instant::now() < deadline, "the startup pass never ended");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    register_axis(axis).unwrap();
    let (name, factory) = motor_rs::motor_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(
            r#"record(motor, "BM:m") { field(DTYP, "asynMotor") field(OUT, "@asyn(BOOTMV,0)") field(MRES, "1") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    // Time for a requested poll's post to process the record.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(db.get_pv("BM:m.DRBV").unwrap(), EpicsValue::Double(7.0));
    assert_eq!(db.get_pv("BM:m.DMOV").unwrap(), EpicsValue::Short(1));
    assert_eq!(db.get_pv("BM:m.MOVN").unwrap(), EpicsValue::Short(0));
    poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}
