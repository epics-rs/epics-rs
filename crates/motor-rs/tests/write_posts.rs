//! The status a controller write posts at once, as C's
//! `asynMotorAxis::callParamCallbacks` ends every `asynMotorController`
//! write: only when the write changed the status (`statusChanged_`); and the
//! process devMotorAsyn's `asynCallback` runs when a move finishes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PidGainKind, StatusWrite};
use asyn_rs::user::AsynUser;
use motor_rs::device_state::{AxisSubscribers, MotorDset, SharedDeviceState, new_shared_state};
use motor_rs::flags::MotorCommand;
use motor_rs::poll_loop::{CommandBatch, MotorPollLoop, PollerInbox};
use motor_rs::trace::{MotorTrace, TraceUser};
use tokio::sync::mpsc;

mod passes;

/// A done axis at 1.0 whose encoder-ratio write sets the encoder position
/// to `encoder`, as phytron's `setEncoderRatio` does, and whose move sets
/// the direction, as drvEnsembleAsyn's does. With `fail` set every move and
/// set-position write fails.
struct Axis {
    encoder: f64,
    fail: Arc<AtomicBool>,
    pending: Vec<StatusWrite>,
}

impl Axis {
    fn outcome(&self) -> AsynResult<()> {
        if self.fail.load(Ordering::Relaxed) {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "refused".into(),
            });
        }
        Ok(())
    }
}

impl AsynMotor for Axis {
    fn move_absolute(
        &mut self,
        _: &AsynUser,
        position: f64,
        _: f64,
        _: f64,
        _: f64,
    ) -> AsynResult<()> {
        self.outcome()?;
        self.pending.push(StatusWrite::Direction(position > 1.0));
        Ok(())
    }
    fn move_relative(
        &mut self,
        _: &AsynUser,
        _distance: f64,
        _: f64,
        _: f64,
        _: f64,
    ) -> AsynResult<()> {
        Ok(())
    }
    fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
        self.outcome()
    }
    /// A failed stop raises PROBLEM and COMMS_ERROR, as a C caller of a
    /// failed `writeController` does (smarActMCSMotorDriver.cpp:622-627).
    fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        if self.fail.load(Ordering::Relaxed) {
            self.pending.push(StatusWrite::Problem(true));
            self.pending.push(StatusWrite::CommsError(true));
        }
        self.outcome()
    }
    fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        self.outcome()
    }
    fn set_encoder_ratio(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        self.pending
            .push(StatusWrite::EncoderPosition(self.encoder));
        Ok(())
    }
    fn set_pid_gain(&mut self, _: &AsynUser, _: PidGainKind, _: f64) -> AsynResult<()> {
        Ok(())
    }
    fn set_high_limit(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        self.outcome()
    }
    fn poll_failure(&self) -> asyn_rs::interfaces::motor::PollFailure {
        asyn_rs::interfaces::motor::PollFailure::PROBLEM_AND_COMMS_ERROR
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        *status = MotorStatus {
            position: 1.0,
            encoder_position: 1.0,
            done: true,
            ..MotorStatus::default()
        };
        Ok(())
    }
    fn take_status_writes(&mut self) -> Vec<StatusWrite> {
        std::mem::take(&mut self.pending)
    }
}

struct Rig {
    poll_loop: MotorPollLoop,
    commands: mpsc::UnboundedSender<motor_rs::poll_loop::ToAxis>,
    trace: Arc<MotorTrace>,
    /// The record whose commands run.
    record: Bound,
    /// A second record on the axis: it sees every post and nothing else.
    observer: Bound,
    fail: Arc<AtomicBool>,
}

struct Bound {
    reads: passes::Reads,
    state: SharedDeviceState,
}

impl Bound {
    /// The statuses the processes so far read.
    fn passes(&mut self) -> Vec<MotorStatus> {
        (self.reads.take().into_iter())
            .map(|status| status.expect("every process reads a status"))
            .collect()
    }
}

fn rig(user: TraceUser, encoder: f64) -> Rig {
    let (_poll_tx, poll) = mpsc::channel(4);
    let (commands, commands_rx) = mpsc::unbounded_channel();
    let fail = Arc::new(AtomicBool::new(false));
    let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(Axis {
        encoder,
        fail: fail.clone(),
        pending: Vec::new(),
    }));
    let subscribers = AxisSubscribers::new();
    let trace = Arc::new(MotorTrace::with_user(user));
    let bind = || {
        let (io_intr_tx, _) = mpsc::channel(8);
        let state = new_shared_state();
        let own = Arc::<MotorTrace>::default();
        let reads = passes::bind(&state, &own);
        // As MotorAxis::attach binds a model-1 record.
        if trace.is_model1() {
            state.lock().unwrap().set_dset(MotorDset::MotordevCom);
        }
        subscribers.attach(state.clone(), io_intr_tx, own);
        Bound { reads, state }
    };
    let mut poll_loop = MotorPollLoop::new(
        PollerInbox {
            poll,
            commands: commands_rx,
        },
        motor,
        subscribers.clone(),
        Duration::from_secs(1),
        Duration::from_secs(1),
        0,
    );
    // The records are bound after the axis's constructor and before the
    // poller starts, so its startup pass posts to them.
    let (mut record, mut observer) = (bind(), bind());
    poll_loop.run_startup_pass();
    record.passes();
    observer.passes();
    Rig {
        poll_loop,
        commands,
        trace,
        record,
        observer,
        fail,
    }
}

fn controller() -> TraceUser {
    TraceUser::Asyn {
        port: "writePosts".into(),
        addr: 0,
    }
}

fn model2() -> TraceUser {
    TraceUser::Model2 {
        port: "writePostsModel2".into(),
        addr: 0,
    }
}

impl Rig {
    /// Run `command` for record 0, counted pending as `build_trans` counts
    /// it; the status it posted, if it posted one.
    fn write(&mut self, command: MotorCommand) -> Option<MotorStatus> {
        if command.is_move() {
            self.record.state.lock().unwrap().move_queued();
        }
        self.commands
            .send(motor_rs::poll_loop::ToAxis::from(CommandBatch {
                trace: self.trace.clone(),
                record: Some(0),
                commands: vec![command],
            }))
            .unwrap();
        self.poll_loop.run_queued_commands();
        let mut posts = self.observer.passes();
        assert!(posts.len() <= 1, "one write posts at most once");
        posts.pop()
    }
}

fn move_to(position: f64) -> MotorCommand {
    MotorCommand::MoveAbsolute {
        position,
        min_velocity: 0.1,
        velocity: 1.0,
        acceleration: 1.0,
    }
}

#[test]
fn a_set_position_posts_the_new_position_at_once() {
    let mut rig = rig(controller(), 1.0);
    let posted = rig
        .write(MotorCommand::SetPosition { position: 4.0 })
        .unwrap();
    assert_eq!(posted.position, 4.0);
    assert!(posted.done);
}

#[test]
fn a_set_position_to_the_posted_position_posts_nothing() {
    let mut rig = rig(controller(), 1.0);
    assert_eq!(rig.write(MotorCommand::SetPosition { position: 1.0 }), None);
}

#[test]
fn a_move_on_a_done_axis_posts_done_off_and_a_second_move_posts_nothing() {
    let mut rig = rig(controller(), 1.0);
    let posted = rig.write(move_to(5.0)).unwrap();
    assert!(!posted.done);
    assert_eq!(posted.position, 1.0);
    assert_eq!(rig.write(move_to(6.0)), None);
}

/// Every start clears Done ahead of the driver, not only the absolute
/// move (asynMotorController.cpp:301-353).
#[test]
fn each_move_start_on_a_done_axis_posts_done_off() {
    let starts = [
        MotorCommand::MoveRelative {
            distance: 1.0,
            min_velocity: 0.1,
            velocity: 1.0,
            acceleration: 1.0,
        },
        MotorCommand::MoveVelocity {
            min_velocity: 0.1,
            velocity: 1.0,
            acceleration: 1.0,
        },
        MotorCommand::JogVelocity {
            min_velocity: 0.1,
            velocity: 1.0,
            acceleration: 1.0,
        },
        MotorCommand::Home {
            forward: true,
            min_velocity: 0.1,
            velocity: 1.0,
            acceleration: 1.0,
        },
    ];
    for start in starts {
        let mut rig = rig(controller(), 1.0);
        let posted = rig.write(start.clone());
        assert!(!posted.unwrap().done, "{start:?}");
    }
}

#[test]
fn a_write_that_sets_no_status_field_posts_nothing() {
    let mut rig = rig(controller(), 1.0);
    let gain = MotorCommand::SetPidGain {
        kind: PidGainKind::Proportional,
        gain: 0.5,
    };
    assert_eq!(rig.write(gain), None);
    assert_eq!(rig.write(MotorCommand::Stop { acceleration: 1.0 }), None);
}

#[test]
fn a_field_the_driver_set_posts_only_when_it_changed() {
    let mut rig = rig(controller(), 2.0);
    let ratio = MotorCommand::SetEncoderRatio { pulses: [2.0, 1.0] };
    assert_eq!(rig.write(ratio.clone()).unwrap().encoder_position, 2.0);
    assert_eq!(rig.write(ratio), None);
}

#[test]
fn a_controller_write_that_fails_still_posts_what_the_controller_set() {
    let mut rig = rig(controller(), 1.0);
    rig.fail.store(true, Ordering::Relaxed);
    assert!(!rig.write(move_to(5.0)).unwrap().done);
    let posted = rig
        .write(MotorCommand::SetPosition { position: 4.0 })
        .unwrap();
    assert_eq!(posted.position, 4.0);
}

#[test]
fn a_model2_move_posts_its_direction_and_done_off_once() {
    let mut rig = rig(model2(), 1.0);
    let posted = rig.write(move_to(5.0)).unwrap();
    assert!(!posted.done);
    assert!(posted.direction);
    assert_eq!(posted.position, 1.0);
    assert_eq!(rig.write(move_to(6.0)), None);
    assert!(!rig.write(move_to(0.0)).unwrap().direction);
}

#[test]
fn a_model2_home_posts_done_off() {
    let mut rig = rig(model2(), 1.0);
    let home = MotorCommand::Home {
        forward: true,
        min_velocity: 0.1,
        velocity: 1.0,
        acceleration: 1.0,
    };
    assert!(!rig.write(home).unwrap().done);
}

#[test]
fn a_model2_set_position_posts_the_new_position() {
    let mut rig = rig(model2(), 1.0);
    let posted = rig
        .write(MotorCommand::SetPosition { position: 4.0 })
        .unwrap();
    assert_eq!(posted.position, 4.0);
    assert!(posted.done);
    assert_eq!(rig.write(MotorCommand::SetPosition { position: 4.0 }), None);
}

#[test]
fn a_model2_write_that_fails_posts_nothing() {
    let mut rig = rig(model2(), 1.0);
    rig.fail.store(true, Ordering::Relaxed);
    assert_eq!(rig.write(move_to(5.0)), None);
    assert_eq!(rig.write(MotorCommand::SetPosition { position: 4.0 }), None);
}

#[test]
fn a_model2_driver_field_posts_like_a_controllers() {
    let mut rig = rig(model2(), 2.0);
    let ratio = MotorCommand::SetEncoderRatio { pulses: [2.0, 1.0] };
    assert_eq!(rig.write(ratio.clone()).unwrap().encoder_position, 2.0);
    assert_eq!(rig.write(ratio), None);
}

fn model1() -> TraceUser {
    TraceUser::Model1 {
        device: "devWritePosts",
        card: 0,
        signal: 0,
    }
}

/// motordrvCom sends a MOTION or VELOCITY message with no callback: the
/// record hears of the move only from the held move's next query.
#[test]
fn a_model1_move_or_jog_posts_nothing() {
    let mut rig = rig(model1(), 1.0);
    for command in [
        move_to(5.0),
        MotorCommand::MoveVelocity {
            min_velocity: 0.1,
            velocity: 1.0,
            acceleration: 1.0,
        },
    ] {
        assert_eq!(rig.write(command), None);
        assert!(rig.record.passes().is_empty());
        assert!(rig.observer.passes().is_empty());
    }
}

#[test]
fn a_failed_model1_move_posts_nothing() {
    let mut rig = rig(model1(), 1.0);
    rig.fail.store(true, Ordering::Relaxed);
    assert_eq!(rig.write(move_to(5.0)), None);
    assert!(rig.record.passes().is_empty());
}

#[test]
fn a_finished_move_processes_its_record_once_with_the_status_it_posted() {
    let mut rig = rig(controller(), 1.0);
    let posted = rig.write(move_to(5.0)).unwrap();
    assert_eq!(rig.record.passes(), vec![posted]);
}

#[test]
fn a_finished_move_that_posted_nothing_processes_with_the_last_post() {
    let mut rig = rig(controller(), 1.0);
    let posted = rig.write(move_to(5.0)).unwrap();
    rig.record.passes();
    assert_eq!(rig.write(move_to(6.0)), None);
    assert_eq!(rig.record.passes(), vec![posted]);
}

#[test]
fn a_write_that_is_no_move_processes_its_record_only_through_a_post() {
    let mut rig = rig(controller(), 2.0);
    assert_eq!(rig.write(MotorCommand::Stop { acceleration: 1.0 }), None);
    assert!(rig.record.passes().is_empty());
    let posted = rig.write(MotorCommand::SetEncoderRatio { pulses: [2.0, 1.0] });
    assert_eq!(rig.record.passes(), Vec::from_iter(posted));
}

#[test]
fn a_failed_model2_move_still_processes_its_record() {
    let mut rig = rig(model2(), 1.0);
    rig.fail.store(true, Ordering::Relaxed);
    assert_eq!(rig.write(move_to(5.0)), None);
    assert_eq!(rig.record.passes().len(), 1);
    assert_eq!(rig.write(MotorCommand::SetPosition { position: 4.0 }), None);
    assert_eq!(rig.record.passes().len(), 1);
}

#[test]
fn a_set_position_processes_its_record_with_the_new_position() {
    let mut rig = rig(model2(), 1.0);
    rig.write(MotorCommand::SetPosition { position: 4.0 });
    let read = rig.record.passes();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].position, 4.0);
}

fn high_limit(position: f64) -> MotorCommand {
    MotorCommand::SetHighLimit { position }
}

#[test]
fn a_model2_parameter_write_posts_to_every_record_only_when_it_changed() {
    let mut rig = rig(model2(), 1.0);
    let posted = rig.write(high_limit(5.0)).unwrap();
    assert_eq!(rig.record.passes(), vec![posted.clone()]);
    assert_eq!(rig.write(high_limit(5.0)), None);
    assert!(rig.record.passes().is_empty());
    assert_eq!(rig.write(high_limit(6.0)), Some(posted));
}

#[test]
fn each_model2_parameter_keeps_its_own_value() {
    let mut rig = rig(model2(), 1.0);
    let gain = |gain| MotorCommand::SetPidGain {
        kind: PidGainKind::Proportional,
        gain,
    };
    assert!(rig.write(high_limit(5.0)).is_some());
    assert!(
        rig.write(MotorCommand::SetLowLimit { position: 5.0 })
            .is_some()
    );
    assert!(rig.write(gain(5.0)).is_some());
    assert!(
        rig.write(MotorCommand::SetClosedLoop { enable: true })
            .is_some()
    );
    assert_eq!(rig.write(gain(5.0)), None);
}

#[test]
fn a_model2_parameter_write_that_fails_stores_and_posts_nothing() {
    let mut rig = rig(model2(), 1.0);
    rig.fail.store(true, Ordering::Relaxed);
    assert_eq!(rig.write(high_limit(5.0)), None);
    rig.fail.store(false, Ordering::Relaxed);
    assert!(rig.write(high_limit(5.0)).is_some());
}

#[test]
fn a_model2_stop_stores_no_parameter() {
    let mut rig = rig(model2(), 1.0);
    assert_eq!(rig.write(MotorCommand::Stop { acceleration: 1.0 }), None);
}

#[test]
fn a_controller_parameter_write_posts_nothing() {
    let mut rig = rig(controller(), 1.0);
    assert_eq!(rig.write(high_limit(5.0)), None);
    assert!(rig.record.passes().is_empty());
}

#[test]
fn a_controller_write_that_fails_on_the_wire_posts_the_comms_error() {
    let mut rig = rig(controller(), 1.0);
    rig.fail.store(true, Ordering::Relaxed);
    let stop = MotorCommand::Stop { acceleration: 1.0 };
    let posted = rig.write(stop.clone()).unwrap();
    assert!(posted.comms_error && posted.problem);
    assert_eq!(rig.write(stop), None);
}
