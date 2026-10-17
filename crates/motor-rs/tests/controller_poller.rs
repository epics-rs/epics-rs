//! One poller per controller (asynMotorController.cpp:615-695): each pass
//! runs the controller poll, then every axis in index order on the one
//! thread, so a slow axis delays the axes after it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure};
use asyn_rs::user::AsynUser;
use motor_rs::builder::{ControllerAxis, ControllerBuilder};
use motor_rs::poll_loop::PollCommand;
use motor_rs::trace::TraceUser;

type Log = Arc<Mutex<Vec<(&'static str, Instant)>>>;

const SLOW: Duration = Duration::from_millis(40);
const PERIOD: Duration = Duration::from_millis(5);

struct Axis {
    name: &'static str,
    log: Log,
    delay: Duration,
}

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
        self.log.lock().unwrap().push((self.name, Instant::now()));
        std::thread::sleep(self.delay);
        Ok(MotorStatus::default())
    }
}

/// A controller whose axis 0 polls slowly, run for a few passes; the log
/// of every poll it made.
async fn run_controller() -> Vec<(&'static str, Instant)> {
    let log = Log::default();
    let controller_log = log.clone();
    let mut controller = ControllerBuilder::new()
        .controller_poll(move || controller_log.lock().unwrap().push(("C", Instant::now())));
    let mut senders = Vec::new();
    for (addr, (name, delay)) in [("A0", SLOW), ("A1", Duration::ZERO)]
        .into_iter()
        .enumerate()
    {
        let motor = Axis {
            name,
            log: log.clone(),
            delay,
        };
        let ControllerAxis { poll_cmd_tx, .. } = controller.axis(
            Arc::new(Mutex::new(motor)),
            TraceUser::Asyn {
                port: String::new(),
                addr: addr as i32,
            },
        );
        senders.push(poll_cmd_tx);
    }
    let poller = tokio::spawn(controller.build(PERIOD, PERIOD, 0).run());
    tokio::time::sleep(SLOW * 4).await;
    senders[1].send(PollCommand::Shutdown).await.unwrap();
    poller.await.unwrap();
    log.lock().unwrap().clone()
}

#[tokio::test]
async fn each_pass_polls_the_controller_then_the_axes_in_order() {
    let log = run_controller().await;
    let names: Vec<_> = log.iter().map(|(name, _)| *name).collect();
    assert!(names.len() >= 6, "{names:?}");
    let whole = names.len() / 3 * 3;
    assert_eq!(
        names[..whole],
        ["C", "A0", "A1"].repeat(whole / 3),
        "{names:?}"
    );
}

#[tokio::test]
async fn a_slow_axis_delays_the_axes_after_it() {
    let log = run_controller().await;
    let axis0: Vec<_> = log
        .iter()
        .filter(|(n, _)| *n == "A0")
        .map(|e| e.1)
        .collect();
    let axis1: Vec<_> = log
        .iter()
        .filter(|(n, _)| *n == "A1")
        .map(|e| e.1)
        .collect();
    assert!(axis1.len() >= 2, "{log:?}");
    for (a0, a1) in axis0.iter().zip(&axis1) {
        assert!(
            *a1 >= *a0 + SLOW,
            "axis 1 polled {:?} after axis 0",
            *a1 - *a0
        );
    }
}

/// SmarAct and phytron start the poller in the controller's constructor
/// and create the axes after it: an axis added to the running poller is
/// polled on every pass, in address order.
#[tokio::test]
async fn an_axis_added_after_the_poller_starts_joins_it_in_address_order() {
    let log = Log::default();
    let controller_log = log.clone();
    let mut controller = ControllerBuilder::new()
        .controller_poll(move || controller_log.lock().unwrap().push(("C", Instant::now())));
    let axis = |name, addr: i32| {
        let motor = Axis {
            name,
            log: log.clone(),
            delay: Duration::ZERO,
        };
        let user = TraceUser::Asyn {
            port: String::new(),
            addr,
        };
        (
            Arc::new(Mutex::new(motor)) as Arc<Mutex<dyn AsynMotor>>,
            user,
        )
    };
    let (motor, user) = axis("A2", 2);
    let ControllerAxis { poll_cmd_tx, .. } = controller.axis(motor, user);
    let axes = controller.axes();
    let poller = tokio::spawn(controller.build(PERIOD, PERIOD, 0).run());
    tokio::time::sleep(PERIOD * 4).await;
    let (motor, user) = axis("A0", 0);
    // Held: a dropped axis ends the poller.
    let _a0 = tokio::task::spawn_blocking(move || axes.add(motor, user))
        .await
        .unwrap()
        .unwrap();
    let joined = log.lock().unwrap().len();
    tokio::time::sleep(PERIOD * 8).await;
    poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await.unwrap();
    // A pass may be under way when the add returns; count from the next.
    let names: Vec<_> = log.lock().unwrap()[joined..]
        .iter()
        .map(|(name, _)| *name)
        .skip_while(|&name| name != "C")
        .collect();
    assert!(names.len() >= 6, "{names:?}");
    let whole = names.len() / 3 * 3;
    assert_eq!(
        names[..whole],
        ["C", "A0", "A2"].repeat(whole / 3),
        "{names:?}"
    );
}

/// Moves one count every poll, so every poll posts a change.
struct Creeping(Arc<std::sync::atomic::AtomicU32>);

impl AsynMotor for Creeping {
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
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        Ok(MotorStatus {
            position: f64::from(n),
            ..MotorStatus::default()
        })
    }
}

/// At each poll, the pulses axis 0's record had received and the polls
/// axis 0 had made.
struct Watching {
    io_intr: tokio::sync::mpsc::Receiver<()>,
    pulses: u32,
    polls: Arc<std::sync::atomic::AtomicU32>,
    seen: Arc<Mutex<Vec<(u32, u32)>>>,
}

impl AsynMotor for Watching {
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
        while self.io_intr.try_recv().is_ok() {
            self.pulses += 1;
        }
        let polls = self.polls.load(std::sync::atomic::Ordering::SeqCst);
        self.seen.lock().unwrap().push((polls, self.pulses));
        Ok(MotorStatus::default())
    }
}

/// C's poller posts each axis's change as its poll returns
/// (asynMotorAxis::callParamCallbacks inside pAxis->poll), so on a silent
/// controller m1 alarms one read timeout before m2.
#[tokio::test]
async fn an_axis_s_post_reaches_its_records_before_the_next_axis_polls() {
    use epics_base_rs::runtime::interrupt_accept::InterruptGate;
    use epics_base_rs::server::device_support::DeviceSupport;

    let polls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut controller = ControllerBuilder::new();
    let user = |addr| TraceUser::Asyn {
        port: String::new(),
        addr,
    };
    let a0 = controller.axis(Arc::new(Mutex::new(Creeping(polls.clone()))), user(0));
    let mut record = a0.axis.attach();
    record.bind_dev_sup_gate(&InterruptGate::opened());
    let watching = Watching {
        io_intr: record.io_intr_receiver().unwrap(),
        pulses: 0,
        polls: polls.clone(),
        seen: seen.clone(),
    };
    let a1 = controller.axis(Arc::new(Mutex::new(watching)), user(1));
    let poller = tokio::spawn(controller.build(PERIOD, PERIOD, 0).run());
    tokio::time::sleep(PERIOD * 8).await;
    a1.poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await.unwrap();
    drop(a0);
    let seen = seen.lock().unwrap().clone();
    assert!(seen.len() >= 3, "{seen:?}");
    // The startup poll posts nothing; every later poll of axis 0 does.
    for &(polls, pulses) in &seen[1..] {
        assert_eq!(pulses, polls - 1, "{seen:?}");
    }
}

/// C's poller runs one startup pass, the wakeupPoller before its loop
/// (asynMotorController.cpp:630); an axis created after the poller started
/// is first polled on a later pass, not on one of its own.
#[tokio::test]
async fn axes_joining_a_running_poller_get_no_pass_of_their_own() {
    let log = Log::default();
    let controller_log = log.clone();
    let controller = ControllerBuilder::new()
        .controller_poll(move || controller_log.lock().unwrap().push(("C", Instant::now())));
    let axes = controller.axes();
    let idle = Duration::from_secs(60);
    let poller = tokio::spawn(controller.build(idle, idle, 0).run());
    let names = |log: &Log| -> Vec<&'static str> {
        log.lock().unwrap().iter().map(|(name, _)| *name).collect()
    };
    let mut joined = Vec::new();
    for (addr, name) in ["A0", "A1", "A2"].into_iter().enumerate() {
        let motor = Axis {
            name,
            log: log.clone(),
            delay: Duration::ZERO,
        };
        let axes = axes.clone();
        let user = TraceUser::Asyn {
            port: String::new(),
            addr: addr as i32,
        };
        let motor: Arc<Mutex<dyn AsynMotor>> = Arc::new(Mutex::new(motor));
        joined.push(
            tokio::task::spawn_blocking(move || axes.add(motor, user))
                .await
                .unwrap()
                .unwrap(),
        );
    }
    tokio::time::sleep(PERIOD * 4).await;
    assert_eq!(names(&log), ["C"], "the startup pass alone");
    joined[0]
        .poll_cmd_tx
        .send(PollCommand::Wakeup)
        .await
        .unwrap();
    tokio::time::sleep(PERIOD * 4).await;
    assert_eq!(names(&log), ["C", "C", "A0", "A1", "A2"]);
    joined[0]
        .poll_cmd_tx
        .send(PollCommand::Shutdown)
        .await
        .unwrap();
    poller.await.unwrap();
}

/// Done until it is told to move, then moving at the same place.
#[derive(Default)]
struct Mover {
    moved: bool,
}

impl AsynMotor for Mover {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        self.moved = true;
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
        Ok(MotorStatus {
            done: !self.moved,
            moving: self.moved,
            ..MotorStatus::default()
        })
    }
}

/// C `writeFloat64` posts the moved axis (`pAxis->callParamCallbacks()`)
/// before `wakeupPoller`, so its records see the move before the pass polls
/// the axes ahead of it (asynMotorController.cpp:317-320).
#[tokio::test]
async fn a_move_write_posts_before_the_pass_it_wakes() {
    use epics_base_rs::runtime::interrupt_accept::InterruptGate;
    use epics_base_rs::server::device_support::DeviceSupport;
    use motor_rs::controller_port::index;

    const PORT: &str = "CPMOVEPOST";
    let port =
        motor_rs::devices::register_controller_port(&asyn_rs::registry::open_owner(), PORT, 2, ())
            .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut controller = ControllerBuilder::new();
    let user = |addr| TraceUser::Asyn {
        port: PORT.into(),
        addr,
    };
    let mover = Arc::new(Mutex::new(Mover::default()));
    let m0 = {
        let watched = Arc::new(Mutex::new(None::<tokio::sync::mpsc::Receiver<()>>));
        let watcher = MoveWatcher {
            io_intr: watched.clone(),
            pulses: 0,
            seen: seen.clone(),
        };
        let m0 = controller.axis(Arc::new(Mutex::new(watcher)), user(0));
        let m1 = controller.axis(mover, user(1));
        let mut record = m1.axis.attach();
        record.bind_dev_sup_gate(&InterruptGate::opened());
        *watched.lock().unwrap() = record.io_intr_receiver();
        motor_rs::devices::register_axis(m0.axis.clone()).unwrap();
        motor_rs::devices::register_axis(m1.axis.clone()).unwrap();
        (m0, record)
    };
    let idle = Duration::from_secs(60);
    let poller = tokio::spawn(controller.build(idle, idle, 0).run());
    tokio::time::sleep(PERIOD * 4).await;
    tokio::task::spawn_blocking(move || port.write_float64_blocking(index::MOVE_ABS, 1, 3.0))
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(PERIOD * 4).await;
    // m0 once more: every post m1's records had by now.
    m0.0.poll_cmd_tx.send(PollCommand::ForcePoll).await.unwrap();
    tokio::time::sleep(PERIOD * 4).await;
    m0.0.poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await.unwrap();
    let seen = seen.lock().unwrap().clone();
    // At the startup pass, the pass the move woke and the forced poll: m1's
    // move post (DMOV 0), then its poll's (MOVN 1).
    assert_eq!(seen, [0, 1, 2]);
}

/// At each poll, the pulses another axis's record had received.
struct MoveWatcher {
    io_intr: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<()>>>>,
    pulses: u32,
    seen: Arc<Mutex<Vec<u32>>>,
}

impl AsynMotor for MoveWatcher {
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
        if let Some(rx) = self.io_intr.lock().unwrap().as_mut() {
            while rx.try_recv().is_ok() {
                self.pulses += 1;
            }
        }
        self.seen.lock().unwrap().push(self.pulses);
        Ok(MotorStatus::default())
    }
}
