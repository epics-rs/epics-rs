//! The over-run report of a periodic scan thread allocates nothing on it: C's
//! `periodicTask` prints it with `errlogPrintf` (`dbScan.c:830-841`), which
//! formats into the buffer errlog allocated at init. A slow host trips the
//! report in any allocation-counted window. Counted per thread name in the
//! alloc binary's allocator, across over-runs forced by holding TICK's write
//! lock past each deadline until the report fires.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use alloc_count::named::{self, Name};
use epics_base_rs::runtime::log::{
    errlog_add_listener, errlog_console_paints, errlog_flush, errlog_remove_listener,
    errlog_vprintf,
};
use epics_base_rs::server::ioc_builder::IocBuilder;

static SCAN_THREAD: [Name; 1] = [Name::Exact("scan-0.1")];

static REPORTED: AtomicBool = AtomicBool::new(false);

const DB: &str = r#"record(calc, "TICK") { field(SCAN, ".1 second") field(CALC, "VAL+1") }"#;

#[epics_macros_rs::epics_test]
async fn an_over_run_report_allocates_nothing_on_the_scan_thread() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let ticks = || db.get_pv("TICK").unwrap().to_f64().unwrap();
    // An IOC has printed through errlog long before a list over-runs: the
    // console and the `tracing` callsite exist, and the worker's listener
    // snapshot is sized.
    let listener = errlog_add_listener(|m| {
        if m.contains("Over-runs have now happened") {
            REPORTED.store(true, Ordering::SeqCst);
        }
    });
    errlog_console_paints();
    errlog_vprintf(format_args!("warm\n"));
    errlog_flush();

    let owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());
    let start = Instant::now();
    while ticks() < 5.0 {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "scan never ticked"
        );
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }

    let window = named::open(&SCAN_THREAD);
    // C reports the tenth over-run in a row once 10 s have passed since the
    // thread started (`dbScan.c:830`): hold every sweep past its deadline
    // until it does.
    let stop = Arc::new(AtomicBool::new(false));
    let holder = {
        let (stop, rec) = (stop.clone(), db.get_record("TICK").unwrap());
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let held = rec.write();
                std::thread::sleep(Duration::from_millis(150));
                drop(held);
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let start = Instant::now();
    while !REPORTED.load(Ordering::SeqCst) {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "no over-run report"
        );
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }
    stop.store(true, Ordering::SeqCst);
    holder.join().unwrap();
    // Two sweeps past the one that reported.
    let reported = ticks();
    while ticks() < reported + 2.0 {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }
    let [scan] = window.close();
    drop(owner);
    errlog_remove_listener(listener);

    assert_eq!(
        (scan.allocs, scan.frees),
        (0, 0),
        "allocations and frees on scan-0.1 across over-runs and their report"
    );
}
