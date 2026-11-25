//! A move a missing axis refuses still ends in C's `asynCallback`, which the
//! port thread runs as for any write (devMotorAsyn.c:661-737): the record
//! stops waiting for that move whatever the callback band holds. Here the
//! callback runs on the record's band: a full band delays it, never drops it.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::error::AsynResult;
use asyn_rs::interfaces::motor::{AsynMotor, MotorStatus, PollFailure};
use asyn_rs::registry::open_owner;
use asyn_rs::user::AsynUser;
use epics_base_rs::runtime::background::callback_executor::{set_parallel_threads, set_queue_size};
use epics_base_rs::runtime::task::{
    CallbackPriority, background_callback_stats, spawn_blocking_background,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use motor_rs::builder::{ControllerAxis, ControllerBuilder};
use motor_rs::devices::{device_support_factory, devices, register_axis, register_controller_port};
use motor_rs::poll_loop::PollCommand;
use motor_rs::trace::TraceUser;

/// Holds the low band's worker until dropped.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn wait(&self) {
        let (open, wake) = &*self.0;
        let mut open = open.lock().unwrap();
        while !*open {
            open = wake.wait(open).unwrap();
        }
    }
}

struct FullBand(Gate);

impl Drop for FullBand {
    fn drop(&mut self) {
        let (open, wake) = &*(self.0).0;
        *open.lock().unwrap() = true;
        wake.notify_all();
    }
}

fn low_overflows() -> u64 {
    background_callback_stats(false).unwrap()[CallbackPriority::Low.index()].num_overflow
}

/// Hold the low band's one worker, then take its one ring slot with ad-hoc
/// requests until one is refused.
fn fill_low_band() -> FullBand {
    let gate = Gate::default();
    let (running_tx, running_rx) = std::sync::mpsc::channel();
    let holder = gate.clone();
    drop(spawn_blocking_background(
        CallbackPriority::Low,
        move |_| {
            running_tx.send(()).unwrap();
            holder.wait();
        },
    ));
    running_rx.recv_timeout(fact_budget::FACT_BUDGET).unwrap();
    let refused = low_overflows() + 1;
    for _ in 0..64 {
        let filler = gate.clone();
        drop(spawn_blocking_background(
            CallbackPriority::Low,
            move |_| filler.wait(),
        ));
        if low_overflows() == refused {
            return FullBand(gate);
        }
    }
    panic!("the low band never filled");
}

/// A stage parked at step 5000.
struct Parked;

impl AsynMotor for Parked {
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
        *status = MotorStatus {
            done: true,
            ..MotorStatus::default().with_position(5000.0)
        };
        Ok(())
    }
}

/// `M` (PRIO LOW) moves while its controller address has no axis, on a full
/// band. An axis that joins later reaches the record only once the refused
/// move's callback has run: until then the record waits for that move.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_refused_on_a_full_band_still_ends() {
    // `callbackSetQueueSize` and `callbackParallelThreads` before `callbackInit`.
    set_queue_size(1);
    set_parallel_threads(1, Some(CallbackPriority::Low));
    register_controller_port(&open_owner(), "RMFULL", 1, ()).unwrap();
    let controller = ControllerBuilder::new();
    let axes = controller.axes();
    let tick = Duration::from_millis(10);
    let poller = controller.build(tick, tick, 0).start();
    let (name, factory) = motor_rs::motor_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_dynamic_device_support(&devices(), device_support_factory())
        .db_string(
            r#"record(motor, "M") { field(DTYP, "asynMotor") field(OUT, "@asyn(RMFULL,0)") field(MRES, "1") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build_served()
        .await
        .unwrap();
    db.scan_control().run();

    let full = fill_low_band();
    db.put_record_field_from_ca("M", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("M.DMOV").unwrap(),
        EpicsValue::Short(0),
        "the move is out"
    );
    drop(full);

    let user = TraceUser::Asyn {
        port: "RMFULL".into(),
        addr: 0,
    };
    let ControllerAxis { axis, poll_cmd_tx } =
        tokio::task::spawn_blocking(move || axes.add(Arc::new(Mutex::new(Parked)), user))
            .await
            .unwrap()
            .unwrap();
    register_axis(axis).unwrap();
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while db.get_pv("M.DRBV").unwrap() != EpicsValue::Double(5000.0) {
        assert!(
            Instant::now() < deadline,
            "the record still waits for the refused move: DRBV {:?}",
            db.get_pv("M.DRBV").unwrap()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    poll_cmd_tx.send(PollCommand::Shutdown).await.unwrap();
    poller.await;
}
