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
    let mut controller =
        ControllerBuilder::new(Duration::from_millis(5), Duration::from_millis(5), 0)
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
    let poller = tokio::spawn(controller.build().run());
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
