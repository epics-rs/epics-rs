//! Each poll of an asynMotorController axis sets its status values in the
//! controller port's parameters at the axis address, as C's driver poll does
//! through `asynMotorAxis::setDoubleParam` / `setIntegerParam` and
//! `callParamCallbacks`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure, PortSets, StatusWrite};
use asyn_rs::interrupt::{InterruptFilter, InterruptReceiver, InterruptSubscription};
use asyn_rs::param::ParamValue;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port_handle::PortHandle;
use asyn_rs::registry::open_owner;
use asyn_rs::user::AsynUser;
use motor_rs::builder::{AxisSetup, MotorBuilder};
use motor_rs::controller_port::{
    AxisUser, ControllerParams, FinishedWrite, ParamDelta, ParamWrite, WriteHook, WriteOutcome,
    index,
};
use motor_rs::device_state::AxisSubscribers;
use motor_rs::device_support::MotorAxis;
use motor_rs::devices::{register_axis, register_controller_port};
use motor_rs::poll_loop::MotorPollLoop;
use motor_rs::trace::TraceUser;
use tokio::sync::mpsc;

struct Axis(Arc<Mutex<MotorStatus>>);

impl AsynMotor for Axis {
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
        *status = self.0.lock().unwrap().clone();
        Ok(())
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
        done: true,
        high_limit: true,
        powered: true,
        homed: true,
        ..MotorStatus::default()
            .with_position(3.5)
            .with_encoder_position(7.0)
            .with_velocity(1.25)
    }));
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(Axis(status.clone())));
    // The startup poll lands in the port once the axis mirrors it.
    let build = || {
        let mut setup = MotorBuilder::new(motor.clone())
            .trace_user(TraceUser::Asyn {
                port: "pollParams".into(),
                addr: 1,
            })
            .build_axis();
        setup.poll_loop.run_startup_pass();
        setup
    };
    let setup = build();
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
        moving: true,
        msta_moving: true,
        done: false,
        low_limit: true,
        ..MotorStatus::default().with_position(-2.0)
    };
    // One controller's axis at a time: the address is the first's until it
    // drops.
    drop(setup);
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
        *self.0.lock().unwrap() += 1;
        *status = MotorStatus::default();
        Ok(())
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
    poll_loop.run_startup_pass();
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

/// Polls as scripted, failing once the script runs out. A `None` fails
/// before writing anything, as XPS's poll bails before its setDoubleParam
/// (XPSAxis.cpp:575, :610).
struct Scripted(VecDeque<Option<MotorStatus>>);

impl AsynMotor for Scripted {
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
        let Some(polled) = self.0.pop_front().flatten() else {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "failed".into(),
            });
        };
        *status = polled;
        Ok(())
    }
}

const READBACKS: [usize; 3] = [
    index::POSITION,
    index::ENCODER_POSITION,
    index::ACT_VELOCITY,
];

/// Axis 1 of a new controller port `name`, polled from `script`, before its
/// startup poll.
fn scripted<const N: usize>(
    name: &str,
    script: [Option<MotorStatus>; N],
) -> (PortHandle, AxisSetup) {
    let port = register_controller_port(&open_owner(), name, 2, ()).unwrap();
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(Scripted(script.into())));
    let setup = MotorBuilder::new(motor)
        .trace_user(TraceUser::Asyn {
            port: name.into(),
            addr: 1,
        })
        .build_axis();
    (port, setup)
}

/// A subscription to each readback of axis 1.
fn subscribe(port: &PortHandle) -> Vec<(InterruptSubscription, InterruptReceiver)> {
    READBACKS
        .map(|reason| {
            let filter = InterruptFilter {
                reason: Some(reason),
                addr: Some(1),
                ..Default::default()
            };
            port.interrupts()
                .register_interrupt_user(filter, 16)
                .unwrap()
        })
        .into()
}

/// The value each subscription was posted since it was last taken. The
/// mailbox is written inside the port's callbacks, which a blocking read of
/// the port has waited out.
fn posted(
    port: &PortHandle,
    subs: &mut [(InterruptSubscription, InterruptReceiver)],
) -> Vec<Option<f64>> {
    port.read_int32_blocking(index::STATUS, 1).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    subs.iter_mut()
        .map(|(_, rx)| {
            let value = rt.block_on(async {
                tokio::time::timeout(Duration::from_millis(20), rx.recv()).await
            });
            value.ok().flatten().map(|iv| match iv.value {
                ParamValue::Float64(v) => v,
                other => panic!("{other:?}"),
            })
        })
        .collect()
}

/// C's paramLib holds the readbacks undefined until the driver's poll sets
/// them, which a failed poll bails before: an axis no poll returned on reads
/// asynParamUndefined.
#[test]
fn an_axis_never_polled_ok_leaves_its_readbacks_undefined() {
    let (port, mut setup) = scripted("readbackNever", []);
    setup.poll_loop.run_startup_pass();
    setup.poll_loop.run_startup_pass();
    for reason in READBACKS {
        let e = port.read_float64_blocking(reason, 1).unwrap_err();
        assert_eq!(e.c_status_code(), 10, "reason {reason}: {e}");
    }
    // The failures' own bits are set: PROBLEM and COMMS_ERROR.
    let word = port.read_int32_blocking(index::STATUS, 1).unwrap();
    assert_eq!(word & (1 << 9 | 1 << 12), 1 << 9 | 1 << 12);
}

/// A failed poll after one that returned leaves the readbacks it never set
/// as they were, so nothing posts them.
#[test]
fn failed_polls_keep_the_last_good_readbacks_without_a_callback() {
    let good = MotorStatus {
        done: true,
        ..MotorStatus::default()
            .with_position(3.5)
            .with_encoder_position(7.0)
            .with_velocity(1.25)
    };
    let (port, mut setup) = scripted("readbackKept", [Some(good)]);
    let mut subs = subscribe(&port);
    setup.poll_loop.run_startup_pass();
    assert_eq!(posted(&port, &mut subs), [Some(3.5), Some(7.0), Some(1.25)]);
    setup.poll_loop.run_startup_pass();
    setup.poll_loop.run_startup_pass();
    assert_eq!(posted(&port, &mut subs), [None, None, None]);
    let read = |reason| port.read_float64_blocking(reason, 1).unwrap();
    assert_eq!(READBACKS.map(read), [3.5, 7.0, 1.25]);
}

/// The first poll to return after failures defines the readbacks it sets
/// and posts them, even at the values a never-polled status holds.
#[test]
fn the_first_good_poll_after_failures_defines_and_posts_the_readbacks() {
    let (port, mut setup) = scripted(
        "readbackFirst",
        [
            None,
            Some(MotorStatus {
                done: true,
                ..MotorStatus::default()
                    .with_position(0.0)
                    .with_encoder_position(0.0)
                    .with_velocity(0.0)
            }),
        ],
    );
    let mut subs = subscribe(&port);
    setup.poll_loop.run_startup_pass();
    assert_eq!(posted(&port, &mut subs), [None; 3]);
    let read = |reason| port.read_float64_blocking(reason, 1).ok();
    assert_eq!(READBACKS.map(read), [None; 3]);
    setup.poll_loop.run_startup_pass();
    assert_eq!(posted(&port, &mut subs), [Some(0.0); 3]);
    assert_eq!(READBACKS.map(read), [Some(0.0); 3]);
}

/// One poll: the readbacks it sets, then whether it returns or bails.
type Poll = Box<dyn FnOnce(&mut MotorStatus) -> AsynResult<()> + Send + Sync>;

fn poll(poll: impl FnOnce(&mut MotorStatus) -> AsynResult<()> + Send + Sync + 'static) -> Poll {
    Box::new(poll)
}

fn failed() -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: "failed".into(),
    }
}

/// A poll that returns with every readback set.
fn good() -> Poll {
    poll(|status| {
        *status = MotorStatus {
            done: true,
            ..MotorStatus::default()
                .with_position(3.5)
                .with_encoder_position(7.0)
                .with_velocity(1.25)
        };
        Ok(())
    })
}

/// Polls as its polls do in turn, failing with nothing set once they run
/// out. A move to `p` writes the position `p`, as SmarAct MCS2's open-loop
/// move does, and also sets POSITION to `p + 0.5` on the axis when `moved`
/// is set; an encoder-ratio write of `r` writes the encoder position `r`,
/// as phytron's `setEncoderRatio` does.
struct Polls {
    polls: VecDeque<Poll>,
    writes: Vec<StatusWrite>,
    sets: PortSets,
    moved: bool,
    failure: PollFailure,
}

fn polls(polls: impl IntoIterator<Item = Poll>) -> Polls {
    Polls {
        polls: polls.into_iter().collect(),
        writes: Vec::new(),
        sets: PortSets::default(),
        moved: false,
        failure: PollFailure::PROBLEM_AND_COMMS_ERROR,
    }
}

impl AsynMotor for Polls {
    fn move_absolute(&mut self, _: &AsynUser, p: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        if self.moved {
            self.sets
                .axis(index::POSITION, ParamValue::Float64(p + 0.5));
        } else {
            self.writes.push(StatusWrite::Position(p));
        }
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
    fn set_encoder_ratio(&mut self, _: &AsynUser, r: f64) -> AsynResult<()> {
        self.writes.push(StatusWrite::EncoderPosition(r));
        Ok(())
    }
    fn poll_failure(&self) -> PollFailure {
        self.failure
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        match self.polls.pop_front() {
            Some(poll) => poll(status),
            None => Err(failed()),
        }
    }
    fn take_status_writes(&mut self) -> Vec<StatusWrite> {
        std::mem::take(&mut self.writes)
    }
    fn take_port_sets(&mut self) -> PortSets {
        std::mem::take(&mut self.sets)
    }
}

/// Axis 1 of a new controller port `name` on `motor`, which the port's
/// writes reach; its poller before the startup poll.
fn polled_by(name: &str, motor: Polls) -> (PortHandle, MotorPollLoop) {
    polled_on(name, (), motor)
}

/// [`polled_by`] on a port of `params`.
fn polled_on(
    name: &str,
    params: impl ControllerParams,
    motor: Polls,
) -> (PortHandle, MotorPollLoop) {
    let port = register_controller_port(&open_owner(), name, 2, params).unwrap();
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(motor));
    let setup = MotorBuilder::new(motor)
        .trace_user(TraceUser::Asyn {
            port: name.into(),
            addr: 1,
        })
        .build_axis();
    register_axis(setup.axis).unwrap();
    (port, setup.poll_loop)
}

/// Each readback of axis 1, or the C status a read of it returns.
fn readbacks(port: &PortHandle) -> [Result<f64, i32>; 3] {
    READBACKS.map(|reason| {
        port.read_float64_blocking(reason, 1)
            .map_err(|e| e.c_status_code())
    })
}

/// C's asynMotorAxis::setDoubleParam defines a readback at once, so a poll
/// bailing after it leaves it defined (XPSAxis.cpp:706, :716), and one it
/// never set undefined.
#[test]
fn a_failed_poll_defines_what_it_set_before_bailing() {
    let (port, mut poll_loop) = polled_by(
        "readbackSetFail",
        polls([poll(|status| {
            status.set_encoder_position(6.0);
            Err(failed())
        })]),
    );
    poll_loop.run_startup_pass();
    assert_eq!(readbacks(&port), [Err(10), Ok(6.0), Err(10)]);
}

/// A driver bailing before callParamCallbacks with no status bit posts
/// nothing, yet defines what it set, even at the value its status held.
#[test]
fn a_quiet_failed_poll_defines_what_it_set_unposted() {
    let (port, mut poll_loop) = polled_by(
        "readbackSetQuiet",
        Polls {
            failure: PollFailure::NONE,
            ..polls([poll(|status| {
                status.set_encoder_position(status.encoder_position());
                Err(failed())
            })])
        },
    );
    let mut subs = subscribe(&port);
    // Past the boot sweep, which posts whatever is defined then.
    posted(&port, &mut subs);
    poll_loop.run_startup_pass();
    assert_eq!(posted(&port, &mut subs), [None; 3]);
    assert_eq!(readbacks(&port), [Err(10), Ok(0.0), Err(10)]);
}

/// A returned poll defines only the readbacks it set: C's paramLib holds
/// the others undefined, as motorSim's never-set ACT_VELOCITY is.
#[test]
fn a_returned_poll_defines_only_the_readbacks_it_set() {
    let (port, mut poll_loop) = polled_by(
        "readbackSetSome",
        polls([poll(|status| {
            status.done = true;
            status.set_encoder_position(6.0);
            Ok(())
        })]),
    );
    let mut subs = subscribe(&port);
    poll_loop.run_startup_pass();
    assert_eq!(posted(&port, &mut subs), [None, Some(6.0), None]);
    assert_eq!(readbacks(&port), [Err(10), Ok(6.0), Err(10)]);
}

/// A failed poll updates the encoder position it set before bailing and no
/// readback it left unset, whatever value its status holds.
#[test]
fn a_failed_poll_updates_the_encoder_position_it_set() {
    let (port, mut poll_loop) = polled_by(
        "readbackFailEncoder",
        polls([
            good(),
            poll(|status| {
                *status = MotorStatus::default().with_encoder_position(8.0);
                Err(failed())
            }),
        ]),
    );
    let mut subs = subscribe(&port);
    poll_loop.run_startup_pass();
    posted(&port, &mut subs);
    poll_loop.run_startup_pass();
    assert_eq!(posted(&port, &mut subs), [None, Some(8.0), None]);
    assert_eq!(readbacks(&port), [Ok(3.5), Ok(8.0), Ok(1.25)]);
}

/// [`a_failed_poll_updates_the_encoder_position_it_set`] for the velocity.
#[test]
fn a_failed_poll_updates_the_velocity_it_set() {
    let (port, mut poll_loop) = polled_by(
        "readbackFailVelocity",
        polls([
            good(),
            poll(|status| {
                *status = MotorStatus::default().with_velocity(2.5);
                Err(failed())
            }),
        ]),
    );
    let mut subs = subscribe(&port);
    poll_loop.run_startup_pass();
    posted(&port, &mut subs);
    poll_loop.run_startup_pass();
    assert_eq!(posted(&port, &mut subs), [None, None, Some(2.5)]);
    assert_eq!(readbacks(&port), [Ok(3.5), Ok(7.0), Ok(2.5)]);
}

/// A driver's write of the position defines it and no other readback.
#[test]
fn a_drivers_position_write_defines_the_position() {
    let (port, mut poll_loop) = polled_by("readbackWritePosition", polls([]));
    poll_loop.run_startup_pass();
    port.write_float64_blocking(index::MOVE_ABS, 1, 4.0)
        .unwrap();
    poll_loop.run_queued_commands();
    assert_eq!(readbacks(&port), [Ok(4.0), Err(10), Err(10)]);
}

/// A driver's write of the encoder position defines it and no other
/// readback.
#[test]
fn a_drivers_encoder_position_write_defines_the_encoder_position() {
    let (port, mut poll_loop) = polled_by("readbackWriteEncoder", polls([]));
    poll_loop.run_startup_pass();
    port.write_float64_blocking(index::ENCODER_RATIO, 1, 2.5)
        .unwrap();
    poll_loop.run_queued_commands();
    assert_eq!(readbacks(&port), [Err(10), Ok(2.5), Err(10)]);
}

/// A record's write of ACT_VELOCITY sets the axis's velocity, as C's
/// `pAxis->setDoubleParam` sets `status_` (asynMotorController.cpp:289): a
/// poll setting the velocity it had before puts it back.
#[test]
fn a_poll_puts_back_the_velocity_a_record_wrote_over() {
    let (port, mut poll_loop) = polled_by("readbackWriteVelocity", polls([good(), good()]));
    poll_loop.run_startup_pass();
    port.write_float64_blocking(index::ACT_VELOCITY, 1, 9.0)
        .unwrap();
    poll_loop.run_queued_commands();
    assert_eq!(readbacks(&port), [Ok(3.5), Ok(7.0), Ok(9.0)]);
    poll_loop.run_startup_pass();
    assert_eq!(readbacks(&port), [Ok(3.5), Ok(7.0), Ok(1.25)]);
}

/// A driver's `pAxis->setDoubleParam(motorPosition_)` in a write sets the
/// axis's position, so a poll setting the one it had before puts it back.
#[test]
fn a_poll_puts_back_the_position_a_drivers_port_set_wrote_over() {
    let (port, mut poll_loop) = polled_by(
        "readbackPortSet",
        Polls {
            moved: true,
            ..polls([good(), good()])
        },
    );
    poll_loop.run_startup_pass();
    port.write_float64_blocking(index::MOVE_ABS, 1, 4.0)
        .unwrap();
    poll_loop.run_queued_commands();
    assert_eq!(readbacks(&port), [Ok(4.5), Ok(7.0), Ok(1.25)]);
    poll_loop.run_startup_pass();
    assert_eq!(readbacks(&port), [Ok(3.5), Ok(7.0), Ok(1.25)]);
}

/// A subclass write setting the position after the base's, as a C
/// `writeInt32` calling `setDoubleParam(addr, motorPosition_)` does.
struct SetsPosition;

impl WriteHook for SetsPosition {
    fn after_write(&self, write: &FinishedWrite<'_>) -> WriteOutcome {
        let mut delta = ParamDelta::default();
        if write.function == index::STOP {
            delta.set_float64(index::POSITION, 4.5);
        }
        WriteOutcome {
            delta,
            status: None,
        }
    }
}

impl ControllerParams for SetsPosition {
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::NONE;
    const INTERRUPT_MASK: InterfaceMask = InterfaceMask::NONE;

    fn write_hook(&self) -> Option<Arc<dyn WriteHook>> {
        Some(Arc::new(SetsPosition))
    }
}

/// [`a_poll_puts_back_the_position_a_drivers_port_set_wrote_over`] for a
/// write hook's set.
#[test]
fn a_poll_puts_back_the_position_a_write_hook_wrote_over() {
    let (port, mut poll_loop) = polled_on("readbackHook", SetsPosition, polls([good(), good()]));
    poll_loop.run_startup_pass();
    port.write_int32_blocking(index::STOP, 1, 1).unwrap();
    poll_loop.run_queued_commands();
    assert_eq!(readbacks(&port), [Ok(4.5), Ok(7.0), Ok(1.25)]);
    poll_loop.run_startup_pass();
    assert_eq!(readbacks(&port), [Ok(3.5), Ok(7.0), Ok(1.25)]);
}

/// A constructor's setDoubleParam defines a readback on the port before any
/// poll (ScriptMotorDriver.cpp:212), and a poll setting nothing keeps it.
#[test]
fn a_constructor_set_readback_is_defined() {
    let (port, mut poll_loop) = polled_by(
        "readbackCtorSet",
        Polls {
            writes: vec![StatusWrite::EncoderPosition(4.0)],
            ..polls([poll(|_| Err(failed()))])
        },
    );
    assert_eq!(readbacks(&port), [Err(10), Ok(4.0), Err(10)]);
    poll_loop.run_startup_pass();
    assert_eq!(readbacks(&port), [Err(10), Ok(4.0), Err(10)]);
}

/// A subclass write setting the position on the port's own parameter, as a
/// C `writeFloat64` calling `setDoubleParam(axis, motorPosition_)` on the
/// controller does, leaving the axis's `status_` alone.
struct BaseSetsPosition;

impl ControllerParams for BaseSetsPosition {
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::NONE;
    const INTERRUPT_MASK: InterfaceMask = InterfaceMask::NONE;

    fn write_float64(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
        user: AxisUser<'_>,
        _value: f64,
    ) -> ParamWrite {
        if user.reason != index::MOVE_REL {
            return ParamWrite::PassToBase(Ok(()));
        }
        base.set_float64_param(index::POSITION, user.addr, 4.5)
            .unwrap();
        base.call_param_callbacks(user.addr).unwrap();
        ParamWrite::Handled(Ok(()))
    }
}

/// The axis's setDoubleParam reaches `pC_->setDoubleParam` whatever its
/// `status_` holds, so a poll setting the position it had before puts back
/// a set on the port's own parameter.
#[test]
fn a_poll_puts_back_a_position_set_on_the_ports_own_parameter() {
    let (port, mut poll_loop) =
        polled_on("readbackBaseSet", BaseSetsPosition, polls([good(), good()]));
    poll_loop.run_startup_pass();
    port.write_float64_blocking(index::MOVE_REL, 1, 1.0)
        .unwrap();
    poll_loop.run_queued_commands();
    assert_eq!(readbacks(&port), [Ok(4.5), Ok(7.0), Ok(1.25)]);
    poll_loop.run_startup_pass();
    assert_eq!(readbacks(&port), [Ok(3.5), Ok(7.0), Ok(1.25)]);
}
