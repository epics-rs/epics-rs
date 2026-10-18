//! Each poll of an asynMotorController axis sets its status values in the
//! controller port's parameters at the axis address, as C's driver poll does
//! through `asynMotorAxis::setDoubleParam` / `setIntegerParam` and
//! `callParamCallbacks`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure};
use asyn_rs::port_handle::PortHandle;
use asyn_rs::registry::open_owner;
use asyn_rs::user::AsynUser;
use motor_rs::builder::MotorBuilder;
use motor_rs::controller_port::index;
use motor_rs::device_state::AxisSubscribers;
use motor_rs::device_support::MotorAxis;
use motor_rs::devices::{register_axis, register_controller_port};
use motor_rs::trace::TraceUser;
use tokio::sync::mpsc;

struct Axis(Arc<Mutex<MotorStatus>>);

impl AsynMotor for Axis {
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
        Ok(self.0.lock().unwrap().clone())
    }
}

/// The MOTOR_STATUS_* bits of axis `addr`, DIRECTION first.
fn bits(port: &PortHandle, addr: i32) -> Vec<i32> {
    (0..15)
        .map(|bit| {
            port.read_int32_blocking(index::STATUS_DIRECTION + bit, addr)
                .unwrap()
        })
        .collect()
}

#[test]
fn a_poll_sets_the_axis_status_in_the_controller_ports_params() {
    let port = register_controller_port(&open_owner(), "pollParams", 2, ()).unwrap();
    let status = Arc::new(Mutex::new(MotorStatus {
        position: 3.5,
        encoder_position: 7.0,
        velocity: 1.25,
        done: true,
        high_limit: true,
        powered: true,
        homed: true,
        ..MotorStatus::default()
    }));
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(Axis(status.clone())));
    // The startup poll lands in the port once the axis mirrors it.
    let build = || {
        MotorBuilder::new(motor.clone())
            .trace_user(TraceUser::Asyn {
                port: "pollParams".into(),
                addr: 1,
            })
            .build_axis()
    };
    let _setup = build();
    assert_eq!(port.read_float64_blocking(index::POSITION, 1).unwrap(), 3.5);
    assert_eq!(
        port.read_float64_blocking(index::ENCODER_POSITION, 1)
            .unwrap(),
        7.0
    );
    assert_eq!(
        port.read_float64_blocking(index::ACT_VELOCITY, 1).unwrap(),
        1.25
    );
    // DONE, HIGH_LIMIT, POWERED, HOMED: bits 1, 2, 5, 14.
    let word = (1 << 1) | (1 << 2) | (1 << 5) | (1 << 14);
    assert_eq!(port.read_int32_blocking(index::STATUS, 1).unwrap(), word);
    let want: Vec<i32> = (0..15).map(|bit| (word >> bit) & 1).collect();
    assert_eq!(bits(&port, 1), want);
    // Axis 0 is another axis: none of it is set.
    assert!(port.read_float64_blocking(index::POSITION, 0).is_err());

    *status.lock().unwrap() = MotorStatus {
        position: -2.0,
        moving: true,
        msta_moving: true,
        done: false,
        low_limit: true,
        ..MotorStatus::default()
    };
    let _setup = build();
    assert_eq!(
        port.read_float64_blocking(index::POSITION, 1).unwrap(),
        -2.0
    );
    let word = (1 << 10) | (1 << 13);
    assert_eq!(port.read_int32_blocking(index::STATUS, 1).unwrap(), word);
    let want: Vec<i32> = (0..15).map(|bit| (word >> bit) & 1).collect();
    assert_eq!(bits(&port, 1), want);
}

#[test]
fn a_new_axis_sets_what_the_asyn_motor_axis_constructor_does() {
    let port = register_controller_port(&open_owner(), "axisCtorParams", 2, ()).unwrap();
    let (motor_tx, _) = mpsc::unbounded_channel();
    let (poll_tx, _) = mpsc::channel(1);
    register_axis(MotorAxis::new(
        TraceUser::Asyn {
            port: "axisCtorParams".into(),
            addr: 1,
        },
        std::sync::Arc::new(std::sync::Mutex::new(motor_rs::sim_motor::SimMotor::new())),
        Duration::from_secs(1),
        poll_tx,
        motor_tx,
        AxisSubscribers::new(),
    ))
    .unwrap();
    assert_eq!(
        port.read_int32_blocking(index::POWER_AUTO_ONOFF, 1)
            .unwrap(),
        0
    );
    assert_eq!(port.read_int32_blocking(index::STATUS, 1).unwrap(), 0);
    assert_eq!(
        port.read_float64_blocking(index::POWER_OFF_DELAY, 1)
            .unwrap(),
        0.0
    );
    assert_eq!(
        port.read_float64_blocking(index::POWER_ON_DELAY, 1)
            .unwrap(),
        0.0
    );
    // No axis 0 was made.
    assert!(port.read_int32_blocking(index::STATUS, 0).is_err());
}

/// Counts the driver's polls.
struct Counted(Arc<Mutex<u32>>);

impl AsynMotor for Counted {
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
        *self.0.lock().unwrap() += 1;
        Ok(MotorStatus::default())
    }
}

#[test]
fn a_records_get_info_polls_and_posts_an_unchanged_status() {
    use motor_rs::device_state::new_shared_state;
    use motor_rs::flags::MotorCommand;
    use motor_rs::poll_loop::{CommandBatch, MotorPollLoop, PollerInbox};
    use motor_rs::trace::MotorTrace;

    let polls = Arc::new(Mutex::new(0));
    let (_poll_tx, poll) = mpsc::channel(4);
    let (tx, commands) = mpsc::unbounded_channel();
    let subscribers = AxisSubscribers::new();
    let trace = Arc::new(MotorTrace::with_user(TraceUser::Model2 {
        port: "getInfoPoll".into(),
        addr: 0,
    }));
    let state = new_shared_state();
    let (io_tx, _io_rx) = mpsc::channel(4);
    let record = subscribers.attach(state.clone(), io_tx, trace.clone());
    let mut poll_loop = MotorPollLoop::new(
        PollerInbox { poll, commands },
        Arc::new(Mutex::new(Counted(polls.clone()))),
        subscribers,
        Duration::from_secs(1),
        Duration::from_secs(1),
        0,
    );
    let seq = |state: &motor_rs::device_state::SharedDeviceState| {
        state.lock().unwrap().latest_status.as_ref().map(|s| s.seq)
    };
    let primed = seq(&state);

    tx.send(motor_rs::poll_loop::ToAxis::from(CommandBatch {
        trace,
        record: Some(record),
        commands: vec![MotorCommand::Poll],
    }))
    .unwrap();
    poll_loop.run_queued_commands();
    // C motorUpdateStatus_: poll, then force the callback
    // (asynMotorController.cpp:217-222).
    assert_eq!(*polls.lock().unwrap(), 2);
    assert_ne!(seq(&state), primed, "an unchanged status still posts");
}
