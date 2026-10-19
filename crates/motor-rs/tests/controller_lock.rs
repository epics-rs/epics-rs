//! C's poller holds the controller lock across `poll()` and every axis
//! poll (asynMotorController.cpp:644-658), and writeFloat64 runs under the
//! same lock, so no move lands between a controller poll and the axis
//! polls that parse what it read.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure};
use asyn_rs::registry::open_owner;
use asyn_rs::user::AsynUser;
use motor_rs::builder::{ControllerAxis, ControllerBuilder};
use motor_rs::controller_port::index;
use motor_rs::devices::{register_axis, register_controller_port};
use motor_rs::trace::TraceUser;

#[derive(Default)]
struct Hw {
    moving: AtomicBool,
    /// What the controller poll last read, as SPiiPlus's binary poll.
    cache_moving: AtomicBool,
}

struct Cached {
    hw: Arc<Hw>,
    /// (cache said done, hardware moving) for each axis poll.
    seen: Arc<Mutex<Vec<(bool, bool)>>>,
}

impl AsynMotor for Cached {
    fn move_absolute(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: f64) -> AsynResult<()> {
        self.hw.moving.store(true, Ordering::SeqCst);
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
        let cached = self.hw.cache_moving.load(Ordering::SeqCst);
        let hw = self.hw.moving.load(Ordering::SeqCst);
        self.seen.lock().unwrap().push((!cached, hw));
        Ok(MotorStatus {
            done: !cached,
            moving: cached,
            ..MotorStatus::default()
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_port_write_waits_out_the_poller_s_pass() {
    let port = register_controller_port(&open_owner(), "CLPASS", 1, ()).unwrap();
    let hw = Arc::new(Hw::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let armed = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (release_rx, entered_tx) = (Mutex::new(release_rx), Mutex::new(entered_tx));
    let (phw, parmed) = (hw.clone(), armed.clone());
    // The pass parks inside its controller poll once armed.
    let mut controller = ControllerBuilder::new().controller_poll(move || {
        phw.cache_moving
            .store(phw.moving.load(Ordering::SeqCst), Ordering::SeqCst);
        if parmed.swap(false, Ordering::SeqCst) {
            entered_tx.lock().unwrap().send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
    });
    let motor = Cached {
        hw: hw.clone(),
        seen: seen.clone(),
    };
    let ControllerAxis { axis, .. } = controller.axis(
        Arc::new(Mutex::new(motor)),
        TraceUser::Asyn {
            port: "CLPASS".into(),
            addr: 0,
        },
    );
    register_axis(axis).unwrap();
    let period = Duration::from_millis(5);
    let _poller = tokio::spawn(controller.build(period, period, 0).start());
    armed.store(true, Ordering::SeqCst);
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(5)))
        .await
        .unwrap()
        .unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let r = port.write_float64_blocking(index::MOVE_ABS, 0, 7.0);
        done_tx.send(r.is_ok()).unwrap();
    });
    let during = done_rx.recv_timeout(Duration::from_millis(300)).ok();
    seen.lock().unwrap().clear();
    release_tx.send(()).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while seen.lock().unwrap().is_empty() && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let first = seen.lock().unwrap().first().copied();
    // (the write returned while the pass ran, the pass's axis poll as
    // (done from the cache, hardware moving))
    assert_eq!((during, first), (None, Some((true, false))));
}
