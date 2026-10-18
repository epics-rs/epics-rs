//! The controller asyn port: C `asynMotorController::writeInt32` /
//! `writeFloat64` and a subclass's own parameters at the controller and
//! axis addresses.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::param::ParamType;
use asyn_rs::port::PortDriver;
use asyn_rs::port::PortDriverBase;
use asyn_rs::registry::open_owner;
use asyn_rs::user::AsynUser;
use motor_rs::controller_port::{
    AxisUser, BASE_PARAMS, ControllerCtx, ControllerParams, ControllerPort, ControllerReport,
    FinishedWrite, PROFILE_EXECUTE_DONE, PROFILE_TIME_MODE_FIXED, ParamDelta, ParamWrite,
    WriteHook, WriteOutcome, WriteValue, has_motor_axis, index,
};
use motor_rs::device_state::AxisSubscribers;
use motor_rs::device_support::{AxisReport, MotorAxis};
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
        user: AxisUser<'_>,
        value: i32,
    ) -> ParamWrite {
        self.seen.lock().unwrap().push((user.reason as i32, value));
        ParamWrite::PassToBase(Ok(()))
    }
}

/// The driver calls the port makes on an axis, as the commands they run.
#[derive(Default)]
struct Recorder(Arc<Mutex<VecDeque<MotorCommand>>>);

impl Recorder {
    fn push(&self, command: MotorCommand) -> AsynResult<()> {
        self.0.lock().unwrap().push_back(command);
        Ok(())
    }
}

impl asyn_rs::interfaces::motor::AsynMotor for Recorder {
    fn move_absolute(&mut self, _: &AsynUser, p: f64, b: f64, v: f64, a: f64) -> AsynResult<()> {
        self.push(MotorCommand::MoveAbsolute {
            position: p,
            min_velocity: b,
            velocity: v,
            acceleration: a,
        })
    }
    fn move_relative(&mut self, _: &AsynUser, d: f64, b: f64, v: f64, a: f64) -> AsynResult<()> {
        self.push(MotorCommand::MoveRelative {
            distance: d,
            min_velocity: b,
            velocity: v,
            acceleration: a,
        })
    }
    fn move_velocity(&mut self, _: &AsynUser, b: f64, v: f64, a: f64) -> AsynResult<()> {
        self.push(MotorCommand::MoveVelocity {
            min_velocity: b,
            velocity: v,
            acceleration: a,
        })
    }
    fn home(&mut self, _: &AsynUser, b: f64, v: f64, a: f64, forward: bool) -> AsynResult<()> {
        self.push(MotorCommand::Home {
            forward,
            min_velocity: b,
            velocity: v,
            acceleration: a,
        })
    }
    fn stop(&mut self, _: &AsynUser, acceleration: f64) -> AsynResult<()> {
        self.push(MotorCommand::Stop { acceleration })
    }
    fn set_position(&mut self, _: &AsynUser, position: f64) -> AsynResult<()> {
        self.push(MotorCommand::SetPosition { position })
    }
    fn set_encoder_position(&mut self, _: &AsynUser, position: f64) -> AsynResult<()> {
        self.push(MotorCommand::SetEncoderPosition { position })
    }
    fn set_closed_loop(&mut self, _: &AsynUser, enable: bool) -> AsynResult<()> {
        self.push(MotorCommand::SetClosedLoop { enable })
    }
    fn set_deferred_moves(&mut self, _: &AsynUser, defer: bool) -> AsynResult<()> {
        self.push(MotorCommand::DeferMoves { defer })
    }
    fn initialize_profile(&mut self, _: &AsynUser, max_points: usize) -> AsynResult<()> {
        self.push(MotorCommand::ProfileInitialize { max_points })
    }
    fn define_profile(&mut self, _: &AsynUser, positions: &[f64]) -> AsynResult<()> {
        self.push(MotorCommand::ProfileDefine {
            positions: positions.to_vec(),
        })
    }
    fn build_profile(&mut self, _: &AsynUser) -> AsynResult<()> {
        self.push(MotorCommand::ProfileBuild)
    }
    fn execute_profile(&mut self, _: &AsynUser) -> AsynResult<()> {
        self.push(MotorCommand::ProfileExecute)
    }
    fn abort_profile(&mut self, _: &AsynUser) -> AsynResult<()> {
        self.push(MotorCommand::ProfileAbort)
    }
    fn readback_profile(&mut self, _: &AsynUser) -> AsynResult<Vec<f64>> {
        self.push(MotorCommand::ProfileReadback)?;
        Ok(Vec::new())
    }
    fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
        asyn_rs::interfaces::motor::PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser) -> AsynResult<asyn_rs::interfaces::motor::MotorStatus> {
        self.push(MotorCommand::Poll)?;
        Ok(Default::default())
    }
}

struct Axis {
    calls: Arc<Mutex<VecDeque<MotorCommand>>>,
    polls: mpsc::Receiver<PollCommand>,
    // Held so the port's posts to the poller find it open.
    _commands: mpsc::UnboundedReceiver<motor_rs::poll_loop::ToAxis>,
}

fn axis(port: &str, addr: i32) -> Axis {
    let (motor_tx, commands) = mpsc::unbounded_channel();
    let (poll_tx, polls) = mpsc::channel(1);
    let recorder = Recorder::default();
    let calls = recorder.0.clone();
    register_axis(MotorAxis::new(
        TraceUser::Asyn {
            port: port.into(),
            addr,
        },
        Arc::new(Mutex::new(recorder)),
        Duration::from_secs(1),
        poll_tx,
        motor_tx,
        AxisSubscribers::new(),
    ))
    .unwrap();
    Axis {
        calls,
        polls,
        _commands: commands,
    }
}

fn next(axis: &mut Axis) -> MotorCommand {
    axis.calls
        .lock()
        .unwrap()
        .pop_front()
        .expect("a driver call")
}

fn no_call(axis: &Axis) -> bool {
    axis.calls.lock().unwrap().is_empty()
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

/// omsBaseController's shape: a string parameter its `writeOctet` sends
/// and does not store.
#[derive(Default)]
struct Sends {
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    send: usize,
}

impl ControllerParams for Sends {
    fn create_params(&mut self, base: &mut PortDriverBase) -> AsynResult<()> {
        self.send = base.create_param("SEND", ParamType::Octet)?;
        Ok(())
    }

    fn write_octet(
        &mut self,
        _: &mut PortDriverBase,
        user: AxisUser<'_>,
        data: &[u8],
    ) -> ParamWrite {
        if user.reason != self.send {
            return ParamWrite::PassToBase(Ok(()));
        }
        self.sent.lock().unwrap().push(data.to_vec());
        ParamWrite::Handled(Ok(()))
    }
}

#[test]
fn a_driver_write_octet_override_runs_in_place_of_the_base() {
    let params = Sends::default();
    let sent = params.sent.clone();
    let port = register_controller_port(&open_owner(), "CPOCTET", 1, params).unwrap();
    let io = asyn_rs::sync_io::SyncIOHandle::from_handle(port, 0, Duration::from_secs(1));
    let send = BASE_PARAMS.len();
    assert_eq!(io.write_octet(send, b"AX RP").unwrap(), 5);
    assert_eq!(*sent.lock().unwrap(), [b"AX RP".to_vec()]);
    // Never stored, so still undefined.
    assert!(io.read_octet(send, 16).is_err());
    // The base still stores what the override passes on.
    let message = index::PROFILE_BUILD_MESSAGE;
    io.write_octet(message, b"built").unwrap();
    assert_eq!(io.read_octet(message, 16).unwrap(), b"built\0");
}

/// phytronController's shape: a per-axis option it answers itself, the
/// rest the base's.
#[derive(Default)]
struct Options {
    methods: Arc<Mutex<HashMap<i32, String>>>,
}

impl ControllerParams for Options {
    fn read_option(
        &self,
        _: &PortDriverBase,
        user: AxisUser<'_>,
        key: &str,
    ) -> Option<AsynResult<String>> {
        (key == "pollMethod").then(|| {
            let methods = self.methods.lock().unwrap();
            Ok(methods.get(&user.addr).cloned().unwrap_or_default())
        })
    }

    fn write_option(
        &mut self,
        _: &mut PortDriverBase,
        user: AxisUser<'_>,
        key: &str,
        value: &str,
    ) -> Option<AsynResult<()>> {
        (key == "pollMethod").then(|| {
            self.methods
                .lock()
                .unwrap()
                .insert(user.addr, value.to_owned());
            Ok(())
        })
    }
}

#[test]
fn a_driver_option_override_runs_in_place_of_the_base() {
    let params = Options::default();
    let methods = params.methods.clone();
    let port = register_controller_port(&open_owner(), "CPOPTION", 2, params).unwrap();
    let user = |addr| AsynUser::default().with_addr(addr);
    port.set_option_blocking(user(1), "pollMethod", "serial")
        .unwrap();
    assert_eq!(methods.lock().unwrap()[&1], "serial");
    // The read resolves the axis from its user, as phytronController's
    // readOption does (phytronAxisMotor.cpp:596).
    assert_eq!(
        port.get_option_blocking(user(1), "pollMethod").unwrap(),
        "serial"
    );
    assert_eq!(port.get_option_blocking(user(0), "pollMethod").unwrap(), "");
    // Any other key is the base's, which keeps none (asynPortDriver.cpp:3402).
    assert!(port.set_option_blocking(user(1), "other", "x").is_err());
    assert!(port.get_option_blocking(user(1), "other").is_err());
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
    // C polls inside the write, ahead of its callbacks and closing line
    // (asynMotorController.cpp:217-221, :245-254).
    port.write_int32_blocking(index::UPDATE_STATUS, 1, 1)
        .unwrap();
    assert_eq!(next(&mut ax), MotorCommand::Poll);
    assert!(ax.polls.try_recv().is_err());
}

/// C's base write posts with the callParamCallbacks after the driver call
/// (asynMotorController.cpp:203-245, :277-460), inside the write.
#[test]
fn a_port_write_to_an_axis_is_posted_after_the_driver_call() {
    use asyn_rs::interrupt::InterruptFilter;
    let gate = epics_base_rs::runtime::interrupt_accept::InterruptGate::new();
    let port = register_controller_port(&gate, "CPPOSTW", 2, ()).unwrap();
    let mut ax = axis("CPPOSTW", 1);
    let subscribe = |reason| {
        port.interrupts().register_interrupt_user(
            InterruptFilter {
                reason: Some(reason),
                addr: Some(1),
                ..Default::default()
            },
            16,
        )
    };
    let (_c, mut closed_loop) = subscribe(index::CLOSED_LOOP);
    let (_m, mut move_abs) = subscribe(index::MOVE_ABS);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    // Past the asyn-boot-flush the gate starts, which would post the writes.
    // A STATUS post can be register_axis's own notify, queued before the
    // gate opened; a param set without a callback is posted by the flush.
    let marker = asyn_rs::request::ParamSetValue::new(
        index::MOVE_REL,
        1,
        asyn_rs::param::ParamValue::Float64(-1.0),
    );
    port.set_params_blocking(1, vec![marker]).unwrap();
    let (_r, mut move_rel) = subscribe(index::MOVE_REL);
    gate.set(true);
    let flushed =
        rt.block_on(async { tokio::time::timeout(Duration::from_secs(10), move_rel.recv()).await });
    assert!(
        matches!(flushed, Ok(Some(_))),
        "the boot flush: {flushed:?}"
    );
    port.write_int32_blocking(index::CLOSED_LOOP, 1, 1).unwrap();
    port.write_float64_blocking(index::MOVE_ABS, 1, 10.0)
        .unwrap();
    assert_eq!(next(&mut ax), MotorCommand::SetClosedLoop { enable: true });
    assert!(matches!(next(&mut ax), MotorCommand::MoveAbsolute { .. }));
    let posted = |rx: &mut asyn_rs::interrupt::InterruptReceiver| {
        rt.block_on(async { tokio::time::timeout(Duration::from_secs(10), rx.recv()).await })
            .ok()
            .flatten()
            .is_some()
    };
    assert!(posted(&mut closed_loop));
    assert!(posted(&mut move_abs));
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

#[test]
fn initialize_profile_sizes_every_axis_and_reaches_the_axis_drivers() {
    let port = register_controller_port(&open_owner(), "CPPINIT", 2, ()).unwrap();
    let mut ax = axis("CPPINIT", 1);
    let sizes = port
        .with_driver_blocking::<ControllerPort<()>, _>(|d| {
            d.initialize_profile(5).unwrap();
            d.with_params(|_, ctx| {
                let p = &ctx.profile;
                (
                    p.max_points,
                    p.times.clone(),
                    p.axes.len(),
                    p.axes[1].readbacks.len(),
                )
            })
        })
        .unwrap();
    assert_eq!(sizes, (5, vec![0.0; 5], 2, 5));
    assert_eq!(
        next(&mut ax),
        MotorCommand::ProfileInitialize { max_points: 5 }
    );
}

/// A driver's `initializeProfile` that runs its own step around the base.
#[derive(Default)]
struct OwnInit(usize);

impl ControllerParams for OwnInit {
    fn initialize_profile(
        &mut self,
        ctx: &mut ControllerCtx<'_>,
        max_points: usize,
    ) -> AsynResult<()> {
        self.0 = max_points * 2;
        ctx.initialize_profile(max_points)
    }
}

#[test]
fn a_driver_initialize_profile_override_runs_and_calls_the_base() {
    let port = register_controller_port(&open_owner(), "CPPOWN", 1, OwnInit::default()).unwrap();
    let seen = port
        .with_driver_blocking::<ControllerPort<OwnInit>, _>(|d| {
            d.initialize_profile(3).unwrap();
            d.with_params(|own, ctx| (own.0, ctx.profile.max_points))
        })
        .unwrap();
    assert_eq!(seen, (6, 3));
}

#[test]
fn the_profile_indices_name_cs_parameters() {
    for (i, name) in [
        (index::ENCODER_POSITION, "MOTOR_ENCODER_POSITION"),
        (index::PROFILE_NUM_POINTS, "PROFILE_NUM_POINTS"),
        (index::PROFILE_TIME_MODE, "PROFILE_TIME_MODE"),
        (index::PROFILE_FIXED_TIME, "PROFILE_FIXED_TIME"),
        (index::PROFILE_BUILD, "PROFILE_BUILD"),
        (index::PROFILE_EXECUTE, "PROFILE_EXECUTE"),
        (index::PROFILE_ABORT, "PROFILE_ABORT"),
        (index::PROFILE_READBACK, "PROFILE_READBACK"),
        (index::REC_RESOLUTION, "MOTOR_REC_RESOLUTION"),
        (index::REC_DIRECTION, "MOTOR_REC_DIRECTION"),
        (index::REC_OFFSET, "MOTOR_REC_OFFSET"),
        (index::PROFILE_NUM_READBACKS, "PROFILE_NUM_READBACKS"),
        (index::PROFILE_TIME_ARRAY, "PROFILE_TIME_ARRAY"),
        (index::PROFILE_POSITIONS, "PROFILE_POSITIONS"),
        (index::PROFILE_READBACKS, "PROFILE_READBACKS"),
        (index::PROFILE_FOLLOWING_ERRORS, "PROFILE_FOLLOWING_ERRORS"),
        (index::PROFILE_CURRENT_POINT, "PROFILE_CURRENT_POINT"),
        (index::PROFILE_NUM_PULSES, "PROFILE_NUM_PULSES"),
        (index::PROFILE_START_PULSES, "PROFILE_START_PULSES"),
        (index::PROFILE_END_PULSES, "PROFILE_END_PULSES"),
        (index::PROFILE_ACTUAL_PULSES, "PROFILE_ACTUAL_PULSES"),
        (index::PROFILE_ACCELERATION, "PROFILE_ACCELERATION"),
        (index::PROFILE_MOVE_MODE, "PROFILE_MOVE_MODE"),
        (index::PROFILE_BUILD_STATE, "PROFILE_BUILD_STATE"),
        (index::PROFILE_BUILD_STATUS, "PROFILE_BUILD_STATUS"),
        (index::PROFILE_BUILD_MESSAGE, "PROFILE_BUILD_MESSAGE"),
        (index::PROFILE_EXECUTE_STATE, "PROFILE_EXECUTE_STATE"),
        (index::PROFILE_EXECUTE_STATUS, "PROFILE_EXECUTE_STATUS"),
        (index::PROFILE_EXECUTE_MESSAGE, "PROFILE_EXECUTE_MESSAGE"),
        (index::PROFILE_READBACK_STATE, "PROFILE_READBACK_STATE"),
        (index::PROFILE_READBACK_STATUS, "PROFILE_READBACK_STATUS"),
        (index::PROFILE_READBACK_MESSAGE, "PROFILE_READBACK_MESSAGE"),
        (index::PROFILE_USE_AXIS, "PROFILE_USE_AXIS"),
    ] {
        assert_eq!(BASE_PARAMS[i].0, name);
    }
}

fn profile_times(port: &asyn_rs::port_handle::PortHandle) -> Vec<f64> {
    port.with_driver_blocking::<ControllerPort<()>, _>(|d| {
        d.with_params(|_, ctx| ctx.profile.times.clone())
    })
    .unwrap()
}

#[test]
fn build_profile_fills_the_fixed_times_and_reaches_every_axis() {
    let port = register_controller_port(&open_owner(), "CPPBUILD", 3, ()).unwrap();
    let mut ax0 = axis("CPPBUILD", 0);
    let mut ax2 = axis("CPPBUILD", 2);
    port.with_driver_blocking::<ControllerPort<()>, _>(|d| d.initialize_profile(4))
        .unwrap()
        .unwrap();
    next(&mut ax0);
    next(&mut ax2);
    // The base reads its parameters at the controller address.
    port.write_int32_blocking(index::PROFILE_TIME_MODE, 0, PROFILE_TIME_MODE_FIXED)
        .unwrap();
    port.write_float64_blocking(index::PROFILE_FIXED_TIME, 0, 0.5)
        .unwrap();
    port.write_int32_blocking(index::PROFILE_NUM_POINTS, 0, 2)
        .unwrap();
    // Written at axis 2, built for the controller.
    port.write_int32_blocking(index::PROFILE_BUILD, 2, 1)
        .unwrap();
    assert_eq!(profile_times(&port), [0.5, 0.5, 0.0, 0.0]);
    assert_eq!(next(&mut ax0), MotorCommand::ProfileBuild);
    assert_eq!(next(&mut ax2), MotorCommand::ProfileBuild);
    assert_eq!(
        port.read_int32_blocking(index::PROFILE_BUILD, 2).unwrap(),
        1
    );
}

#[test]
fn build_profile_fails_on_an_undefined_parameter() {
    let port = register_controller_port(&open_owner(), "CPPBUNDEF", 1, ()).unwrap();
    let ax = axis("CPPBUNDEF", 0);
    assert!(
        port.write_int32_blocking(index::PROFILE_BUILD, 0, 1)
            .is_err()
    );
    assert!(no_call(&ax));
}

#[test]
fn the_execute_state_starts_done() {
    let port = register_controller_port(&open_owner(), "CPPESTATE", 1, ()).unwrap();
    assert_eq!(
        port.read_int32_blocking(index::PROFILE_EXECUTE_STATE, 0)
            .unwrap(),
        PROFILE_EXECUTE_DONE
    );
}

#[test]
fn execute_profile_reaches_every_axis() {
    let port = register_controller_port(&open_owner(), "CPPEXEC", 2, ()).unwrap();
    let mut ax0 = axis("CPPEXEC", 0);
    let mut ax1 = axis("CPPEXEC", 1);
    port.write_int32_blocking(index::PROFILE_EXECUTE, 0, 1)
        .unwrap();
    assert_eq!(next(&mut ax0), MotorCommand::ProfileExecute);
    assert_eq!(next(&mut ax1), MotorCommand::ProfileExecute);
}

#[test]
fn abort_profile_reaches_every_axis() {
    let port = register_controller_port(&open_owner(), "CPPABORT", 2, ()).unwrap();
    let mut ax0 = axis("CPPABORT", 0);
    let mut ax1 = axis("CPPABORT", 1);
    port.write_int32_blocking(index::PROFILE_ABORT, 1, 1)
        .unwrap();
    assert_eq!(next(&mut ax0), MotorCommand::ProfileAbort);
    assert_eq!(next(&mut ax1), MotorCommand::ProfileAbort);
}

#[test]
fn readback_profile_converts_each_axis_to_user_units_and_posts_it() {
    use asyn_rs::interrupt::InterruptFilter;
    use asyn_rs::param::ParamValue;
    let port = register_controller_port(&open_owner(), "CPPREAD", 2, ()).unwrap();
    let _ax0 = axis("CPPREAD", 0);
    let mut ax1 = axis("CPPREAD", 1);
    port.with_driver_blocking::<ControllerPort<()>, _>(|d| {
        d.initialize_profile(3).unwrap();
        d.with_params(|_, ctx| {
            ctx.profile.axes[1].readbacks = vec![10.0, 20.0, 30.0];
            ctx.profile.axes[1].following_errors = vec![1.0, 2.0, 3.0];
        })
    })
    .unwrap();
    next(&mut ax1);
    port.write_float64_blocking(index::REC_RESOLUTION, 1, 0.5)
        .unwrap();
    port.write_float64_blocking(index::REC_OFFSET, 1, 100.0)
        .unwrap();
    port.write_int32_blocking(index::REC_DIRECTION, 1, 1)
        .unwrap();
    port.write_int32_blocking(index::PROFILE_NUM_READBACKS, 0, 2)
        .unwrap();
    let subscribe = |reason| {
        port.interrupts().register_interrupt_user(
            InterruptFilter {
                reason: Some(reason),
                addr: Some(1),
                ..Default::default()
            },
            1024,
        )
    };
    let (_r, mut readbacks) = subscribe(index::PROFILE_READBACKS);
    let (_f, mut errors) = subscribe(index::PROFILE_FOLLOWING_ERRORS);
    // Axis 0's record units are undefined: its conversion fails, unreported.
    port.write_int32_blocking(index::PROFILE_READBACK, 1, 1)
        .unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let posted = |rx: &mut asyn_rs::interrupt::InterruptReceiver| {
        let iv = rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(1), rx.recv()).await })
            .unwrap()
            .unwrap();
        match iv.value {
            ParamValue::Float64Array(a) => a.to_vec(),
            other => panic!("{other:?}"),
        }
    };
    // Direction 1 negates the resolution; only NUM_READBACKS points convert.
    assert_eq!(posted(&mut readbacks), [95.0, 90.0]);
    assert_eq!(posted(&mut errors), [-0.5, -1.0]);
    let arrays = port
        .with_driver_blocking::<ControllerPort<()>, _>(|d| {
            d.with_params(|_, ctx| ctx.profile.axes[1].readbacks.clone())
        })
        .unwrap();
    assert_eq!(arrays, [95.0, 90.0, 30.0]);
    assert_eq!(next(&mut ax1), MotorCommand::ProfileReadback);
}

fn write_array(
    port: &asyn_rs::port_handle::PortHandle,
    reason: usize,
    addr: i32,
    data: Vec<f64>,
) -> AsynResult<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(port.write_float64_array(reason, addr, data))
}

fn axis_positions<P: ControllerParams>(
    port: &asyn_rs::port_handle::PortHandle,
    addr: usize,
) -> Vec<f64> {
    port.with_driver_blocking::<ControllerPort<P>, _>(move |d| {
        d.with_params(|_, ctx| ctx.profile.axes[addr].positions.clone())
    })
    .unwrap()
}

#[test]
fn profile_arrays_are_cut_to_the_profile_and_positions_go_to_controller_units() {
    let port = register_controller_port(&open_owner(), "CPPWARR", 2, ()).unwrap();
    let mut ax0 = axis("CPPWARR", 0);
    let mut ax1 = axis("CPPWARR", 1);
    port.with_driver_blocking::<ControllerPort<()>, _>(|d| d.initialize_profile(3))
        .unwrap()
        .unwrap();
    write_array(
        &port,
        index::PROFILE_TIME_ARRAY,
        0,
        vec![0.1, 0.2, 0.3, 0.4],
    )
    .unwrap();
    assert_eq!(profile_times(&port), [0.1, 0.2, 0.3]);
    port.write_float64_blocking(index::REC_RESOLUTION, 1, 0.5)
        .unwrap();
    port.write_float64_blocking(index::REC_OFFSET, 1, 1.0)
        .unwrap();
    port.write_int32_blocking(index::REC_DIRECTION, 1, 1)
        .unwrap();
    write_array(&port, index::PROFILE_POSITIONS, 1, vec![2.0, 3.0]).unwrap();
    assert_eq!(axis_positions::<()>(&port, 1), [-2.0, -4.0, 0.0]);
    next(&mut ax1);
    // The axis driver's defineProfile gets the converted points.
    assert_eq!(
        next(&mut ax1),
        MotorCommand::ProfileDefine {
            positions: vec![-2.0, -4.0]
        }
    );
    // Axis 0's record units are undefined: its defineProfile fails, the write does not.
    write_array(&port, index::PROFILE_POSITIONS, 0, vec![2.0]).unwrap();
    assert_eq!(axis_positions::<()>(&port, 0), [0.0, 0.0, 0.0]);
    next(&mut ax0);
    assert!(no_call(&ax0));
}

#[test]
fn an_array_write_to_another_parameter_fails() {
    let port = register_controller_port(&open_owner(), "CPPWBAD", 1, ()).unwrap();
    let _ax = axis("CPPWBAD", 0);
    assert!(write_array(&port, index::PROFILE_READBACKS, 0, vec![1.0]).is_err());
}

/// SPiiPlus's shape: its own units on top of the base conversion.
#[derive(Default)]
struct Scaled;

impl ControllerParams for Scaled {
    fn define_profile(
        &mut self,
        ctx: &mut ControllerCtx<'_>,
        addr: i32,
        positions: &[f64],
    ) -> AsynResult<()> {
        ctx.define_profile(addr, positions)?;
        for p in &mut ctx.profile.axes[addr as usize].positions[..positions.len()] {
            *p *= 10.0;
        }
        Ok(())
    }
}

#[test]
fn a_driver_define_profile_override_runs_on_the_base_conversion() {
    let port = register_controller_port(&open_owner(), "CPPWOWN", 1, Scaled).unwrap();
    let _ax = axis("CPPWOWN", 0);
    port.with_driver_blocking::<ControllerPort<Scaled>, _>(|d| d.initialize_profile(2))
        .unwrap()
        .unwrap();
    port.write_float64_blocking(index::REC_RESOLUTION, 0, 2.0)
        .unwrap();
    port.write_float64_blocking(index::REC_OFFSET, 0, 0.0)
        .unwrap();
    port.write_int32_blocking(index::REC_DIRECTION, 0, 0)
        .unwrap();
    write_array(&port, index::PROFILE_POSITIONS, 0, vec![4.0, 6.0]).unwrap();
    assert_eq!(axis_positions::<Scaled>(&port, 0), [20.0, 30.0]);
}

fn read_array(
    port: &asyn_rs::port_handle::PortHandle,
    reason: usize,
    addr: i32,
    max: usize,
) -> AsynResult<Vec<f64>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(port.read_float64_array(reason, addr, max))
        .map(|a| a.to_vec())
}

#[test]
fn read_float64_array_returns_num_readbacks_of_the_axis_arrays() {
    let port = register_controller_port(&open_owner(), "CPPRARR", 2, ()).unwrap();
    let _ax0 = axis("CPPRARR", 0);
    let _ax1 = axis("CPPRARR", 1);
    port.with_driver_blocking::<ControllerPort<()>, _>(|d| {
        d.initialize_profile(4).unwrap();
        d.with_params(|_, ctx| {
            ctx.profile.axes[1].readbacks = vec![1.0, 2.0, 3.0, 4.0];
            ctx.profile.axes[1].following_errors = vec![0.1, 0.2, 0.3, 0.4];
        })
    })
    .unwrap();
    port.write_int32_blocking(index::PROFILE_NUM_READBACKS, 0, 3)
        .unwrap();
    assert_eq!(
        read_array(&port, index::PROFILE_READBACKS, 1, 10).unwrap(),
        [1.0, 2.0, 3.0]
    );
    assert_eq!(
        read_array(&port, index::PROFILE_FOLLOWING_ERRORS, 1, 2).unwrap(),
        [0.1, 0.2]
    );
    assert!(read_array(&port, index::PROFILE_POSITIONS, 1, 10).is_err());
}

#[test]
fn an_encoder_position_write_reaches_the_axis() {
    let port = register_controller_port(&open_owner(), "CPENC", 1, ()).unwrap();
    let mut ax = axis("CPENC", 0);
    port.write_float64_blocking(index::ENCODER_POSITION, 0, 12.5)
        .unwrap();
    assert_eq!(
        next(&mut ax),
        MotorCommand::SetEncoderPosition { position: 12.5 }
    );
}

/// nf874x's shape: an axis that defines its encoder position.
#[derive(Default)]
struct Encoder(Vec<f64>);

impl asyn_rs::interfaces::motor::AsynMotor for Encoder {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn move_relative(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn move_velocity(&mut self, _: &AsynUser, _: f64, _: f64, _: f64) -> AsynResult<()> {
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
    fn set_encoder_position(&mut self, _: &AsynUser, position: f64) -> AsynResult<()> {
        self.0.push(position);
        Ok(())
    }
    fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
        asyn_rs::interfaces::motor::PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser) -> AsynResult<asyn_rs::interfaces::motor::MotorStatus> {
        Ok(Default::default())
    }
}

#[test]
fn set_encoder_position_calls_the_driver_and_posts_the_encoder_position() {
    use asyn_rs::interfaces::motor::StatusWrite;
    let mut motor = Encoder::default();
    let mut fields = Vec::new();
    motor_rs::poll_loop::execute_commands(
        &mut motor,
        &motor_rs::trace::MotorTrace::new("CPENCX", 0),
        &[MotorCommand::SetEncoderPosition { position: 7.0 }],
        &mut |w| fields.extend(w.fields),
    );
    assert_eq!(motor.0, [7.0]);
    assert!(matches!(fields[..], [StatusWrite::EncoderPosition(p)] if p == 7.0));
}

/// LC400's shape: its own registers never reach the base.
struct Registers {
    servo: usize,
    fail: bool,
}

impl ControllerParams for Registers {
    fn create_params(&mut self, base: &mut PortDriverBase) -> AsynResult<()> {
        self.servo = base.create_param("SERVO", ParamType::Int32)?;
        Ok(())
    }

    fn write_int32(&mut self, _: &mut PortDriverBase, user: AxisUser<'_>, _: i32) -> ParamWrite {
        if user.reason != self.servo {
            return ParamWrite::PassToBase(Ok(()));
        }
        ParamWrite::Handled(if self.fail {
            Err(asyn_rs::error::AsynError::Status {
                status: asyn_rs::error::AsynStatus::Error,
                message: String::new(),
            })
        } else {
            Ok(())
        })
    }
}

#[test]
fn a_handled_write_skips_the_base_store() {
    let regs = Registers {
        servo: 0,
        fail: false,
    };
    let port = register_controller_port(&open_owner(), "CPHANDLED", 1, regs).unwrap();
    let _ax = axis("CPHANDLED", 0);
    let servo = BASE_PARAMS.len();
    port.write_int32_blocking(servo, 0, 1).unwrap();
    assert!(
        port.read_int32_blocking(servo, 0).is_err(),
        "C never stores it"
    );
    // A base parameter still passes to the base.
    port.write_int32_blocking(index::DEFER_MOVES, 0, 1).unwrap();
    assert_eq!(port.read_int32_blocking(index::DEFER_MOVES, 0).unwrap(), 1);
}

#[test]
fn a_handled_write_returns_its_own_failure() {
    let regs = Registers {
        servo: 0,
        fail: true,
    };
    let port = register_controller_port(&open_owner(), "CPHANDFAIL", 1, regs).unwrap();
    let _ax = axis("CPHANDFAIL", 0);
    assert!(port.write_int32_blocking(BASE_PARAMS.len(), 0, 1).is_err());
}

#[test]
fn a_defined_profile_reaches_the_sim_motor() {
    use asyn_rs::interfaces::motor::AsynMotor;
    let mut motor = motor_rs::sim_motor::SimMotor::new();
    motor_rs::poll_loop::execute_commands(
        &mut motor,
        &motor_rs::trace::MotorTrace::new("CPSIMDEF", 0),
        &[
            MotorCommand::ProfileDefine {
                positions: vec![1.0, 2.0],
            },
            MotorCommand::ProfileExecute,
        ],
        &mut |_| {},
    );
    assert_eq!(
        motor.readback_profile(&AsynUser::new(0)).unwrap(),
        [1.0, 2.0]
    );
}

#[test]
fn a_failed_profile_build_prints_cs_writeint32_error_line() {
    use asyn_rs::manager::PortManager;
    use asyn_rs::trace::TraceFile;
    let port = register_controller_port(&open_owner(), "CPTRACE", 1, ()).unwrap();
    let _ax = axis("CPTRACE", 0);
    let path = std::env::temp_dir().join(format!("cp-trace-{}", std::process::id()));
    let file = std::fs::File::create(&path).unwrap();
    PortManager::global().trace_manager().set_device_trace_file(
        "CPTRACE",
        0,
        TraceFile::File(Arc::new(Mutex::new(file))),
    );
    // The base reads PROFILE_TIME_MODE, never set: buildProfile fails.
    assert!(
        port.write_int32_blocking(index::PROFILE_BUILD, 0, 1)
            .is_err()
    );
    let text = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let line = text
        .lines()
        .find(|l| l.contains("asynMotorController"))
        .expect("the error line");
    assert!(
        line.ends_with(&format!(
            "asynMotorController:writeInt32 error, status=3 axis=0, function={}, value=1",
            index::PROFILE_BUILD
        )),
        "{line}"
    );
}

/// A driver whose buildProfile refuses with a param-library status.
struct UndefinedBuild;

impl ControllerParams for UndefinedBuild {
    fn build_profile(
        &mut self,
        _: &mut motor_rs::controller_port::ControllerCtx<'_>,
    ) -> AsynResult<()> {
        Err(asyn_rs::error::AsynError::ParamUndefined(
            index::PROFILE_TIME_MODE,
        ))
    }
}

/// C writeFloat64 reads the speeds inside the move branches alone, and a
/// velocity move reads no VELOCITY (asynMotorController.cpp:291-344):
/// another write prints no undefined-parameter line for them.
#[test]
fn only_a_move_write_reads_the_speeds() {
    use asyn_rs::manager::PortManager;
    use asyn_rs::trace::{TraceFile, TraceMask};
    let port = register_controller_port(&open_owner(), "CPSPEEDREAD", 2, ()).unwrap();
    let _a0 = axis("CPSPEEDREAD", 0);
    let _a1 = axis("CPSPEEDREAD", 1);
    let path = std::env::temp_dir().join(format!("cp-speed-read-{}", std::process::id()));
    let file = std::fs::File::create(&path).unwrap();
    let manager = PortManager::global();
    let traces = manager.trace_manager();
    traces.set_trace_mask(Some("CPSPEEDREAD"), TraceMask::FLOW);
    traces.set_device_trace_file(
        "CPSPEEDREAD",
        0,
        TraceFile::File(Arc::new(Mutex::new(file))),
    );
    let undefined = |name: &str| {
        let text = std::fs::read_to_string(&path).unwrap();
        text.lines()
            .any(|l| l.contains("getDoubleParam") && l.contains(&format!(" {name},")))
    };
    port.write_float64_blocking(index::VELOCITY, 0, 2.0)
        .unwrap();
    for name in ["MOTOR_VEL_BASE", "MOTOR_ACCEL"] {
        assert!(!undefined(name), "a VELOCITY write read {name}");
    }
    port.write_float64_blocking(index::MOVE_VEL, 1, 2.0)
        .unwrap();
    assert!(
        undefined("MOTOR_VEL_BASE"),
        "the velocity move reads VEL_BASE"
    );
    assert!(
        !undefined("MOTOR_VELOCITY"),
        "the velocity move read VELOCITY"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_param_library_refusal_prints_its_own_status_code() {
    use asyn_rs::manager::PortManager;
    use asyn_rs::trace::TraceFile;
    let port = register_controller_port(&open_owner(), "CPTRACEPARAM", 1, UndefinedBuild).unwrap();
    let _ax = axis("CPTRACEPARAM", 0);
    let path = std::env::temp_dir().join(format!("cp-trace-param-{}", std::process::id()));
    let file = std::fs::File::create(&path).unwrap();
    PortManager::global().trace_manager().set_device_trace_file(
        "CPTRACEPARAM",
        0,
        TraceFile::File(Arc::new(Mutex::new(file))),
    );
    assert!(
        port.write_int32_blocking(index::PROFILE_BUILD, 0, 1)
            .is_err()
    );
    let text = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    // asynParamUndefined is 10; AsynStatus has no such value.
    assert!(
        text.lines().any(|l| l.ends_with(&format!(
            "asynMotorController:writeInt32 error, status=10 axis=0, function={}, value=1",
            index::PROFILE_BUILD
        ))),
        "{text}"
    );
}

/// What a driver's write hook saw: address, function, value, status.
type Seen = Arc<Mutex<Vec<(i32, usize, WriteValue, bool)>>>;

/// LC400's shape: refuses the axes with no controller address and reports
/// every write after it; pmac's: sets COMMS_ERROR from each write's status;
/// parker's: a `failing` write runs a command after the base that fails.
struct Gate {
    refused: i32,
    failing: [usize; 2],
    seen: Seen,
}

impl WriteHook for Gate {
    fn check(&self, addr: i32, _: usize) -> AsynResult<()> {
        if addr == self.refused {
            return Err(asyn_rs::error::AsynError::Status {
                status: asyn_rs::error::AsynStatus::Error,
                message: String::new(),
            });
        }
        Ok(())
    }

    fn after_write(&self, write: &FinishedWrite<'_>) -> WriteOutcome {
        self.seen.lock().unwrap().push((
            write.addr,
            write.function,
            write.value,
            write.status.is_ok(),
        ));
        let mut delta = ParamDelta::default();
        delta.set_int32(index::STATUS_COMMS_ERROR, write.status.is_err() as i32);
        let status = self.failing.contains(&write.function).then(|| {
            Err(asyn_rs::error::AsynError::Status {
                status: asyn_rs::error::AsynStatus::Error,
                message: "MR failed".into(),
            })
        });
        WriteOutcome { delta, status }
    }
}

/// [`Registers`] with a [`Gate`] refusing axis 1.
struct Hooked {
    registers: Registers,
    gate: Arc<Gate>,
}

impl Hooked {
    fn new() -> (Self, Seen) {
        let seen = Seen::default();
        let hooked = Self {
            registers: Registers {
                servo: 0,
                fail: true,
            },
            gate: Arc::new(Gate {
                refused: 1,
                failing: [index::REC_OFFSET, index::CLOSED_LOOP],
                seen: seen.clone(),
            }),
        };
        (hooked, seen)
    }
}

impl ControllerParams for Hooked {
    fn create_params(&mut self, base: &mut PortDriverBase) -> AsynResult<()> {
        self.registers.create_params(base)
    }

    fn write_hook(&self) -> Option<Arc<dyn WriteHook>> {
        Some(self.gate.clone())
    }

    fn write_int32(
        &mut self,
        base: &mut PortDriverBase,
        user: AxisUser<'_>,
        value: i32,
    ) -> ParamWrite {
        self.registers.write_int32(base, user, value)
    }
}

fn take(seen: &Seen) -> Vec<(i32, usize, WriteValue, bool)> {
    std::mem::take(&mut *seen.lock().unwrap())
}

#[test]
fn the_hook_follows_a_write_the_port_finishes() {
    let (params, seen) = Hooked::new();
    let port = register_controller_port(&open_owner(), "CPHOOKPORT", 2, params).unwrap();
    let _ax = axis("CPHOOKPORT", 0);
    port.write_float64_blocking(index::VELOCITY, 0, 2.5)
        .unwrap();
    // The driver's own register, refused by its own code.
    let servo = BASE_PARAMS.len();
    assert!(port.write_int32_blocking(servo, 0, 1).is_err());
    assert_eq!(
        take(&seen),
        [
            (0, index::VELOCITY, WriteValue::Float64(2.5), true),
            (0, servo, WriteValue::Int32(1), false),
        ]
    );
    let comms = port.read_int32_blocking(index::STATUS_COMMS_ERROR, 0);
    assert_eq!(comms.unwrap(), 1, "the failed write's");
}

#[test]
fn the_hook_status_reaches_the_port_writer() {
    let (params, seen) = Hooked::new();
    let port = register_controller_port(&open_owner(), "CPHOOKSTAT", 2, params).unwrap();
    let _ax = axis("CPHOOKSTAT", 0);
    let e = port
        .write_float64_blocking(index::REC_OFFSET, 0, 1.5)
        .unwrap_err();
    assert!(e.to_string().contains("MR failed"), "{e}");
    assert_eq!(
        take(&seen),
        [(0, index::REC_OFFSET, WriteValue::Float64(1.5), true)],
        "the base's own write succeeded"
    );
}

#[test]
fn the_hook_status_reaches_the_record() {
    use asyn_rs::manager::PortManager;
    use asyn_rs::trace::TraceFile;
    let (params, seen) = Hooked::new();
    let _port = register_controller_port(&open_owner(), "CPHOOKSTAX", 2, params).unwrap();
    let path = std::env::temp_dir().join(format!("cp-hookstax-{}", std::process::id()));
    let file = std::fs::File::create(&path).unwrap();
    PortManager::global().trace_manager().set_device_trace_file(
        "CPHOOKSTAX",
        0,
        TraceFile::File(Arc::new(Mutex::new(file))),
    );
    let trace = motor_rs::trace::MotorTrace::new("CPHOOKSTAX", 0);
    trace.set_record("m1");
    motor_rs::poll_loop::execute_commands(
        &mut Encoder::default(),
        &trace,
        &[MotorCommand::SetClosedLoop { enable: true }],
        &mut |_| {},
    );
    let text = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        take(&seen),
        [(0, index::CLOSED_LOOP, WriteValue::Int32(1), true)],
        "the driver's own write succeeded"
    );
    assert!(
        text.contains(
            "devMotorAsyn::asynCallback: m1 pasyn{Float64,Int32}->write returned MR failed\n"
        ),
        "{text}"
    );
}

#[test]
fn the_hook_refuses_a_port_write_before_the_base() {
    let (params, seen) = Hooked::new();
    let port = register_controller_port(&open_owner(), "CPHOOKREF", 2, params).unwrap();
    let ax = axis("CPHOOKREF", 1);
    assert!(
        port.write_float64_blocking(index::MOVE_ABS, 1, 3.0)
            .is_err()
    );
    assert!(no_call(&ax));
    assert!(port.read_float64_blocking(index::MOVE_ABS, 1).is_err());
    assert_eq!(
        take(&seen),
        [(1, index::MOVE_ABS, WriteValue::Float64(3.0), false)]
    );
}

#[test]
fn a_port_write_an_axis_runs_reaches_the_hook_after_the_driver_call() {
    let (params, seen) = Hooked::new();
    let port = register_controller_port(&open_owner(), "CPHOOKAXIS", 2, params).unwrap();
    let mut ax = axis("CPHOOKAXIS", 0);
    port.write_float64_blocking(index::ENCODER_POSITION, 0, 4.0)
        .unwrap();
    assert_eq!(
        next(&mut ax),
        MotorCommand::SetEncoderPosition { position: 4.0 }
    );
    assert_eq!(
        take(&seen),
        [(0, index::ENCODER_POSITION, WriteValue::Float64(4.0), true)]
    );
    let comms = port.read_int32_blocking(index::STATUS_COMMS_ERROR, 0);
    assert_eq!(comms.unwrap(), 0);
}

#[test]
fn a_port_write_the_axis_runs_keeps_the_value_written() {
    let (params, seen) = Hooked::new();
    let port = register_controller_port(&open_owner(), "CPHOOKVAL", 2, params).unwrap();
    let mut ax = axis("CPHOOKVAL", 0);
    port.write_int32_blocking(index::STOP, 0, 1).unwrap();
    assert!(matches!(next(&mut ax), MotorCommand::Stop { .. }));
    // C stores and prints the value written, not one rebuilt from the
    // driver call (asynMotorController.cpp:204, :245-254).
    assert_eq!(take(&seen), [(0, index::STOP, WriteValue::Int32(1), true)]);
    assert_eq!(port.read_int32_blocking(index::STOP, 0).unwrap(), 1);
}

#[test]
fn the_hook_refuses_a_record_write_before_the_driver() {
    let (params, seen) = Hooked::new();
    let port = register_controller_port(&open_owner(), "CPHOOKREC", 2, params).unwrap();
    let mut motor = Encoder::default();
    let mut fields = Vec::new();
    motor_rs::poll_loop::execute_commands(
        &mut motor,
        &motor_rs::trace::MotorTrace::new("CPHOOKREC", 1),
        &[MotorCommand::SetEncoderPosition { position: 5.0 }],
        &mut |w| fields.extend(w.fields),
    );
    assert!(motor.0.is_empty());
    assert!(fields.is_empty());
    assert_eq!(
        take(&seen),
        [(1, index::ENCODER_POSITION, WriteValue::Float64(5.0), false)]
    );
    let comms = port.read_int32_blocking(index::STATUS_COMMS_ERROR, 1);
    assert_eq!(comms.unwrap(), 1, "the refused write's");
}

/// SMChydra's shape: the controller's line, then each axis's, then
/// `asynPortDriver::report`.
struct Reporting;

impl ControllerParams for Reporting {
    fn report(&self, ctx: &ControllerReport<'_>, out: &mut dyn std::fmt::Write, level: i32) {
        let _ = writeln!(out, "driver {} numAxes={}", ctx.port_name(), ctx.num_axes());
        ctx.report(out, level);
    }
}

struct AxisLine(i32);

impl AxisReport for AxisLine {
    fn report(&self, out: &mut dyn std::fmt::Write, level: i32) {
        if level > 0 {
            let _ = writeln!(out, "  axis {}", self.0);
        }
    }
}

#[test]
fn the_controller_report_runs_the_driver_then_each_axis_then_the_base() {
    register_controller_port(&open_owner(), "CPREPORT", 3, ()).unwrap();
    for addr in [0, 2] {
        let (motor_tx, _) = mpsc::unbounded_channel();
        let (poll_tx, _) = mpsc::channel(1);
        let axis = MotorAxis::new(
            TraceUser::Asyn {
                port: "CPREPORT".into(),
                addr,
            },
            Arc::new(Mutex::new(Recorder::default())),
            Duration::from_secs(1),
            poll_tx,
            motor_tx,
            AxisSubscribers::new(),
        );
        register_axis(axis.with_report(Arc::new(AxisLine(addr)))).unwrap();
    }
    let mut port = ControllerPort::new("CPREPORT", 3, Reporting).unwrap();
    let mut out = String::new();
    port.report(&mut out, 1);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[..5],
        [
            "driver CPREPORT numAxes=3",
            "  axis 0",
            "  axis 2",
            "Port: CPREPORT",
            lines[4]
        ],
        "{out}"
    );
    assert!(lines[4].starts_with("  Timestamp: "), "{out}");

    let mut out = String::new();
    port.report(&mut out, 0);
    assert_eq!(out, "driver CPREPORT numAxes=3\nPort: CPREPORT\n");
}

/// motorsim's shape: the report reads the axis's parameters, as C's
/// getDoubleParam/getIntegerParam do (motorSimDriver.cpp report).
struct SoftLimits;

impl ControllerParams for SoftLimits {
    fn report(&self, ctx: &ControllerReport<'_>, out: &mut dyn std::fmt::Write, _level: i32) {
        let high = ctx.float64_param(1, index::HIGH_LIMIT).unwrap();
        let low = ctx.float64_param(1, index::LOW_LIMIT).unwrap();
        let defer = ctx.int32_param(1, index::DEFER_MOVES).unwrap();
        let _ = writeln!(out, "high={high} low={low} defer={defer}");
        assert!(ctx.float64_param(1, index::DEFER_MOVES).is_err());
    }
}

#[test]
fn a_driver_report_reads_the_port_s_parameters() {
    let mut port = ControllerPort::new("CPREPORTPARAM", 2, SoftLimits).unwrap();
    let base = port.base_mut();
    base.set_float64_param(index::HIGH_LIMIT, 1, 5.5).unwrap();
    base.set_float64_param(index::LOW_LIMIT, 1, -2.0).unwrap();
    base.set_int32_param(index::DEFER_MOVES, 1, 1).unwrap();
    let mut out = String::new();
    port.report(&mut out, 0);
    assert_eq!(out, "high=5.5 low=-2 defer=1\n");
}

/// ACR's shape: `writeUInt32Digital` on its binary outputs sends `BIT n=v`
/// and never reaches the base (ACRMotorDriver.cpp:232-248).
#[derive(Default)]
struct BinaryOut {
    out: usize,
    seen: Arc<Mutex<Vec<(u32, u32)>>>,
}

impl ControllerParams for BinaryOut {
    fn create_params(&mut self, base: &mut PortDriverBase) -> AsynResult<()> {
        self.out = base.create_param("BINARY_OUT", ParamType::UInt32Digital)?;
        base.create_param("BINARY_STORED", ParamType::UInt32Digital)?;
        Ok(())
    }

    fn write_uint32_digital(
        &mut self,
        _: &mut PortDriverBase,
        user: AxisUser<'_>,
        value: u32,
        mask: u32,
    ) -> ParamWrite {
        if user.reason != self.out {
            return ParamWrite::PassToBase(Ok(()));
        }
        self.seen.lock().unwrap().push((value, mask));
        ParamWrite::Handled(Ok(()))
    }
}

#[test]
fn a_uint32_digital_write_reaches_the_driver_s_own_code() {
    use asyn_rs::sync_io::SyncIOHandle;
    let regs = BinaryOut::default();
    let seen = regs.seen.clone();
    let port = register_controller_port(&open_owner(), "CPBINOUT", 1, regs).unwrap();
    let _ax = axis("CPBINOUT", 0);
    let io = SyncIOHandle::from_handle(port.clone(), 0, Duration::from_secs(1));
    let (out, stored) = (BASE_PARAMS.len(), BASE_PARAMS.len() + 1);
    io.write_uint32_digital(out, 1, 0x4).unwrap();
    assert_eq!(*seen.lock().unwrap(), [(1, 0x4)]);
    assert!(
        io.read_uint32_digital(out, 0xffff_ffff).is_err(),
        "C never stores it"
    );
    // asynMotorController has no writeUInt32Digital: the rest is asynPortDriver's.
    io.write_uint32_digital(stored, 0x6, 0x2).unwrap();
    assert_eq!(io.read_uint32_digital(stored, 0xffff_ffff).unwrap(), 0x2);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// phytron's shape: a parameter whose read asks the controller, after the
/// base read as C's `phytronController::readInt32` calls it
/// (phytronAxisMotor.cpp:285-357, :472-512).
#[derive(Default)]
struct Queried {
    current: usize,
    temp: usize,
}

impl ControllerParams for Queried {
    fn create_params(&mut self, base: &mut PortDriverBase) -> AsynResult<()> {
        self.current = base.create_param("CURRENT", ParamType::Int32)?;
        self.temp = base.create_param("TEMP", ParamType::Float64)?;
        Ok(())
    }

    fn read_int32(&mut self, base: &mut PortDriverBase, user: AxisUser<'_>) -> AsynResult<i32> {
        let stored = base.read_int32_param(user.asyn());
        if user.reason == self.current {
            return Ok(10 * (user.addr + 1));
        }
        stored
    }

    fn read_float64(&mut self, base: &mut PortDriverBase, user: AxisUser<'_>) -> AsynResult<f64> {
        let stored = base.read_float64_param(user.asyn());
        if user.reason == self.temp {
            return Ok(21.5);
        }
        stored
    }
}

#[test]
fn a_driver_read_override_answers_its_own_params_and_leaves_the_base_s() {
    let port = register_controller_port(&open_owner(), "CPREAD", 2, Queried::default()).unwrap();
    let _ax = axis("CPREAD", 0);
    let (current, temp) = (BASE_PARAMS.len(), BASE_PARAMS.len() + 1);
    assert_eq!(port.read_int32_blocking(current, 1).unwrap(), 20);
    assert_eq!(port.read_float64_blocking(temp, 0).unwrap(), 21.5);
    port.write_int32_blocking(index::DEFER_MOVES, 0, 1).unwrap();
    assert_eq!(port.read_int32_blocking(index::DEFER_MOVES, 0).unwrap(), 1);
    assert!(
        port.read_float64_blocking(index::ACCEL, 0).is_err(),
        "undefined"
    );
}

/// SPiiPlus's shape: a `getAddress` that takes a tag past maxAddr-1 to list
/// 0, and a `drvUserCreate` that cuts `START_<name>` to the shared START_
/// parameter with `<name>` as the record's drvUser
/// (SPiiPlusDriver.cpp:306-322, :342-357).
/// `(the user's address, the value, its program name)` per START_ write.
type ProgramWrites = Arc<Mutex<Vec<(i32, i32, Option<String>)>>>;

#[derive(Default)]
struct Programs {
    start: usize,
    seen: ProgramWrites,
}

fn tag_address(space: asyn_rs::port::AddressSpace, addr: i32) -> Result<i32, i32> {
    if addr > space.max_addr as i32 - 1 {
        Ok(0)
    } else {
        asyn_rs::port::port_get_address(space, addr)
    }
}

impl ControllerParams for Programs {
    fn create_params(&mut self, base: &mut PortDriverBase) -> AsynResult<()> {
        self.start = base.create_param("START_", ParamType::Int32)?;
        base.set_get_address(tag_address);
        Ok(())
    }

    fn drv_user_create(
        &mut self,
        base: &mut PortDriverBase,
        req: &asyn_rs::port::DrvUserRequest,
    ) -> AsynResult<asyn_rs::port::DrvUserInfo> {
        let Some(name) = req
            .drv_info
            .strip_prefix("START_")
            .filter(|n| !n.is_empty())
        else {
            return base.drv_user_create(req);
        };
        let shared = asyn_rs::port::DrvUserRequest::new("START_", req.addr);
        Ok(base
            .drv_user_create(&shared)?
            .with_drv_user(Arc::new(name.to_string())))
    }

    fn write_int32(
        &mut self,
        _: &mut PortDriverBase,
        user: AxisUser<'_>,
        value: i32,
    ) -> ParamWrite {
        if user.reason != self.start {
            return ParamWrite::PassToBase(Ok(()));
        }
        let name = user
            .asyn()
            .drv_user
            .as_ref()
            .and_then(|d| d.downcast_ref::<String>())
            .cloned();
        self.seen
            .lock()
            .unwrap()
            .push((user.asyn().addr, value, name));
        ParamWrite::Handled(Ok(()))
    }
}

#[test]
fn a_driver_drv_user_create_and_get_address_reach_its_writes() {
    let params = Programs::default();
    let seen = params.seen.clone();
    let port = register_controller_port(&open_owner(), "CPPROG", 2, params).unwrap();
    let _ax = axis("CPPROG", 0);
    let info = port
        .drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new("START_PROG1", 30))
        .unwrap();
    assert_eq!(info.reason, BASE_PARAMS.len());
    let user = AsynUser::new(info.reason)
        .with_addr(30)
        .with_drv_user(info.drv_user);
    port.submit_blocking(asyn_rs::request::RequestOp::Int32Write { value: 1 }, user)
        .unwrap();
    assert_eq!(*seen.lock().unwrap(), [(30, 1, Some("PROG1".to_string()))]);
}

/// phytron's controller records: C's subclass `writeInt32` runs at an
/// address with no axis, and only the base it calls refuses there
/// (asynMotorController.cpp `getAxis` → asynError; phytronAxisMotor.cpp:365-385).
#[derive(Default)]
struct ControllerWrites {
    reset: usize,
    refused: usize,
    gain: usize,
    seen: Arc<Mutex<Vec<(usize, i32, bool)>>>,
}

impl ControllerParams for ControllerWrites {
    fn create_params(&mut self, base: &mut PortDriverBase) -> AsynResult<()> {
        self.reset = base.create_param("RESET", ParamType::Int32)?;
        self.refused = base.create_param("REFUSED", ParamType::Int32)?;
        self.gain = base.create_param("GAIN", ParamType::Float64)?;
        Ok(())
    }

    fn write_int32(
        &mut self,
        base: &mut PortDriverBase,
        user: AxisUser<'_>,
        _value: i32,
    ) -> ParamWrite {
        let found = has_motor_axis(base, user);
        self.seen
            .lock()
            .unwrap()
            .push((user.reason, user.addr, found));
        if user.reason == self.reset {
            return ParamWrite::Handled(Ok(()));
        }
        if user.reason == self.refused {
            return ParamWrite::PassToBase(Err(AsynError::Status {
                status: AsynStatus::Timeout,
                message: "driver's own".into(),
            }));
        }
        ParamWrite::PassToBase(Ok(()))
    }

    fn write_float64(
        &mut self,
        base: &mut PortDriverBase,
        user: AxisUser<'_>,
        _value: f64,
    ) -> ParamWrite {
        let found = has_motor_axis(base, user);
        self.seen
            .lock()
            .unwrap()
            .push((user.reason, user.addr, found));
        if user.reason == self.gain {
            return ParamWrite::Handled(Ok(()));
        }
        ParamWrite::PassToBase(Ok(()))
    }
}

#[test]
fn a_write_at_an_address_with_no_axis_runs_the_driver_s_code_first() {
    let params = ControllerWrites::default();
    let seen = params.seen.clone();
    let port = register_controller_port(&open_owner(), "CPNOAXIS", 2, params).unwrap();
    let _ax = axis("CPNOAXIS", 0);
    let (reset, refused, gain) = (
        BASE_PARAMS.len(),
        BASE_PARAMS.len() + 1,
        BASE_PARAMS.len() + 2,
    );
    port.write_int32_blocking(reset, 0, 1).unwrap();
    port.write_int32_blocking(reset, 1, 1).unwrap();
    assert!(
        port.read_int32_blocking(reset, 1).is_err(),
        "the base stored nothing"
    );
    port.write_float64_blocking(gain, 1, 2.5).unwrap();
    let e = port.write_int32_blocking(refused, 1, 1).unwrap_err();
    assert_eq!(
        e.status(),
        AsynStatus::Timeout,
        "the driver's own status first"
    );
    let e = port
        .write_int32_blocking(index::DEFER_MOVES, 1, 1)
        .unwrap_err();
    assert_eq!(e.status(), AsynStatus::Error, "the base's no-axis refusal");
    assert!(port.write_float64_blocking(index::ACCEL, 1, 1.0).is_err());
    assert_eq!(
        *seen.lock().unwrap(),
        [
            (reset, 0, true),
            (reset, 1, false),
            (gain, 1, false),
            (refused, 1, false),
            (index::DEFER_MOVES, 1, false),
            (index::ACCEL, 1, false)
        ]
    );
}

/// C never runs a subclass's code at an out-of-range address: its connect
/// refuses the device with getAddress's line (asynPortDriver.cpp:3772) and
/// asynManager queues nothing for it (asynManager.c:1570-1577). The write
/// is refused with that one line.
#[test]
fn an_out_of_range_write_prints_getaddress_once_and_runs_no_driver_code() {
    use asyn_rs::manager::PortManager;
    use asyn_rs::trace::TraceFile;
    let params = ControllerWrites::default();
    let seen = params.seen.clone();
    let port = register_controller_port(&open_owner(), "CPRANGE", 2, params).unwrap();
    let _ax = axis("CPRANGE", 0);
    let path = std::env::temp_dir().join(format!("cp-range-{}", std::process::id()));
    let file = Arc::new(Mutex::new(std::fs::File::create(&path).unwrap()));
    let manager = PortManager::global();
    let trace = manager.trace_manager();
    trace.set_trace_file(Some("CPRANGE"), TraceFile::File(file.clone()));
    trace.set_device_trace_file("CPRANGE", 5, TraceFile::File(file));
    let lines = || {
        std::fs::read_to_string(&path)
            .unwrap()
            .matches("getAddress: CPRANGE invalid address=5")
            .count()
    };
    let reset = BASE_PARAMS.len();
    assert!(port.write_int32_blocking(reset, 5, 1).is_err());
    assert_eq!(lines(), 1);
    assert!(port.write_int32_blocking(index::DEFER_MOVES, 5, 1).is_err());
    assert_eq!(lines(), 2);
    assert!(seen.lock().unwrap().is_empty());
    let _ = std::fs::remove_file(&path);
}

/// Records the address every subclass entry is handed.
#[derive(Default)]
struct Addrs {
    seen: Arc<Mutex<Vec<i32>>>,
}

impl Addrs {
    fn saw(&self, addr: i32) {
        self.seen.lock().unwrap().push(addr);
    }
}

impl ControllerParams for Addrs {
    fn write_int32(&mut self, _: &mut PortDriverBase, user: AxisUser<'_>, _: i32) -> ParamWrite {
        self.saw(user.addr);
        ParamWrite::Handled(Ok(()))
    }
    fn write_float64(&mut self, _: &mut PortDriverBase, user: AxisUser<'_>, _: f64) -> ParamWrite {
        self.saw(user.addr);
        ParamWrite::Handled(Ok(()))
    }
    fn read_int32(&mut self, _: &mut PortDriverBase, user: AxisUser<'_>) -> AsynResult<i32> {
        self.saw(user.addr);
        Ok(0)
    }
    fn read_float64(&mut self, _: &mut PortDriverBase, user: AxisUser<'_>) -> AsynResult<f64> {
        self.saw(user.addr);
        Ok(0.0)
    }
    fn write_octet(&mut self, _: &mut PortDriverBase, user: AxisUser<'_>, _: &[u8]) -> ParamWrite {
        self.saw(user.addr);
        ParamWrite::Handled(Ok(()))
    }
    fn write_uint32_digital(
        &mut self,
        _: &mut PortDriverBase,
        user: AxisUser<'_>,
        _: u32,
        _: u32,
    ) -> ParamWrite {
        self.saw(user.addr);
        ParamWrite::Handled(Ok(()))
    }
    fn read_option(
        &self,
        _: &PortDriverBase,
        user: AxisUser<'_>,
        _: &str,
    ) -> Option<AsynResult<String>> {
        self.saw(user.addr);
        Some(Ok(String::new()))
    }
    fn write_option(
        &mut self,
        _: &mut PortDriverBase,
        user: AxisUser<'_>,
        _: &str,
        _: &str,
    ) -> Option<AsynResult<()>> {
        self.saw(user.addr);
        Some(Ok(()))
    }
}

/// A user at address -1 reaches the subclass at the axis C's getAxis
/// finds there, getAddress's 0 (asynPortDriver.cpp:1901-1914).
#[test]
fn every_subclass_entry_is_handed_getaddress_s_axis() {
    let params = Addrs::default();
    let seen = params.seen.clone();
    let port = register_controller_port(&open_owner(), "CPADDRM1", 2, params).unwrap();
    let _ax = axis("CPADDRM1", 0);
    port.write_int32_blocking(index::STOP, -1, 1).unwrap();
    port.write_float64_blocking(index::VELOCITY, -1, 1.0)
        .unwrap();
    port.read_int32_blocking(index::STOP, -1).unwrap();
    port.read_float64_blocking(index::VELOCITY, -1).unwrap();
    let io = asyn_rs::sync_io::SyncIOHandle::from_handle(port.clone(), -1, Duration::from_secs(1));
    io.write_octet(index::PROFILE_BUILD_MESSAGE, b"x").unwrap();
    io.write_uint32_digital(index::STOP, 1, 1).unwrap();
    let user = || AsynUser::default().with_addr(-1);
    port.get_option_blocking(user(), "k").unwrap();
    port.set_option_blocking(user(), "k", "v").unwrap();
    assert_eq!(*seen.lock().unwrap(), [0; 8]);
}

/// Records the closed-loop calls, moves and polls.
struct Servoed(Arc<Mutex<Vec<String>>>);

impl asyn_rs::interfaces::motor::AsynMotor for Servoed {
    fn move_absolute(&mut self, _: &AsynUser, p: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        self.0.lock().unwrap().push(format!("move {p}"));
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
    fn set_closed_loop(&mut self, _: &AsynUser, enable: bool) -> AsynResult<()> {
        self.0.lock().unwrap().push(format!("servo {enable}"));
        Ok(())
    }
    fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
        asyn_rs::interfaces::motor::PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser) -> AsynResult<asyn_rs::interfaces::motor::MotorStatus> {
        self.0.lock().unwrap().push("poll".into());
        Ok(asyn_rs::interfaces::motor::MotorStatus::default())
    }
}

/// pi-gcs2's PI_SUP_SERVO shape: a parameter of the driver's whose write
/// switches the axis's closed loop, after the port has been busy long
/// enough for a record move to wait on the port.
#[derive(Default)]
struct Servo {
    reason: usize,
}

impl ControllerParams for Servo {
    fn create_params(&mut self, base: &mut PortDriverBase) -> AsynResult<()> {
        self.reason = base.create_param("SERVO", ParamType::Int32)?;
        Ok(())
    }

    fn write_int32(
        &mut self,
        base: &mut PortDriverBase,
        user: AxisUser<'_>,
        value: i32,
    ) -> ParamWrite {
        if user.reason != self.reason {
            return ParamWrite::PassToBase(Ok(()));
        }
        std::thread::sleep(Duration::from_millis(200));
        ParamWrite::Handled(user.run(base, MotorCommand::SetClosedLoop { enable: value != 0 }))
    }
}

/// A driver's write runs on the port thread with the axis locked, as C's
/// does under the controller lock, and a record move the poller runs
/// meanwhile reads its speeds from the port without holding the axis.
#[test]
fn a_driver_write_overlapping_a_record_move_does_not_deadlock() {
    use motor_rs::poll_loop::{MotorPollLoop, PollerInbox};
    use motor_rs::trace::MotorTrace;
    let name = "CPQUEUE";
    let port = register_controller_port(&open_owner(), name, 1, Servo::default()).unwrap();
    let reason = BASE_PARAMS.len();
    let (poll_tx, poll) = mpsc::channel(4);
    let (motor_tx, commands) = mpsc::unbounded_channel();
    let user = TraceUser::Asyn {
        port: name.into(),
        addr: 0,
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let motor: Arc<Mutex<dyn asyn_rs::interfaces::motor::AsynMotor>> =
        Arc::new(Mutex::new(Servoed(seen.clone())));
    register_axis(MotorAxis::new(
        user.clone(),
        motor.clone(),
        Duration::from_secs(1),
        poll_tx,
        motor_tx.clone(),
        AxisSubscribers::new(),
    ))
    .unwrap();
    let mut poll_loop = MotorPollLoop::new(
        PollerInbox { poll, commands },
        motor,
        AxisSubscribers::new(),
        Duration::from_secs(1),
        Duration::from_secs(1),
        0,
    );
    seen.lock().unwrap().clear();
    let write = {
        let port = port.clone();
        std::thread::spawn(move || port.write_int32_blocking(reason, 0, 1))
    };
    std::thread::sleep(Duration::from_millis(50));
    motor_tx
        .send(motor_rs::poll_loop::ToAxis::from(CommandBatch {
            trace: Arc::new(MotorTrace::with_user(user)),
            record: Some(0),
            commands: vec![
                MotorCommand::SetVelocity { velocity: 1.0 },
                MotorCommand::MoveAbsolute {
                    position: 5.0,
                    min_velocity: 0.0,
                    velocity: 1.0,
                    acceleration: 1.0,
                },
            ],
        }))
        .unwrap();
    let poller = std::thread::spawn(move || {
        poll_loop.run_queued_commands();
        poll_loop
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !(write.is_finished() && poller.is_finished()) {
        assert!(
            std::time::Instant::now() < deadline,
            "the write and the move deadlocked"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    write.join().unwrap().unwrap();
    poller.join().unwrap();
    assert_eq!(*seen.lock().unwrap(), ["servo true", "move 5"]);
}
