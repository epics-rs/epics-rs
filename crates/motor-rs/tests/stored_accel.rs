//! The record's SET_VELOCITY, SET_VEL_BASE and SET_ACCEL store the speeds
//! an asyn axis moves and stops on, as asynMotorController and drvMotorAsyn
//! read them; a move with no SET_ACCEL ahead of it (C skips it at
//! `acc <= 0`) runs on the last stored one. On an asynMotorController axis
//! they are the controller port's MOTOR_VELOCITY / MOTOR_VEL_BASE /
//! MOTOR_ACCEL, which the speed writes set through the port. On a model-1
//! card they are the card's own registers, which a SET_* string sets.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure};
use asyn_rs::port::PortDriverBase;
use asyn_rs::port_handle::PortHandle;
use asyn_rs::registry::open_owner;
use asyn_rs::user::AsynUser;
use motor_rs::controller_port::{ControllerParams, ParamWrite, index};
use motor_rs::device_state::AxisSubscribers;
use motor_rs::device_support::MotorAxis;
use motor_rs::devices::{register_axis, register_controller_port};
use motor_rs::flags::MotorCommand;
use motor_rs::poll_loop::{CommandBatch, MotorPollLoop, PollerInbox};
use motor_rs::trace::{MotorTrace, TraceUser};
use tokio::sync::mpsc;

/// The acceleration of every move, home and stop the driver ran.
struct Axis(Arc<Mutex<Vec<f64>>>, Arc<Mutex<Vec<(f64, f64)>>>);

impl AsynMotor for Axis {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, b: f64, v: f64, a: f64) -> AsynResult<()> {
        self.0.lock().unwrap().push(a);
        self.1.lock().unwrap().push((b, v));
        Ok(())
    }
    fn move_relative(&mut self, _: &AsynUser, _: f64, b: f64, v: f64, a: f64) -> AsynResult<()> {
        self.0.lock().unwrap().push(a);
        self.1.lock().unwrap().push((b, v));
        Ok(())
    }
    fn home(&mut self, _: &AsynUser, b: f64, v: f64, a: f64, _: bool) -> AsynResult<()> {
        self.0.lock().unwrap().push(a);
        self.1.lock().unwrap().push((b, v));
        Ok(())
    }
    fn stop(&mut self, _: &AsynUser, a: f64) -> AsynResult<()> {
        self.0.lock().unwrap().push(a);
        Ok(())
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn poll_failure(&self) -> PollFailure {
        PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
        Ok(MotorStatus {
            done: true,
            ..MotorStatus::default()
        })
    }
}

/// The accelerations the driver got for a record's `commands` on a `user`
/// axis.
fn run(user: TraceUser, commands: Vec<MotorCommand>) -> Vec<f64> {
    run_from(Some(0), user, commands).0
}

/// [`run`] for `record`, `None` being the controller port's own writes,
/// with each move's `(base velocity, velocity)`.
fn run_from(
    record: Option<usize>,
    user: TraceUser,
    commands: Vec<MotorCommand>,
) -> (Vec<f64>, Vec<(f64, f64)>) {
    let (_poll_tx, poll) = mpsc::channel(4);
    let (tx, commands_rx) = mpsc::unbounded_channel();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let speeds = Arc::new(Mutex::new(Vec::new()));
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(Axis(seen.clone(), speeds.clone())));
    let mut poll_loop = MotorPollLoop::new(
        PollerInbox {
            poll,
            commands: commands_rx,
        },
        motor,
        AxisSubscribers::new(),
        Duration::from_secs(1),
        Duration::from_secs(1),
        0,
    );
    tx.send(CommandBatch {
        trace: Arc::new(MotorTrace::with_user(user)),
        record,
        commands,
    })
    .unwrap();
    poll_loop.run_queued_commands();
    let seen = seen.lock().unwrap().clone();
    (seen, speeds.lock().unwrap().clone())
}

fn set_accel(acceleration: f64) -> MotorCommand {
    MotorCommand::SetAccel { acceleration }
}

fn move_with(acceleration: f64) -> MotorCommand {
    MotorCommand::MoveAbsolute {
        position: 5.0,
        min_velocity: 0.0,
        velocity: 1.0,
        acceleration,
    }
}

fn home_with(acceleration: f64) -> MotorCommand {
    MotorCommand::Home {
        forward: true,
        min_velocity: 0.0,
        velocity: 0.0,
        acceleration,
    }
}

/// Axis 0 of a new controller port `port` with `params`.
fn controller_with(port: &str, params: impl ControllerParams) -> (TraceUser, PortHandle) {
    let handle = register_controller_port(&open_owner(), port, 1, params).unwrap();
    let user = TraceUser::Asyn {
        port: port.into(),
        addr: 0,
    };
    let (motor_tx, _) = mpsc::unbounded_channel();
    let (poll_tx, _) = mpsc::channel(1);
    register_axis(MotorAxis::new(
        user.clone(),
        Duration::from_secs(1),
        poll_tx,
        motor_tx,
        AxisSubscribers::new(),
    ))
    .unwrap();
    (user, handle)
}

fn controller(port: &str) -> (TraceUser, PortHandle) {
    controller_with(port, ())
}

fn model2() -> TraceUser {
    TraceUser::Model2 {
        port: "storedAccel2".into(),
        addr: 0,
    }
}

fn model1() -> TraceUser {
    TraceUser::Model1 {
        device: "devStoredAccel",
        card: 0,
        signal: 0,
    }
}

#[test]
fn a_skipped_acceleration_runs_on_the_last_stored_one() {
    for user in [controller("storedAccel").0, model2(), model1()] {
        let seen = run(
            user,
            vec![
                set_accel(2.0),
                move_with(2.0),
                home_with(0.0),
                set_accel(3.0),
                move_with(3.0),
                home_with(-1.0),
            ],
        );
        assert_eq!(seen, [2.0, 2.0, 3.0, 3.0]);
    }
}

#[test]
fn a_skipped_acceleration_before_any_is_stored_is_zero() {
    for user in [controller("storedAccelNone").0, model2(), model1()] {
        let seen = run(user, vec![home_with(-1.0), MotorCommand::stop_axis()]);
        assert_eq!(seen, [0.0, 0.0]);
    }
}

#[test]
fn the_records_acceleration_is_the_controller_ports_motor_accel() {
    let (user, port) = controller("storedAccelParam");
    assert_eq!(run(user, vec![set_accel(2.0), move_with(2.0)]), [2.0]);
    assert_eq!(port.read_float64_blocking(index::ACCEL, 0).unwrap(), 2.0);
}

#[test]
fn a_skipped_acceleration_runs_on_one_the_port_wrote() {
    let (user, port) = controller("storedAccelPort");
    port.write_float64_blocking(index::ACCEL, 0, 7.0).unwrap();
    assert_eq!(run(user, vec![home_with(0.0)]), [7.0]);
}

/// A subclass refusing an acceleration above 5.
struct SlowOnly;

impl ControllerParams for SlowOnly {
    fn write_float64(&mut self, _: &mut PortDriverBase, user: &AsynUser, value: f64) -> ParamWrite {
        if user.reason == index::ACCEL && value > 5.0 {
            return ParamWrite::Handled(Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "too fast".into(),
            }));
        }
        ParamWrite::PassToBase(Ok(()))
    }
}

#[test]
fn a_refused_set_accel_leaves_the_move_on_the_stored_one() {
    let (user, port) = controller_with("storedAccelRefused", SlowOnly);
    port.write_float64_blocking(index::ACCEL, 0, 3.0).unwrap();
    assert_eq!(run(user, vec![set_accel(9.0), move_with(9.0)]), [3.0]);
    assert_eq!(port.read_float64_blocking(index::ACCEL, 0).unwrap(), 3.0);
}

#[test]
fn a_record_stop_runs_on_the_last_stored_acceleration() {
    for user in [controller("storedAccelStop").0, model2(), model1()] {
        let seen = run(
            user,
            vec![
                set_accel(2.0),
                move_with(2.0),
                MotorCommand::stop_axis(),
                home_with(0.0),
            ],
        );
        assert_eq!(seen, [2.0, 2.0, 2.0]);
    }
}

/// Counts the MOTOR_ACCEL writes reaching the port.
struct AccelWrites(Arc<Mutex<u32>>);

impl ControllerParams for AccelWrites {
    fn write_float64(&mut self, _: &mut PortDriverBase, user: &AsynUser, _: f64) -> ParamWrite {
        if user.reason == index::ACCEL {
            *self.0.lock().unwrap() += 1;
        }
        ParamWrite::PassToBase(Ok(()))
    }
}

#[test]
fn the_ports_own_move_writes_no_accel() {
    let writes = Arc::new(Mutex::new(0));
    let (user, _port) = controller_with("storedAccelPortMove", AccelWrites(writes.clone()));
    assert_eq!(run_from(None, user, vec![move_with(2.0)]).0, [2.0]);
    assert_eq!(*writes.lock().unwrap(), 0);
}

/// A move at 4 steps/s from a base of 1, then C's backlash final leg:
/// SET_VELOCITY 6 and no SET_VEL_BASE (motorRecord.cc:955-968).
fn move_then_backlash_leg() -> Vec<MotorCommand> {
    let leg = |velocity| MotorCommand::MoveRelative {
        distance: 1.0,
        min_velocity: 9.0,
        velocity,
        acceleration: 0.0,
    };
    vec![
        MotorCommand::SetVelocity { velocity: 4.0 },
        MotorCommand::SetVelBase { velocity: 1.0 },
        leg(4.0),
        MotorCommand::SetVelocity { velocity: 6.0 },
        leg(6.0),
    ]
}

#[test]
fn a_move_runs_on_the_stored_velocities() {
    for user in [controller("storedSpeeds").0, model2(), model1()] {
        let (_, speeds) = run_from(Some(0), user, move_then_backlash_leg());
        assert_eq!(speeds, [(1.0, 4.0), (1.0, 6.0)]);
    }
}

#[test]
fn the_records_velocities_are_the_controller_ports_params() {
    let (user, port) = controller("storedSpeedsParam");
    run_from(Some(0), user, move_then_backlash_leg());
    assert_eq!(port.read_float64_blocking(index::VELOCITY, 0).unwrap(), 6.0);
    assert_eq!(port.read_float64_blocking(index::VEL_BASE, 0).unwrap(), 1.0);
}

#[test]
fn a_record_write_sets_the_controller_ports_param() {
    let (user, port) = controller("recordWriteParams");
    run(
        user,
        vec![
            MotorCommand::SetHighLimit { position: 7.0 },
            move_with(2.0),
            MotorCommand::SetClosedLoop { enable: true },
        ],
    );
    assert_eq!(
        port.read_float64_blocking(index::HIGH_LIMIT, 0).unwrap(),
        7.0
    );
    assert_eq!(port.read_float64_blocking(index::MOVE_ABS, 0).unwrap(), 5.0);
    assert_eq!(port.read_int32_blocking(index::CLOSED_LOOP, 0).unwrap(), 1);
}
