//! A PRIO put moves an I/O Intr record to the scan list of its new band
//! (`special(SPC_SCAN)` re-adds it, dbAccess.c:127; `scanIoRequest`
//! requests one callback per band, dbScan.c:616-633), so its next wake runs
//! on that band whatever the old one is doing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::background::callback_executor::{set_parallel_threads, set_queue_size};
use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::runtime::task::{
    CallbackPriority, background_callback_stats, sleep, spawn_blocking_background,
};
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};
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

static WOKEN: DevSup = DevSup {
    name: "devAiWoken",
    choice: "Woken",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], true),
};

struct Woken {
    wakes: Arc<Mutex<Option<mpsc::Sender<()>>>>,
    reads: Arc<AtomicUsize>,
}

impl DeviceSupport for Woken {
    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        let (tx, rx) = mpsc::channel(4);
        *self.wakes.lock().unwrap() = Some(tx);
        Some(rx)
    }

    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "Woken"
    }
}

async fn until(what: &str, fact: impl Fn() -> bool) {
    let deadline = epics_base_rs::runtime::time::deadline_from_now(fact_budget::FACT_BUDGET);
    while !fact() {
        assert!(Instant::now() < deadline, "{what}");
        sleep(Duration::from_millis(5)).await;
    }
}

/// `R` joins I/O Intr at PRIO LOW, takes a PRIO HIGH put, then a wake while
/// the low band is full.
#[epics_base_rs::epics_test]
async fn a_wake_after_a_prio_put_runs_on_the_new_band() {
    // `callbackSetQueueSize` and `callbackParallelThreads` before `callbackInit`.
    set_queue_size(1);
    set_parallel_threads(1, Some(CallbackPriority::Low));
    let wakes = Arc::new(Mutex::new(None));
    let reads = Arc::new(AtomicUsize::new(0));
    let (made_wakes, made_reads) = (wakes.clone(), reads.clone());
    let (db, _) = IocBuilder::new()
        .register_device_support("ai", &WOKEN, move || {
            Box::new(Woken {
                wakes: made_wakes.clone(),
                reads: made_reads.clone(),
            })
        })
        .db_string(
            r#"record(ai, "R") { field(DTYP, "Woken") field(INP, "@a") field(SCAN, "I/O Intr") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();
    let wake = wakes
        .lock()
        .unwrap()
        .clone()
        .expect("R asked for its receiver");

    wake.send(()).await.unwrap();
    until("R's first wake was never served", || {
        reads.load(Ordering::Relaxed) == 1
    })
    .await;

    db.put_record_field_from_ca_no_notify("R", "PRIO", EpicsValue::String("HIGH".into()))
        .await
        .unwrap();
    let _full = fill_low_band();
    wake.send(()).await.unwrap();
    until(
        "R's wake after the PRIO put waited on the full low band",
        || reads.load(Ordering::Relaxed) == 2,
    )
    .await;
}
