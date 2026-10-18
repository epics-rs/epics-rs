//! C setMovingPollPeriod / setIdlePollPeriod: the iocsh commands change a
//! running poller's period and wake it (asynMotorController.cpp:918-979).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure};
use asyn_rs::registry::open_owner;
use asyn_rs::user::AsynUser;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandContext, CommandDef};
use motor_rs::builder::{ControllerAxis, ControllerBuilder};
use motor_rs::devices::{register_axis, register_controller_port};
use motor_rs::trace::TraceUser;

/// An idle axis that counts its polls.
struct Counting(Arc<AtomicUsize>);

impl AsynMotor for Counting {
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
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(MotorStatus::default())
    }
}

fn context() -> CommandContext {
    CommandContext::new(
        Arc::new(epics_base_rs::server::database::PvDatabase::new()),
        epics_base_rs::runtime::task::BlockingBridge::capture(),
    )
}

/// Start a poller on `port` with hour-long periods and `forced` fast polls,
/// run `cmd` on it with a 10 ms period, and return the polls the next
/// 300 ms make.
fn polls_after(port: &str, forced: u32, cmd: CommandDef) -> usize {
    register_controller_port(&open_owner(), port, 1, ()).unwrap();
    let polls = Arc::new(AtomicUsize::new(0));
    let mut controller = ControllerBuilder::new().port(port);
    let ControllerAxis { axis, .. } = controller.axis(
        Arc::new(Mutex::new(Counting(polls.clone()))),
        TraceUser::Asyn {
            port: port.into(),
            addr: 0,
        },
    );
    register_axis(axis).unwrap();
    let hour = Duration::from_secs(3600);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _rt = rt.enter();
    rt.spawn(controller.build(hour, hour, forced).run());
    // The startup pass.
    std::thread::sleep(Duration::from_millis(100));
    let args = [ArgValue::String(port.into()), ArgValue::Double(0.01)];
    cmd.handler.call(&args, &context()).unwrap();
    let before = polls.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(300));
    polls.load(Ordering::Relaxed) - before
}

#[test]
fn set_idle_poll_period_paces_an_idle_poller() {
    let cmd = motor_rs::ioc::set_idle_poll_period_command();
    let polls = polls_after("POLLIDLE", 0, cmd);
    assert!(polls >= 10, "{polls} polls");
}

/// The wakeup reloads the forced fast polls, which run at the new moving
/// period.
#[test]
fn set_moving_poll_period_paces_the_forced_fast_polls() {
    let cmd = motor_rs::ioc::set_moving_poll_period_command();
    let polls = polls_after("POLLMOVING", 1_000_000, cmd);
    assert!(polls >= 10, "{polls} polls");
}

/// C finds the controller by its port (findAsynPortDriver), so a
/// controller whose axes are created later (phytronCreateAxis after
/// phytronCreateController's startPoller) takes the period already.
#[test]
fn set_idle_poll_period_reaches_a_poller_with_no_axes() {
    let port = "POLLNOAXES";
    register_controller_port(&open_owner(), port, 1, ()).unwrap();
    let passes = Arc::new(AtomicUsize::new(0));
    let counted = passes.clone();
    let controller = ControllerBuilder::new()
        .controller_poll(move || {
            counted.fetch_add(1, Ordering::Relaxed);
            asyn_rs::interfaces::motor::PortSets::default()
        })
        .port(port);
    let hour = Duration::from_secs(3600);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _rt = rt.enter();
    rt.spawn(controller.build(hour, hour, 0).run());
    std::thread::sleep(Duration::from_millis(100));
    let args = [ArgValue::String(port.into()), ArgValue::Double(0.01)];
    let cmd = motor_rs::ioc::set_idle_poll_period_command();
    cmd.handler.call(&args, &context()).unwrap();
    let before = passes.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(300));
    let passes = passes.load(Ordering::Relaxed) - before;
    assert!(passes >= 10, "{passes} passes");
}

/// A controller of one axis (MotorBuilder::build_axis, motor-common's
/// install) is its port's poller too.
#[test]
fn set_idle_poll_period_reaches_a_one_axis_controller() {
    let port = "POLLONEAXIS";
    register_controller_port(&open_owner(), port, 1, ()).unwrap();
    let polls = Arc::new(AtomicUsize::new(0));
    let hour = Duration::from_secs(3600);
    let setup = motor_rs::builder::MotorBuilder::new(Arc::new(Mutex::new(Counting(polls.clone()))))
        .trace_user(TraceUser::Asyn {
            port: port.into(),
            addr: 0,
        })
        .moving_poll_interval(hour)
        .idle_poll_interval(hour)
        .forced_fast_polls(0)
        .build_axis();
    register_axis(setup.axis).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _rt = rt.enter();
    rt.spawn(setup.poll_loop.run());
    std::thread::sleep(Duration::from_millis(100));
    let args = [ArgValue::String(port.into()), ArgValue::Double(0.01)];
    let cmd = motor_rs::ioc::set_idle_poll_period_command();
    cmd.handler.call(&args, &context()).unwrap();
    let before = polls.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(300));
    let polls = polls.load(Ordering::Relaxed) - before;
    assert!(polls >= 10, "{polls} polls");
}
