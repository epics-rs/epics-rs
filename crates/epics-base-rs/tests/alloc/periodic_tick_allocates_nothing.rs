//! A periodic scan thread allocates nothing per tick once warm: C's
//! `periodicTask` waits on an `epicsEvent` it created once (`dbScan.c:795-818`)
//! and its records post into free-list memory. An allocation on the scan
//! thread is a malloc in the realtime path, which is what widens its period
//! error. Counted by thread name in the alloc binary's allocator.
//!
//! Thread backend only: there the tick is driven by `park_on`, whose waker is
//! the per-tick allocation this pins.

#![cfg(all(exec_backend, target_os = "linux"))]

use std::collections::HashMap;
use std::time::Duration;

use alloc_count::named::{self, Name};
use epics_base_rs::server::ioc_builder::IocBuilder;

/// The 10 Hz scan thread.
static SCAN_THREAD: [Name; 1] = [Name::Exact("scan-0.1")];

const DB: &str = r#"
record(calc, "TICK") { field(SCAN, ".1 second") field(CALC, "VAL+1") }
record(ao, "OUT") { field(SCAN, ".1 second") field(VAL, "1") }
"#;

/// Ticks seen from the record's own counter; the warm-up is an event, not a
/// duration: under load the owner thread can start the rate thread late, and
/// its first tick (the thread's waker, a record's first-process caches) would
/// otherwise land in the counted window.
async fn warm(db: &epics_base_rs::server::database::PvDatabase) {
    let ticks = || db.get_pv("TICK").unwrap().to_f64().unwrap();
    let start = std::time::Instant::now();
    while ticks() < 5.0 {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "scan never ticked"
        );
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }
}

#[epics_macros_rs::epics_test]
async fn ticks_of_a_periodic_scan_allocate_and_free_nothing() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let ticks = || db.get_pv("TICK").unwrap().to_f64().unwrap();
    let owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());
    warm(&db).await;

    let before = ticks();
    let window = named::open(&SCAN_THREAD);
    epics_base_rs::runtime::task::sleep(Duration::from_millis(2000)).await;
    let [scan] = window.close();
    let ran = ticks() - before;
    drop(owner);

    assert!(ran >= 10.0, "only {ran} ticks ran in the counted window");
    assert_eq!(
        (scan.allocs, scan.frees),
        (0, 0),
        "allocations and frees on scan-0.1 over {ran} ticks"
    );
}

/// A tick late by more than one period — the over-run bookkeeping
/// (`OverrunTracker::after_scan`, `record_scan_overrun`) runs — still
/// allocates nothing. Lateness is forced by holding TICK's write lock across
/// 2.5 periods, so the scan thread blocks inside the pass.
#[epics_macros_rs::epics_test]
async fn a_tick_late_by_more_than_a_period_allocates_nothing() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let ticks = || db.get_pv("TICK").unwrap().to_f64().unwrap();
    let owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());
    warm(&db).await;

    let window = named::open(&SCAN_THREAD);
    {
        let rec = db.get_record("TICK").unwrap();
        let _held = rec.write();
        std::thread::sleep(Duration::from_millis(250));
    }
    let before = ticks();
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1000)).await;
    let [scan] = window.close();
    let ran = ticks() - before;
    drop(owner);

    assert!(ran >= 5.0, "only {ran} ticks ran after the late one");
    assert_eq!(
        (scan.allocs, scan.frees),
        (0, 0),
        "allocations and frees on scan-0.1 across a late tick and {ran} ticks"
    );
}
