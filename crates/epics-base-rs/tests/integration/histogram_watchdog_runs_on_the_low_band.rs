//! C `wdogInit` gives the histogram's SDEL watchdog `priorityLow`
//! (`histogramRecord.c:141`), not the record's PRIO, so a busy PRIO band
//! does not hold back the post the watchdog owes a display.

use std::collections::HashMap;

use epics_base_rs::runtime::background::callback_executor::set_parallel_threads;
use epics_base_rs::runtime::task::{CallbackPriority, spawn_blocking_background};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

#[epics_base_rs::epics_test]
async fn a_prio_high_watchdog_posts_while_the_high_band_is_busy() {
    // `callbackParallelThreads` before `callbackInit`.
    set_parallel_threads(1, Some(CallbackPriority::High));
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(histogram, "H") { field(PRIO, "HIGH") field(NELM, "2") field(ULIM, "10") field(MDEL, "1000") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let rec = db.get_record("H").unwrap();
    let mut val_rx = rec
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Long, EventMask::VALUE.bits())
        .expect("VAL subscription accepted");
    // One count, under MDEL: only the watchdog posts it.
    db.put_record_field_from_ca("H", "SGNL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    while val_rx.try_recv().is_ok() {}

    let (running_tx, running_rx) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel::<()>();
    drop(spawn_blocking_background(
        CallbackPriority::High,
        move |_| {
            running_tx.send(()).unwrap();
            let _ = held.recv();
        },
    ));
    running_rx.recv_timeout(fact_budget::FACT_BUDGET).unwrap();

    db.put_record_field_from_ca("H", "SDEL", EpicsValue::Double(0.05))
        .await
        .unwrap();
    let posted = epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, val_rx.recv())
        .await
        .ok()
        .flatten()
        .is_some();
    release.send(()).unwrap();
    assert!(posted, "the watchdog waited for the busy HIGH band");
    assert_eq!(
        rec.read().record.get_field("MCNT"),
        Some(EpicsValue::Short(0))
    );
}
