//! C runs an sscan's link callbacks on the `recDynLink` threads, so a full
//! callback band cannot lose the search callback that marks a link
//! connected. Here they run on the record's band: a full band delays the
//! event, and the record takes it once the band frees.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::background::callback_executor::{set_parallel_threads, set_queue_size};
use epics_base_rs::runtime::task::{
    CallbackPriority, background_callback_stats, sleep, spawn_blocking_background,
};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

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

fn f64_of(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn put_field(db: &PvDatabase, scan: &str, field: &str, v: EpicsValue) {
    db.put_record_field_from_ca(scan, field, v).await.unwrap();
}

async fn until(what: &str, fact: impl Fn() -> bool) {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while !fact() {
        assert!(Instant::now() < deadline, "{what}");
        sleep(Duration::from_millis(5)).await;
    }
}

/// `S` takes its link events on the low band, `X` on the high one, once it
/// has processed; one `recDynLink` thread connects S's trigger before X's.
#[epics_base_rs::epics_test]
async fn a_trigger_link_connects_after_a_full_band_frees() {
    // `callbackSetQueueSize` and `callbackParallelThreads` before `callbackInit`.
    set_queue_size(1);
    set_parallel_threads(1, Some(CallbackPriority::Low));
    let (name, factory) = sscan_rs::sscan_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .db_string(
            r#"
record(calc, "D") { field(CALC, "1") }
record(sscan, "S") { field(MPTS, "10") field(NPTS, "5") }
record(sscan, "X") { field(MPTS, "10") field(NPTS, "5") field(PRIO, "HIGH") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    // An sscan takes its band from PRIO as it processes.
    put_field(&db, "X", "PROC", EpicsValue::Short(1)).await;

    let full = fill_low_band();
    put_field(&db, "S", "T1PV", EpicsValue::String("D.PROC".into())).await;
    put_field(&db, "X", "T1PV", EpicsValue::String("D.PROC".into())).await;
    until("X's trigger never connected", || {
        f64_of(&db, "X.T1NV") == 0.0
    })
    .await;
    assert_ne!(
        f64_of(&db, "S.T1NV"),
        0.0,
        "the full band holds S's search callback"
    );
    drop(full);
    until("S's search callback was lost to the full band", || {
        f64_of(&db, "S.T1NV") == 0.0
    })
    .await;
}
