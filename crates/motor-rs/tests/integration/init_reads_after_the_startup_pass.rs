//! C devMotorAsyn init_record reads the axis status through
//! `readGenericPointer` (devMotorAsyn.c:404) under the asynPortDriver lock
//! (asynPortDriver.cpp:3290-3292), which the poller holds over a whole
//! pass (asynMotorController.cpp:644-694). A record initialising during the
//! startup pass waits for it and reads what it posted; reading before the
//! post, then taking the post before `dbScanLockOK`, leaves the record
//! with a status no later pass repeats (devMotorAsyn.c:763-766,
//! asynMotorAxis.cpp:318-320).

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure};
use asyn_rs::registry::open_owner;
use asyn_rs::user::AsynUser;
use epics_base_rs::server::ioc_app::init_hooks::{InitHookState, init_hook_register};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::{ControllerAxis, ControllerBuilder};
use motor_rs::devices::{device_support_factory, devices, register_axis, register_controller_port};
use motor_rs::flags::MstaFlags;
use motor_rs::poll_loop::PollCommand;
use motor_rs::trace::TraceUser;

/// An axis at step 5, done. Its first poll says so on `entered`, then
/// waits for `release` if it has one.
struct Parked {
    entered: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<Option<mpsc::Receiver<()>>>,
}

fn at_5() -> MotorStatus {
    MotorStatus {
        done: true,
        ..MotorStatus::default().with_position(5.0)
    }
}

impl AsynMotor for Parked {
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
        if let Some(entered) = self.entered.lock().unwrap().take() {
            let _ = entered.send(());
        }
        if let Some(release) = self.release.lock().unwrap().take() {
            let _ = release.recv_timeout(Duration::from_secs(5));
        }
        *status = at_5();
        Ok(())
    }
}

/// The startup pass parks in axis 0's poll while iocInit runs the records'
/// init, and `finishDevSup` comes after axis 0's post and before axis 1's,
/// as the mcb4b-ioc boot that lost acs:m0's status ordered them.
#[tokio::test(flavor = "multi_thread")]
async fn a_record_initialised_during_the_startup_pass_reads_its_post() {
    register_controller_port(&open_owner(), "INITLK", 2, ()).unwrap();
    let (entered0_tx, entered0) = mpsc::channel();
    let (release0, release0_rx) = mpsc::channel();
    let (entered1_tx, entered1) = mpsc::channel();
    let (init_done_tx, init_done) = mpsc::channel::<()>();
    let mut controller = ControllerBuilder::new();
    let ControllerAxis { axis, poll_cmd_tx } = controller.axis(
        Arc::new(Mutex::new(Parked {
            entered: Mutex::new(Some(entered0_tx)),
            release: Mutex::new(Some(release0_rx)),
        })),
        TraceUser::Asyn {
            port: "INITLK".into(),
            addr: 0,
        },
    );
    register_axis(axis).unwrap();
    let ControllerAxis { axis, .. } = controller.axis(
        Arc::new(Mutex::new(Parked {
            entered: Mutex::new(Some(entered1_tx)),
            release: Mutex::new(None),
        })),
        TraceUser::Asyn {
            port: "INITLK".into(),
            addr: 1,
        },
    );
    register_axis(axis).unwrap();
    let hour = Duration::from_secs(3600);
    let poller = controller.build(hour, hour, 0).start();
    tokio::task::spawn_blocking(move || entered0.recv_timeout(Duration::from_secs(5)))
        .await
        .unwrap()
        .expect("the startup pass polls axis 0");
    // Axis 0's poll ends once the records' init has run, or when it could
    // not run while the pass held the lock.
    std::thread::spawn(move || {
        let _ = init_done.recv_timeout(Duration::from_millis(300));
        let _ = release0.send(());
    });
    // Between the records' init and finishDevSup: wait for axis 0's post,
    // which axis 1's poll follows.
    let entered1 = Mutex::new(entered1);
    let init_done_tx = Mutex::new(init_done_tx);
    init_hook_register(Arc::new(move |state| {
        if state == InitHookState::AfterInitDatabase {
            let _ = init_done_tx.lock().unwrap().send(());
            let _ = entered1
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5));
        }
    }));
    let (name, factory) = motor_rs::motor_record_factory();
    let ioc = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(
            r#"record(motor, "IL:m0") { field(DTYP, "asynMotor") field(OUT, "@asyn(INITLK,0)") field(MRES, "1") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let msta = MstaFlags::from_status(&at_5()).bits();
    assert_eq!(
        (
            db.get_pv("IL:m0.MSTA").unwrap(),
            db.get_pv("IL:m0.DRBV").unwrap()
        ),
        (EpicsValue::ULong(msta), EpicsValue::Double(5.0)),
    );
    poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}
