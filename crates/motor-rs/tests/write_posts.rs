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
use motor_rs::device_state::{AxisSubscribers, SharedDeviceState, new_shared_state};
use motor_rs::flags::MotorCommand;
use motor_rs::poll_loop::{CommandBatch, MotorPollLoop, PollerInbox};
use motor_rs::trace::{MotorTrace, TraceUser};
use tokio::sync::mpsc;

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
    fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
        self.outcome()
    }
    fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
        Ok(())
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
    fn poll(&mut self, _: &AsynUser) -> AsynResult<MotorStatus> {
        Ok(MotorStatus {
            position: 1.0,
            encoder_position: 1.0,
            done: true,
            ..MotorStatus::default()
        })
    }
    fn take_status_writes(&mut self) -> Vec<StatusWrite> {
        std::mem::take(&mut self.pending)
    }
}

struct Rig {
    poll_loop: MotorPollLoop,
    commands: mpsc::UnboundedSender<CommandBatch>,
    trace: Arc<MotorTrace>,
    /// The record whose commands run.
    record: Bound,
    /// A second record on the axis: it sees every post and nothing else.
    observer: Bound,
    fail: Arc<AtomicBool>,
}

struct Bound {
    io_intr: mpsc::Receiver<()>,
    state: SharedDeviceState,
}

impl Bound {
    /// The statuses the processes queued so far read, one per pulse.
    fn passes(&mut self) -> Vec<MotorStatus> {
        let mut read = Vec::new();
        while self.io_intr.try_recv().is_ok() {
            let mut ds = self.state.lock().unwrap();
            let seq = ds.latest_status.as_ref().unwrap().seq;
            ds.begin_callback_pass();
            let latest = ds.latest_status.clone().unwrap();
            assert_ne!(latest.seq, seq, "every pulse reads a status");
            read.push(latest.status);
        }
        read
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
    let bind = || {
        let (io_intr_tx, io_intr) = mpsc::channel(8);
        let state = new_shared_state();
        state.lock().unwrap().accept();
        subscribers.attach(state.clone(), io_intr_tx, Default::default());
        Bound { io_intr, state }
    };
    let (record, observer) = (bind(), bind());
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
    poll_loop.prime_cache();
    Rig {
        poll_loop,
        commands,
        trace: Arc::new(MotorTrace::with_user(user)),
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
            .send(CommandBatch {
                trace: self.trace.clone(),
                record: Some(0),
                commands: vec![command],
            })
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
    let ratio = MotorCommand::SetEncoderRatio { ratio: 2.0 };
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
    let ratio = MotorCommand::SetEncoderRatio { ratio: 2.0 };
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

/// A model-1 record's move-done pass must not read the done status of the
/// poll before the move: the record would end the move there and retry it,
/// or leave a jog running that JOGF 0 then does not stop.
#[test]
fn a_model1_move_or_jog_processes_its_record_with_done_off() {
    let mut rig = rig(model1(), 1.0);
    for command in [
        move_to(5.0),
        MotorCommand::MoveVelocity {
            min_velocity: 0.1,
            velocity: 1.0,
            acceleration: 1.0,
        },
    ] {
        rig.write(command);
        let read = rig.record.passes();
        assert_eq!(read.len(), 1);
        assert!(!read[0].done);
    }
}

#[test]
fn a_failed_model1_move_still_posts_done_off() {
    let mut rig = rig(model1(), 1.0);
    rig.fail.store(true, Ordering::Relaxed);
    assert!(!rig.write(move_to(5.0)).unwrap().done);
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
    let posted = rig.write(MotorCommand::SetEncoderRatio { ratio: 2.0 });
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
