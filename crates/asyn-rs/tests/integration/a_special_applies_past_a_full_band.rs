//! C's port thread applies an `asynCallbackSpecial` result to the record
//! itself (asynRecord.c:834-900), so a full callback band cannot lose it.
//! Here the result is applied on the record's band: a full band delays it,
//! and it lands once the band frees.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::asyn_record::AsynRecord;
use asyn_rs::drivers::ip_port::DrvAsynIPPort;
use asyn_rs::registry::register_port;
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_libcom_rs::runtime::background::callback_executor::{
    set_parallel_threads, set_queue_size,
};
use epics_libcom_rs::runtime::task::{
    CallbackPriority, background_callback_stats, spawn_blocking_background,
};

const OPTION_ERROR: &str = "Error setting option, Unsupported key \"baud\"";

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

fn errs(db: &PvDatabase, rec: &str) -> String {
    let Some(EpicsValue::CharArray(buf)) =
        db.get_record(rec).unwrap().read().record.get_field("ERRS")
    else {
        panic!("ERRS is a char array");
    };
    String::from_utf8_lossy(buf.split(|b| *b == 0).next().unwrap()).into_owned()
}

async fn put_baud(db: &PvDatabase, rec: &str) {
    db.put_record_field_from_ca_no_notify(rec, "BAUD", EpicsValue::String("9600".into()))
        .await
        .unwrap();
}

async fn until(what: &str, fact: impl Fn() -> bool) {
    let deadline = epics_libcom_rs::runtime::time::deadline_from_now(fact_budget::FACT_BUDGET);
    while !fact() {
        assert!(Instant::now() < deadline, "{what}");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

/// `L` applies its specials on the low band and `H` on the high one; both
/// queue on one port, whose thread runs `L`'s before `H`'s.
#[epics_base_rs::epics_test]
async fn a_special_result_lands_after_a_full_band_frees() {
    // `callbackSetQueueSize` and `callbackParallelThreads` before `callbackInit`.
    set_queue_size(1);
    set_parallel_threads(1, Some(CallbackPriority::Low));
    // A device that takes the connection and never answers.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let _held = listener.accept();
        std::thread::park();
    });
    let driver = DrvAsynIPPort::new("SPFULL", &addr.to_string()).unwrap();
    let (rt, _jh) = create_port_runtime_unclaimed(driver, RuntimeConfig::default()).unwrap();
    register_port(
        "SPFULL",
        rt.port_handle().clone(),
        &asyn_rs::registry::open_owner(),
    )
    .unwrap();
    let (db, _) = IocBuilder::new()
        .register_record_type("asyn", || Box::new(AsynRecord::default()))
        .db_string(
            "record(asyn, \"L\") {\n  field(PORT, \"SPFULL\")\n  field(TMOD, \"NoI/O\")\n}\n\
             record(asyn, \"H\") {\n  field(PORT, \"SPFULL\")\n  field(TMOD, \"NoI/O\")\n  \
             field(PRIO, \"HIGH\")\n}\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    put_baud(&db, "L").await;
    until("L's first special applied", || {
        errs(&db, "L") == OPTION_ERROR
    })
    .await;

    let full = fill_low_band();
    put_baud(&db, "L").await;
    assert_eq!(errs(&db, "L"), "", "the put cleared ERRS");
    put_baud(&db, "H").await;
    until("H's special applied", || errs(&db, "H") == OPTION_ERROR).await;
    // The port thread ran L's special before H's, so L's result is out.
    drop(full);
    until("L's special result was lost to the full band", || {
        errs(&db, "L") == OPTION_ERROR
    })
    .await;
}
