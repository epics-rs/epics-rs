//! C's MOTOR_MOVE_HOME: the write signals the controller's `motorMoveToHome`
//! thread, which runs the axis's `doMoveToHome` without the controller lock
//! (asynMotorController.cpp:236-243, 723-742).

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{
    AsynMotor, MotorStatus, MoveToHomeTask, PollFailure, PortSets, StatusWrite,
};
use asyn_rs::manager::PortManager;
use asyn_rs::param::{ParamType, ParamValue};
use asyn_rs::port::PortDriverBase;
use asyn_rs::registry::open_owner;
use asyn_rs::trace::TraceFile;
use asyn_rs::user::AsynUser;
use motor_rs::builder::{ControllerAxis, ControllerBuilder};
use motor_rs::controller_port::{BASE_PARAMS, ControllerParams, index};
use motor_rs::devices::{register_axis, register_controller_port};
use motor_rs::trace::TraceUser;

#[derive(Default)]
struct Own;

impl ControllerParams for Own {
    fn create_params(&mut self, base: &mut PortDriverBase) -> AsynResult<()> {
        base.create_param("OWN", ParamType::Int32)?;
        Ok(())
    }
}

fn own() -> usize {
    BASE_PARAMS.len()
}

/// What the task saw when it ran: its thread's name, whether the axis was
/// free to lock, and its referencing distance.
type Seen = Arc<Mutex<Option<(String, bool, i32)>>>;

type Shared = Arc<Mutex<dyn AsynMotor>>;

/// An axis whose move to home posts OWN=9 and clears homed under the
/// controller lock, as XPS's `doMoveToHome` does (XPSAxis.cpp:1676-1680).
struct Homing {
    me: Arc<OnceLock<Shared>>,
    seen: Seen,
}

impl AsynMotor for Homing {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
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
    fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
        Ok(MotorStatus::default())
    }
    fn move_to_home_task(&mut self, distance: i32) -> Option<MoveToHomeTask> {
        let me = self.me.get().unwrap().clone();
        let seen = self.seen.clone();
        Some(Box::new(move |under_lock: &dyn Fn(PortSets)| {
            let name = std::thread::current().name().unwrap_or("").to_owned();
            let free = me.try_lock().is_ok();
            let mut sets = PortSets::default();
            sets.set(0, own(), ParamValue::Int32(9));
            sets.call_param_callbacks(0);
            sets.status(1, StatusWrite::Homed(false));
            under_lock(sets);
            *seen.lock().unwrap() = Some((name, free, distance));
            Ok(())
        }))
    }
}

/// Axis 1 of `port` on a controller, with or without the moveToHome thread.
fn homing_axis(port: &str, thread: bool) -> Seen {
    let seen = Seen::default();
    let me = Arc::new(OnceLock::new());
    let motor: Shared = Arc::new(Mutex::new(Homing {
        me: me.clone(),
        seen: seen.clone(),
    }));
    let _ = me.set(motor.clone());
    let mut controller = ControllerBuilder::new();
    let ControllerAxis { axis, .. } = controller.axis(
        motor,
        TraceUser::Asyn {
            port: port.into(),
            addr: 1,
        },
    );
    register_axis(axis).unwrap();
    if thread {
        controller.axes().start_move_to_home_thread().unwrap();
    }
    seen
}

fn eventually(mut f: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn a_move_home_write_runs_the_axis_s_task_on_its_thread_off_the_lock() {
    let port = register_controller_port(&open_owner(), "MTHRUN", 2, Own).unwrap();
    let seen = homing_axis("MTHRUN", true);
    port.write_int32_blocking(index::MOVE_TO_HOME, 1, 1)
        .unwrap();
    assert!(eventually(|| seen.lock().unwrap().is_some()));
    assert_eq!(
        seen.lock().unwrap().clone(),
        Some(("motorMoveToHome".to_owned(), true, 0))
    );
    assert_eq!(port.read_int32_blocking(own(), 0).unwrap(), 9);
}

#[test]
fn without_the_thread_a_move_home_write_runs_nothing() {
    let port = register_controller_port(&open_owner(), "MTHNONE", 2, Own).unwrap();
    let seen = homing_axis("MTHNONE", false);
    port.write_int32_blocking(index::MOVE_TO_HOME, 1, 1)
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(seen.lock().unwrap().is_none());
    assert!(port.read_int32_blocking(own(), 0).is_err());
}

/// An axis with C's default `doMoveToHome`.
struct Plain;

impl AsynMotor for Plain {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
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
    fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
        Ok(MotorStatus::default())
    }
}

#[test]
fn an_axis_without_a_move_to_home_prints_c_s_no_implementation_line() {
    let port = register_controller_port(&open_owner(), "MTHPLAIN", 2, Own).unwrap();
    let path = std::env::temp_dir().join(format!("motor-mth-{}", std::process::id()));
    let file = std::fs::File::create(&path).unwrap();
    PortManager::global().trace_manager().set_trace_file(
        Some("MTHPLAIN"),
        TraceFile::File(Arc::new(Mutex::new(file))),
    );
    let mut controller = ControllerBuilder::new();
    let ControllerAxis { axis, .. } = controller.axis(
        Arc::new(Mutex::new(Plain)),
        TraceUser::Asyn {
            port: "MTHPLAIN".into(),
            addr: 1,
        },
    );
    register_axis(axis).unwrap();
    controller.axes().start_move_to_home_thread().unwrap();
    port.write_int32_blocking(index::MOVE_TO_HOME, 1, 1)
        .unwrap();
    let line = "asynMotorAxis:doMoveToHome: Axis=1 no implementation";
    let printed = eventually(|| std::fs::read_to_string(&path).unwrap().contains(line));
    let _ = std::fs::remove_file(&path);
    assert!(printed);
}

#[test]
fn an_enabled_distance_reaches_the_axis_s_task() {
    use epics_base_rs::server::iocsh::registry::{ArgValue, CommandContext};
    let port = register_controller_port(&open_owner(), "MTHDIST", 2, Own).unwrap();
    let seen = homing_axis("MTHDIST", true);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _rt = rt.enter();
    let ctx = CommandContext::new(
        Arc::new(epics_base_rs::server::database::PvDatabase::new()),
        epics_base_rs::runtime::task::BlockingBridge::capture(),
    );
    let enable = motor_rs::ioc::enable_move_to_home_command();
    let args = [
        ArgValue::String("MTHDIST".into()),
        ArgValue::Int(1),
        ArgValue::Int(250),
    ];
    enable.handler.call(&args, &ctx).unwrap();
    port.write_int32_blocking(index::MOVE_TO_HOME, 1, 1)
        .unwrap();
    assert!(eventually(|| seen.lock().unwrap().is_some()));
    assert_eq!(seen.lock().unwrap().as_ref().unwrap().2, 250);
}
