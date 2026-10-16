//! The controller asyn port: C `asynMotorController::writeInt32` /
//! `writeFloat64` and a subclass's own parameters at the controller and
//! axis addresses.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriverBase;
use asyn_rs::registry::open_owner;
use asyn_rs::user::AsynUser;
use motor_rs::controller_port::{BASE_PARAMS, ControllerParams, index};
use motor_rs::device_state::AxisSubscribers;
use motor_rs::device_support::MotorAxis;
use motor_rs::devices::{register_axis, register_controller_port};
use motor_rs::flags::MotorCommand;
use motor_rs::poll_loop::{CommandBatch, PollCommand};
use motor_rs::trace::TraceUser;
use tokio::sync::mpsc;

/// pmac's shape: a controller axis 0 with a parameter of its own.
#[derive(Default)]
struct Pmacish {
    seen: Arc<Mutex<Vec<(i32, i32)>>>,
    feed_rate: usize,
}

impl ControllerParams for Pmacish {
    fn create_params(&mut self, base: &mut PortDriverBase) -> AsynResult<()> {
        self.feed_rate = base.create_param("FEEDRATE", ParamType::Int32)?;
        Ok(())
    }

    fn is_controller_axis(&self, addr: i32) -> bool {
        addr == 0
    }

    fn write_int32(
        &mut self,
        _: &mut PortDriverBase,
        user: &AsynUser,
        value: i32,
    ) -> AsynResult<()> {
        self.seen.lock().unwrap().push((user.reason as i32, value));
        Ok(())
    }
}

struct Axis {
    commands: mpsc::UnboundedReceiver<CommandBatch>,
    polls: mpsc::Receiver<PollCommand>,
}

fn axis(port: &str, addr: i32) -> Axis {
    let (motor_tx, commands) = mpsc::unbounded_channel();
    let (poll_tx, polls) = mpsc::channel(1);
    register_axis(MotorAxis::new(
        TraceUser::Asyn {
            port: port.into(),
            addr,
        },
        Duration::from_secs(1),
        poll_tx,
        motor_tx,
        AxisSubscribers::new(),
    ))
    .unwrap();
    Axis { commands, polls }
}

fn next(axis: &mut Axis) -> MotorCommand {
    let batch = axis.commands.try_recv().expect("a command");
    assert_eq!(batch.record, None);
    let [command] = <[_; 1]>::try_from(batch.commands).unwrap();
    command
}

#[test]
fn a_driver_param_at_the_controller_axis_reaches_the_driver_and_reads_back() {
    let params = Pmacish::default();
    let seen = params.seen.clone();
    let port = register_controller_port(&open_owner(), "CPCTRL", 3, params).unwrap();
    // The first parameter after the base's, as in C.
    let feed_rate = BASE_PARAMS.len();
    port.write_int32_blocking(feed_rate, 0, 75).unwrap();
    assert_eq!(port.read_int32_blocking(feed_rate, 0).unwrap(), 75);
    // MOTOR_DEFER_MOVES at the controller axis: stored, no driver call.
    port.write_int32_blocking(index::DEFER_MOVES, 0, 2).unwrap();
    assert_eq!(port.read_int32_blocking(index::DEFER_MOVES, 0).unwrap(), 2);
    assert_eq!(
        *seen.lock().unwrap(),
        [(feed_rate as i32, 75), (index::DEFER_MOVES as i32, 2)]
    );
}

#[test]
fn a_write_where_the_controller_has_no_axis_fails() {
    let port = register_controller_port(&open_owner(), "CPNOAXIS", 2, ()).unwrap();
    assert!(port.write_int32_blocking(index::STOP, 1, 1).is_err());
}

#[test]
fn base_int32_functions_reach_the_axis() {
    let port = register_controller_port(&open_owner(), "CPINT", 2, ()).unwrap();
    let mut ax = axis("CPINT", 1);
    port.write_int32_blocking(index::DEFER_MOVES, 1, 1).unwrap();
    assert_eq!(next(&mut ax), MotorCommand::DeferMoves { defer: true });
    port.write_float64_blocking(index::ACCEL, 1, 4.0).unwrap();
    port.write_int32_blocking(index::STOP, 1, 1).unwrap();
    assert_eq!(next(&mut ax), MotorCommand::Stop { acceleration: 4.0 });
    port.write_int32_blocking(index::CLOSED_LOOP, 1, 1).unwrap();
    assert_eq!(next(&mut ax), MotorCommand::SetClosedLoop { enable: true });
    port.write_int32_blocking(index::UPDATE_STATUS, 1, 1)
        .unwrap();
    assert!(matches!(ax.polls.try_recv(), Ok(PollCommand::ForcePoll)));
    assert!(ax.commands.try_recv().is_err());
}

#[test]
fn a_move_takes_its_speeds_from_the_params() {
    let port = register_controller_port(&open_owner(), "CPMOVE", 1, ()).unwrap();
    let mut ax = axis("CPMOVE", 0);
    port.write_float64_blocking(index::VEL_BASE, 0, 1.0)
        .unwrap();
    port.write_float64_blocking(index::VELOCITY, 0, 2.0)
        .unwrap();
    port.write_float64_blocking(index::ACCEL, 0, 3.0).unwrap();
    port.write_float64_blocking(index::MOVE_ABS, 0, 10.0)
        .unwrap();
    assert_eq!(
        next(&mut ax),
        MotorCommand::MoveAbsolute {
            position: 10.0,
            min_velocity: 1.0,
            velocity: 2.0,
            acceleration: 3.0,
        }
    );
    port.write_float64_blocking(index::HOME, 0, 1.0).unwrap();
    assert_eq!(
        next(&mut ax),
        MotorCommand::Home {
            forward: true,
            min_velocity: 1.0,
            velocity: 2.0,
            acceleration: 3.0,
        }
    );
    assert_eq!(
        port.read_float64_blocking(index::MOVE_ABS, 0).unwrap(),
        10.0
    );
}

#[test]
fn a_second_port_of_the_same_name_is_refused() {
    register_controller_port(&open_owner(), "CPTWIN", 1, ()).unwrap();
    assert!(register_controller_port(&open_owner(), "CPTWIN", 1, ()).is_err());
}
