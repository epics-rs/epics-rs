//! One poller per controller (asynMotorController.cpp:615-695): each pass
//! runs the controller poll, then every axis in index order on the one
//! thread, so a slow axis delays the axes after it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure};
use asyn_rs::user::AsynUser;
use motor_rs::builder::{ControllerAxes, ControllerAxis, ControllerBuilder};
use motor_rs::poll_loop::PollCommand;
use motor_rs::trace::TraceUser;

mod passes;

type Log = Arc<Mutex<Vec<(&'static str, Instant)>>>;

const SLOW: Duration = Duration::from_millis(40);
const PERIOD: Duration = Duration::from_millis(5);

/// Waits for `done`: a slow host's timers reach it late, never not at all.
async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// [`until`] on a thread that may block.
fn until_blocking(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Waits for `log` to hold `n` entries. An added axis stops counting as
/// pending at its own first poll, before the pass polls the axes after it.
fn polled(log: &Log, n: usize) {
    until_blocking(&format!("{n} polls never ran"), || {
        log.lock().unwrap().len() >= n
    });
}

struct Axis {
    name: &'static str,
    log: Log,
    delay: Duration,
}

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
        self.log.lock().unwrap().push((self.name, Instant::now()));
        std::thread::sleep(self.delay);
        *status = MotorStatus::default();
        Ok(())
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
    let poller = tokio::spawn(controller.build(PERIOD, PERIOD, 0).start());
    until("two passes", || log.lock().unwrap().len() >= 6).await;
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
    // No timed pass: each pass below is one a Wakeup asks for.
    let idle = Duration::from_secs(3600);
    let poller = tokio::spawn(controller.build(idle, idle, 0).start());
    // The startup pass runs on the poller thread and would poll A0 or not
    // by when the add lands, as C's may: add after it.
    until("the startup pass never ended", || {
        axes.first_polls_pending() == 0
    })
    .await;
    let (motor, user) = axis("A0", 0);
    // Held: a dropped axis ends the poller.
    let _a0 = tokio::task::spawn_blocking(move || axes.add(motor, user))
        .await
        .unwrap()
        .unwrap();
    let joined = log.lock().unwrap().len();
    for pass in 1..=2 {
        poll_cmd_tx.send(PollCommand::Wakeup).await.unwrap();
        until(&format!("pass {pass} never ran"), || {
            log.lock().unwrap().len() >= joined + 3 * pass
        })
        .await;
    }
    poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await.unwrap();
    let names: Vec<_> = log.lock().unwrap()[joined..]
        .iter()
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(names, ["C", "A0", "A2"].repeat(2));
}

/// Moves one count every poll, so every poll posts a change.
struct Creeping(Arc<std::sync::atomic::AtomicU32>);

impl AsynMotor for Creeping {
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
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        *status = MotorStatus {
            position: f64::from(n),
            ..MotorStatus::default()
        };
        Ok(())
    }
}

/// At each poll, the processes axis 0's record had run and the polls
/// axis 0 had made.
struct Watching {
    reads: passes::Reads,
    processes: u32,
    polls: Arc<std::sync::atomic::AtomicU32>,
    seen: Arc<Mutex<Vec<(u32, u32)>>>,
}

impl AsynMotor for Watching {
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
        self.processes += self.reads.take().len() as u32;
        let polls = self.polls.load(std::sync::atomic::Ordering::SeqCst);
        self.seen.lock().unwrap().push((polls, self.processes));
        *status = MotorStatus::default();
        Ok(())
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
        reads: passes::bind_support(&mut record, None),
        processes: 0,
        polls: polls.clone(),
        seen: seen.clone(),
    };
    let a1 = controller.axis(Arc::new(Mutex::new(watching)), user(1));
    let poller = tokio::spawn(controller.build(PERIOD, PERIOD, 0).start());
    until("three passes", || seen.lock().unwrap().len() >= 3).await;
    a1.poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await.unwrap();
    drop(a0);
    let seen = seen.lock().unwrap().clone();
    assert!(seen.len() >= 3, "{seen:?}");
    // Every poll of axis 0 posts, the startup pass's too: its record is
    // bound before the poller starts, as ANF2StartPoller's after iocInit
    // (asynMotorController.cpp:630, :658; asynMotorAxis.cpp:318-320).
    for &(polls, processes) in &seen {
        assert_eq!(processes, polls, "{seen:?}");
    }
}

/// An axis created after the poller started is polled at its join, after
/// the controller poll and before its add returns, as C's pass waiting on
/// the lock CreateAxis holds polls it (asynMotorController.cpp:644-658);
/// the join polls no other axis.
#[tokio::test]
async fn an_axis_joining_a_running_poller_is_polled_alone_before_its_add_returns() {
    let log = Log::default();
    let controller_log = log.clone();
    let controller = ControllerBuilder::new()
        .controller_poll(move || controller_log.lock().unwrap().push(("C", Instant::now())));
    let axes = controller.axes();
    let idle = Duration::from_secs(60);
    let poller = tokio::spawn(controller.build(idle, idle, 0).start());
    let names = |log: &Log| -> Vec<&'static str> {
        log.lock().unwrap().iter().map(|(name, _)| *name).collect()
    };
    // Created after the startup pass, not merely after the spawn.
    until("the startup pass", || !names(&log).is_empty()).await;
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
    assert_eq!(names(&log), ["C", "C", "A0", "C", "A1", "C", "A2"]);
    joined[0]
        .poll_cmd_tx
        .send(PollCommand::Wakeup)
        .await
        .unwrap();
    until("the woken pass", || names(&log).len() >= 11).await;
    assert_eq!(names(&log)[7..], ["C", "A0", "A1", "A2"]);
    joined[0]
        .poll_cmd_tx
        .send(PollCommand::Shutdown)
        .await
        .unwrap();
    poller.await.unwrap();
}

/// Done until it is told to move, then moving at the same place, which
/// its poll sets as C sets `motorStatusMoving_`.
#[derive(Default)]
struct Mover {
    moved: bool,
}

impl AsynMotor for Mover {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        self.moved = true;
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
        *status = MotorStatus {
            done: !self.moved,
            moving: self.moved,
            msta_moving: self.moved,
            ..MotorStatus::default()
        };
        Ok(())
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
        let watched = Arc::new(Mutex::new(None::<passes::Reads>));
        let watcher = MoveWatcher {
            reads: watched.clone(),
            processes: 0,
            seen: seen.clone(),
        };
        let m0 = controller.axis(Arc::new(Mutex::new(watcher)), user(0));
        let m1 = controller.axis(mover, user(1));
        let mut record = m1.axis.attach();
        record.bind_dev_sup_gate(&InterruptGate::opened());
        *watched.lock().unwrap() = Some(passes::bind_support(&mut record, None));
        motor_rs::devices::register_axis(m0.axis.clone()).unwrap();
        motor_rs::devices::register_axis(m1.axis.clone()).unwrap();
        (m0, record)
    };
    let axes = controller.axes();
    let idle = Duration::from_secs(60);
    let poller = tokio::spawn(controller.build(idle, idle, 0).start());
    until("the startup pass", || axes.first_polls_pending() == 0).await;
    tokio::task::spawn_blocking(move || port.write_float64_blocking(index::MOVE_ABS, 1, 3.0))
        .await
        .unwrap()
        .unwrap();
    until("the woken pass", || seen.lock().unwrap().len() >= 2).await;
    // m0 once more: every post m1's records had by now.
    m0.0.poll_cmd_tx
        .send(PollCommand::ForcePoll(None))
        .await
        .unwrap();
    until("the forced poll", || seen.lock().unwrap().len() >= 3).await;
    m0.0.poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await.unwrap();
    let seen = seen.lock().unwrap().clone();
    // At the startup pass, the pass the move woke and the forced poll: m1's
    // startup post to its record bound before the poller started (C's
    // startup pass ends each axis poll in callParamCallbacks,
    // asynMotorController.cpp:630, :658; asynMotorAxis.cpp:318-320), its
    // move post (DMOV 0), then its poll's (MOVN 1).
    assert_eq!(seen, [0, 2, 3]);
}

/// At each poll, the processes another axis's record had run.
struct MoveWatcher {
    reads: Arc<Mutex<Option<passes::Reads>>>,
    processes: u32,
    seen: Arc<Mutex<Vec<u32>>>,
}

impl AsynMotor for MoveWatcher {
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
        if let Some(reads) = self.reads.lock().unwrap().as_ref() {
            self.processes += reads.take().len() as u32;
        }
        self.seen.lock().unwrap().push(self.processes);
        *status = MotorStatus::default();
        Ok(())
    }
}

/// The startup pass runs on the poller thread (asynMotorController.cpp:630),
/// so `start` counts it pending until every axis in it has polled.
#[tokio::test]
async fn a_started_poller_is_pending_until_its_startup_pass_ends() {
    let log = Log::default();
    let mut controller = ControllerBuilder::new();
    let axes = controller.axes();
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
    let idle = Duration::from_secs(3600);
    let poller = controller.build(idle, idle, 0).start();
    assert_eq!(axes.first_polls_pending(), 1);
    until_blocking("the startup pass never ended", || {
        axes.first_polls_pending() == 0
    });
    let names: Vec<_> = log.lock().unwrap().iter().map(|(n, _)| *n).collect();
    assert_eq!(names, ["A0", "A1"]);
    senders[1].send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}

/// An add to a running poller returns once the axis is polled: nothing is
/// left pending for a reader to wait on.
#[tokio::test(flavor = "multi_thread")]
async fn an_axis_added_to_a_running_poller_is_polled_when_its_add_returns() {
    let log = Log::default();
    let mut controller = ControllerBuilder::new();
    let axis = |name, addr| {
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
    let idle = Duration::from_secs(3600);
    let poller = controller.build(idle, idle, 0).start();
    let first = axes.clone();
    let settle = || {
        until_blocking("a first poll never ran", || {
            first.first_polls_pending() == 0
        })
    };
    settle();
    let (motor, user) = axis("A0", 0);
    let _a0 = tokio::task::spawn_blocking(move || axes.add(motor, user))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.first_polls_pending(), 0);
    let names = || -> Vec<_> { log.lock().unwrap().iter().map(|(n, _)| *n).collect() };
    assert_eq!(names(), ["A2", "A0"]);
    poll_cmd_tx.send(PollCommand::Wakeup).await.unwrap();
    polled(&log, 4);
    assert_eq!(names(), ["A2", "A0", "A0", "A2"]);
    poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}

// C builds an MVP2001 axis under pC->lock() (MVP2001Driver.cpp:222-224): a
// pass due meanwhile waits on the lock and then polls the new axis
// (asynMotorController.cpp:648-658).
#[tokio::test(flavor = "multi_thread")]
async fn an_axis_built_under_the_lock_is_polled_by_the_pass_that_waited() {
    let log = Log::default();
    let mut controller = ControllerBuilder::new();
    let axis = |name, addr| {
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
    let (motor, user) = axis("B2", 2);
    let ControllerAxis { poll_cmd_tx, .. } = controller.axis(motor, user);
    let axes = controller.axes();
    let idle = Duration::from_secs(3600);
    let poller = controller.build(idle, idle, 0).start();
    let first = axes.clone();
    let settle = || {
        until_blocking("a first poll never ran", || {
            first.first_polls_pending() == 0
        })
    };
    settle();
    let (motor, user) = axis("B0", 0);
    let wake = poll_cmd_tx.clone();
    let _b0 = tokio::task::spawn_blocking(move || {
        axes.add_built(user, || {
            wake.blocking_send(PollCommand::Wakeup).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            Ok::<_, String>((motor, ()))
        })
    })
    .await
    .unwrap()
    .unwrap();
    polled(&log, 3);
    let names: Vec<_> = log.lock().unwrap().iter().map(|(n, _)| *n).collect();
    assert_eq!(names, ["B2", "B0", "B2"]);
    poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}

// pmacCreateAxes builds every axis under one pC->lock()
// (pmacController.cpp:974-979): a pass due during the first build waits out
// the whole batch, then polls the new axes.
#[tokio::test(flavor = "multi_thread")]
async fn no_pass_polls_between_two_builds_of_one_batch() {
    let log = Log::default();
    let mut controller = ControllerBuilder::new();
    let axis = |name, addr| {
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
    let (motor, user) = axis("C3", 3);
    let ControllerAxis { poll_cmd_tx, .. } = controller.axis(motor, user);
    let axes = controller.axes();
    let idle = Duration::from_secs(3600);
    let poller = controller.build(idle, idle, 0).start();
    let first = axes.clone();
    let settle = || {
        until_blocking("a first poll never ran", || {
            first.first_polls_pending() == 0
        })
    };
    settle();
    let builds = [("C1", 1, "built C1"), ("C2", 2, "built C2")].map(|(name, addr, built)| {
        let (motor, user) = axis(name, addr);
        let wake = poll_cmd_tx.clone();
        let log = log.clone();
        let build = move || {
            wake.blocking_send(PollCommand::Wakeup).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            log.lock().unwrap().push((built, Instant::now()));
            Ok::<_, String>((motor, ()))
        };
        (user, build)
    });
    // The pause before the second build is where a hold per build would
    // let the waiting pass in.
    let builds = builds.into_iter().enumerate().map(|(i, build)| {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        build
    });
    let (added, done) = tokio::task::spawn_blocking(move || axes.add_built_all(builds))
        .await
        .unwrap();
    done.unwrap();
    assert_eq!(added.len(), 2);
    polled(&log, 4);
    let names: Vec<_> = log.lock().unwrap().iter().map(|(n, _)| *n).collect();
    assert_eq!(names[..4], ["C3", "built C1", "built C2", "C1"]);
    poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}

// A build that fails ends the batch: the axes built before it stay added
// and are polled from the next pass, the builds after it never run, and the
// failed axis can be added again.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_build_ends_the_batch_and_keeps_the_axes_before_it() {
    let log = Log::default();
    let mut controller = ControllerBuilder::new();
    let axis = |name, addr| {
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
    let (motor, user) = axis("C3", 3);
    let ControllerAxis { poll_cmd_tx, .. } = controller.axis(motor, user);
    let axes = controller.axes();
    let idle = Duration::from_secs(3600);
    let poller = controller.build(idle, idle, 0).start();
    let first = axes.clone();
    let settle = || {
        until_blocking("a first poll never ran", || {
            first.first_polls_pending() == 0
        })
    };
    settle();
    type Build = Box<dyn FnOnce() -> Result<(Arc<Mutex<dyn AsynMotor>>, ()), String> + Send>;
    let built = |name, label: &'static str, addr, fails| -> (TraceUser, Build) {
        let (motor, user) = axis(name, addr);
        let wake = poll_cmd_tx.clone();
        let log = log.clone();
        let build = move || {
            log.lock().unwrap().push((label, Instant::now()));
            if fails {
                return Err(format!("{name} failed"));
            }
            wake.blocking_send(PollCommand::Wakeup).unwrap();
            Ok((motor, ()))
        };
        (user, Box::new(build))
    };
    let builds = vec![
        built("C1", "build C1", 1, false),
        built("C2", "build C2", 2, true),
        built("C4", "build C4", 4, false),
    ];
    let batch = axes.clone();
    let (added, done) = tokio::task::spawn_blocking(move || batch.add_built_all(builds))
        .await
        .unwrap();
    assert_eq!(done, Err("C2 failed".to_string()));
    assert_eq!(added.len(), 1);
    settle();
    let (user, build) = built("C2", "build C2", 2, false);
    tokio::task::spawn_blocking(move || axes.add_built(user, build))
        .await
        .unwrap()
        .unwrap();
    // C2's first poll ends before the pass reaches C3.
    until_blocking("the pass after the retry never ended", || {
        log.lock().unwrap().len() >= 9
    });
    let names: Vec<_> = log.lock().unwrap().iter().map(|(n, _)| *n).collect();
    assert_eq!(
        names[..9],
        [
            "C3", "build C1", "build C2", "C1", "C3", "build C2", "C1", "C2", "C3"
        ]
    );
    poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}

/// An axis whose poll fails, reporting C's `*moving` past the failure.
struct Unreachable {
    log: Log,
}

impl AsynMotor for Unreachable {
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
        PollFailure::PROBLEM
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        self.log.lock().unwrap().push(("U", Instant::now()));
        status.moving = true;
        Err(asyn_rs::error::AsynError::Status {
            status: asyn_rs::error::AsynStatus::Timeout,
            message: "unreachable".into(),
        })
    }
}

// C's poller paces on what a failed poll reports as moving: phytron's
// `!motorStatusDone_` keeps an unreachable controller at the moving period
// (phytronAxisMotor.cpp:1868-1869, asynMotorController.cpp:686-692).
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_poll_reporting_moving_keeps_the_moving_period() {
    let log = Log::default();
    let mut controller = ControllerBuilder::new();
    let motor = Unreachable { log: log.clone() };
    let ControllerAxis { poll_cmd_tx, .. } = controller.axis(
        Arc::new(Mutex::new(motor)),
        TraceUser::Asyn {
            port: String::new(),
            addr: 0,
        },
    );
    let poller = controller
        .build(PERIOD, Duration::from_secs(3600), 0)
        .start();
    // The hour-long idle period would poll once in the 10 s this allows.
    until("polls at the 5 ms moving period", || {
        log.lock().unwrap().len() > 5
    })
    .await;
    poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}

/// A controller built without `.port()` is its axis's port's poller once
/// the axis names the port, so the port counts the controller's first pass.
#[tokio::test(flavor = "multi_thread")]
async fn a_port_named_by_its_axis_counts_the_controllers_first_poll() {
    use asyn_rs::registry::open_owner;
    use motor_rs::devices::{first_polls_pending, register_controller_port};

    let port = "pollerNamedByAxis";
    register_controller_port(&open_owner(), port, 1, ()).unwrap();
    let log = Log::default();
    let mut controller = ControllerBuilder::new();
    let motor = Axis {
        name: "A0",
        log: log.clone(),
        delay: SLOW,
    };
    let user = TraceUser::Asyn {
        port: port.into(),
        addr: 0,
    };
    let ControllerAxis { poll_cmd_tx, .. } = controller.axis(Arc::new(Mutex::new(motor)), user);
    let axes = controller.axes();
    let idle = Duration::from_secs(3600);
    let poller = controller.build(idle, idle, 0).start();
    assert_eq!(axes.first_polls_pending(), 1);
    assert_eq!(first_polls_pending(port), Some(1));
    until_blocking("the startup pass never ended", || {
        axes.first_polls_pending() == 0
    });
    assert_eq!(first_polls_pending(port), Some(0));
    poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}

/// simMotorCreate names its poller before it registers the port; the port
/// still counts that poller's first pass.
#[tokio::test(flavor = "multi_thread")]
async fn a_port_registered_after_its_poller_counts_the_first_poll() {
    use asyn_rs::registry::open_owner;
    use motor_rs::devices::{first_polls_pending, register_controller_port};

    let port = "pollerBeforePort";
    let mut controller = ControllerBuilder::new().port(port);
    let motor = Axis {
        name: "A0",
        log: Log::default(),
        delay: SLOW,
    };
    let user = TraceUser::Asyn {
        port: port.into(),
        addr: 0,
    };
    let ControllerAxis { poll_cmd_tx, .. } = controller.axis(Arc::new(Mutex::new(motor)), user);
    register_controller_port(&open_owner(), port, 1, ()).unwrap();
    let axes = controller.axes();
    let idle = Duration::from_secs(3600);
    let poller = controller.build(idle, idle, 0).start();
    assert_eq!(axes.first_polls_pending(), 1);
    assert_eq!(first_polls_pending(port), Some(1));
    poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}

/// A driver's own poller thread counts its first pass the same way, on its
/// own controller alone.
#[test]
fn a_held_first_poll_counts_until_it_drops() {
    let (axes, other) = (ControllerAxes::default(), ControllerAxes::default());
    let first = axes.first_poll_pending();
    assert_eq!(axes.first_polls_pending(), 1);
    assert_eq!(other.first_polls_pending(), 0);
    drop(first);
    assert_eq!(axes.first_polls_pending(), 0);
}

/// ANF2CreateController registers its port and ANF2StartPoller names the
/// poller later: before then the port has no first poll pending, and only
/// a port that does not exist reads `None`.
#[test]
fn a_port_reads_its_poller_s_first_polls_and_0_before_one_is_named() {
    use motor_rs::devices::{first_polls_pending, register_controller_port};

    const PORT: &str = "CPFIRSTPOLLS";
    register_controller_port(&asyn_rs::registry::open_owner(), PORT, 1, ()).unwrap();
    assert_eq!(first_polls_pending(PORT), Some(0));
    assert_eq!(first_polls_pending("CPFIRSTPOLLSNONE"), None);
    let controller = ControllerBuilder::new().port(PORT);
    let first = controller.axes().first_poll_pending();
    assert_eq!(first_polls_pending(PORT), Some(1));
    drop(first);
    assert_eq!(first_polls_pending(PORT), Some(0));
}

/// Polls once, then returns before its callParamCallbacks every time,
/// having set HOME.
#[derive(Default)]
struct Bailing {
    polls: Arc<std::sync::atomic::AtomicU32>,
}

impl AsynMotor for Bailing {
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
        PollFailure::NONE
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        if self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            *status = MotorStatus::default();
            return Ok(());
        }
        status.home = true;
        Err(asyn_rs::error::AsynError::Status {
            status: asyn_rs::error::AsynStatus::Error,
            message: "bailed".into(),
        })
    }
}

/// The processes `reads` sees: the first, then any other in PERIOD * 8 of
/// polls after it.
async fn processes(reads: &passes::Reads) -> usize {
    let first = reads.wait(Duration::from_secs(10)).await.len();
    tokio::time::sleep(PERIOD * 8).await;
    first + reads.take().len()
}

/// What a bailing poll set raises statusChanged_ (asynMotorAxis.cpp:
/// 255-264). A port write that runs no axis method still ends in
/// pAxis->callParamCallbacks(), which posts it (asynMotorController.cpp:246).
#[tokio::test]
async fn a_port_write_running_no_axis_method_posts_what_a_bailing_poll_left() {
    use epics_base_rs::runtime::interrupt_accept::InterruptGate;
    use epics_base_rs::server::device_support::DeviceSupport;
    use motor_rs::controller_port::index;
    const PORT: &str = "BAILLEFT";
    let port =
        motor_rs::devices::register_controller_port(&asyn_rs::registry::open_owner(), PORT, 2, ())
            .unwrap();
    let mut controller = ControllerBuilder::new();
    let bailing = Bailing::default();
    let polls = bailing.polls.clone();
    let m = controller.axis(
        Arc::new(Mutex::new(bailing)),
        TraceUser::Asyn {
            port: PORT.into(),
            addr: 1,
        },
    );
    let mut record = m.axis.attach();
    record.bind_dev_sup_gate(&InterruptGate::opened());
    let reads = passes::bind_support(&mut record, None);
    motor_rs::devices::register_axis(m.axis.clone()).unwrap();
    let poller = tokio::spawn(controller.build(PERIOD, PERIOD, 0).start());
    assert_eq!(
        processes(&reads).await,
        1,
        "only the startup poll, which the constructor raised, posts"
    );
    // The third poll starts once the pass of the second, the first to bail,
    // has left its status; a write before that has nothing to post.
    until("a bailed pass", || {
        polls.load(std::sync::atomic::Ordering::SeqCst) >= 3
    })
    .await;
    tokio::task::spawn_blocking(move || port.write_int32_blocking(index::POWER_AUTO_ONOFF, 1, 0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(processes(&reads).await, 1);
    m.poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await.unwrap();
}

/// Bails `bails` times before its first poll that calls back.
struct LateFirstPoll {
    bails: u32,
    constructor_calls_back: bool,
    polls: Arc<std::sync::atomic::AtomicU32>,
}

impl AsynMotor for LateFirstPoll {
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
        PollFailure::NONE
    }
    fn poll(&mut self, _: &AsynUser, status: &mut MotorStatus) -> AsynResult<()> {
        self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.bails > 0 {
            self.bails -= 1;
            return Err(asyn_rs::error::AsynError::Status {
                status: asyn_rs::error::AsynStatus::Error,
                message: "bailed".into(),
            });
        }
        *status = MotorStatus::default();
        Ok(())
    }
    fn constructor_calls_param_callbacks(&self) -> bool {
        self.constructor_calls_back
    }
}

/// asynMotorAxis's constructor raises statusChanged_ (asynMotorAxis.cpp:29),
/// so the first poll that calls back posts the constructor's status to a
/// record already bound, unless the driver's constructor called back first
/// (:316-322).
#[tokio::test]
async fn the_first_poll_to_call_back_posts_unless_the_constructor_did() {
    use epics_base_rs::runtime::interrupt_accept::InterruptGate;
    use epics_base_rs::server::device_support::DeviceSupport;
    for (constructor_calls_back, posts) in [(false, 1), (true, 0)] {
        let mut controller = ControllerBuilder::new();
        let polls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let motor = LateFirstPoll {
            bails: 2,
            constructor_calls_back,
            polls: polls.clone(),
        };
        let m = controller.axis(
            Arc::new(Mutex::new(motor)),
            TraceUser::Asyn {
                port: String::new(),
                addr: 0,
            },
        );
        let mut record = m.axis.attach();
        record.bind_dev_sup_gate(&InterruptGate::opened());
        let reads = passes::bind_support(&mut record, None);
        let poller = tokio::spawn(controller.build(PERIOD, PERIOD, 0).start());
        // The poll after the one that calls back, which its post precedes.
        until("four polls", || {
            polls.load(std::sync::atomic::Ordering::SeqCst) >= 4
        })
        .await;
        m.poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
        poller.await.unwrap();
        let processes = reads.take().len();
        assert_eq!(
            processes, posts,
            "constructor called back: {constructor_calls_back}"
        );
    }
}
