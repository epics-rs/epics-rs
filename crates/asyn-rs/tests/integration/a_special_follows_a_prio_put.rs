//! C reads `pasynRec->prio` on every `callbackRequestProcessCallback`
//! (asynRecord.c:403, :831, :926), so a PRIO put moves the record's next
//! callback without another cycle in between.

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

async fn until(what: &str, fact: impl Fn() -> bool) {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while !fact() {
        assert!(Instant::now() < deadline, "{what}");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

/// `A` runs a cycle at PRIO LOW, then takes a PRIO HIGH put and, with no
/// cycle since, a special while the low band is full.
#[epics_base_rs::epics_test]
async fn a_special_lands_on_the_band_a_prio_put_chose() {
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
    let driver = DrvAsynIPPort::new("SPPRIO", &addr.to_string()).unwrap();
    let (rt, _jh) = create_port_runtime_unclaimed(driver, RuntimeConfig::default()).unwrap();
    register_port(
        "SPPRIO",
        rt.port_handle().clone(),
        &asyn_rs::registry::open_owner(),
    )
    .unwrap();
    let (db, _) = IocBuilder::new()
        .register_record_type("asyn", || Box::new(AsynRecord::default()))
        .db_string(
            "record(asyn, \"A\") {\n  field(PORT, \"SPPRIO\")\n  field(TMOD, \"NoI/O\")\n}\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db.put_record_field_from_ca_no_notify("A", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    until("A's cycle ended", || {
        !db.get_record("A").unwrap().read().is_processing()
    })
    .await;
    db.put_record_field_from_ca_no_notify("A", "PRIO", EpicsValue::String("HIGH".into()))
        .await
        .unwrap();

    let _full = fill_low_band();
    db.put_record_field_from_ca_no_notify("A", "BAUD", EpicsValue::String("9600".into()))
        .await
        .unwrap();
    until("A's special result waited on the full low band", || {
        errs(&db, "A") == OPTION_ERROR
    })
    .await;
}
