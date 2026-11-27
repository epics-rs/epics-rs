//! C runs an swait's link callbacks on the `recDynLink` task, which processes
//! an I/O Intr record for each input change (swaitRecord.c:818-926), so a
//! full callback band cannot lose one. Here they run on the record's band: a
//! full band delays the change, and the record takes it once the band frees.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::background::callback_executor::{set_parallel_threads, set_queue_size};
use epics_base_rs::runtime::task::{
    CallbackPriority, background_callback_stats, spawn_blocking_background,
};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

const PV_OK: f64 = 0.0;

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

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn put(db: &PvDatabase, rec: &str, v: f64) {
    db.put_record_field_from_ca(rec, "VAL", EpicsValue::Double(v))
        .await
        .unwrap();
}

async fn until(db: &PvDatabase, what: &str, fact: impl Fn(&PvDatabase) -> bool) {
    let deadline = epics_base_rs::runtime::time::deadline_from_now(fact_budget::FACT_BUDGET);
    while !fact(db) {
        assert!(Instant::now() < deadline, "{what}");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

/// `W` takes its input changes on the low band, `X` on the high one; one
/// event queue posts SRC's change before MARK's.
#[epics_macros_rs::epics_test]
async fn an_input_change_on_a_full_band_still_processes_the_record() {
    // `callbackSetQueueSize` and `callbackParallelThreads` before `callbackInit`.
    set_queue_size(1);
    set_parallel_threads(1, Some(CallbackPriority::Low));
    let ioc = IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(
            r#"
record(ao, "SRC") { }
record(ao, "MARK") { }
record(swait, "W") { field(SCAN, "I/O Intr") field(INAN, "SRC") field(CALC, "A") }
record(swait, "X") { field(SCAN, "I/O Intr") field(INAN, "MARK") field(CALC, "A") field(PRIO, "HIGH") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = Arc::clone(ioc.database());
    assert!(ioc.run().await);
    until(&db, "the inputs never connected", |db| {
        num(db, "W.INAV") == PV_OK && num(db, "X.INAV") == PV_OK
    })
    .await;
    put(&db, "SRC", 1.0).await;
    put(&db, "MARK", 1.0).await;
    until(&db, "the first changes never processed", |db| {
        num(db, "W") == 1.0 && num(db, "X") == 1.0
    })
    .await;

    let full = fill_low_band();
    put(&db, "SRC", 2.0).await;
    put(&db, "MARK", 2.0).await;
    until(&db, "X never took MARK's change", |db| num(db, "X") == 2.0).await;
    assert_eq!(num(&db, "W"), 1.0, "the full band holds W's change");
    drop(full);
    until(&db, "W's input change was lost to the full band", |db| {
        num(db, "W") == 2.0
    })
    .await;
}
