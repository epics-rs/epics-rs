//! `eventRecord.c::process` calls `postEvent(prec->epvt)` and posts VAL with
//! `monitor_mask|DBE_VALUE` on every process, whether or not VAL moved.
//! softIoc R7.0.10.1, a `.PROC` put twice on each of E1/E2/E3:
//!
//! ```text
//! E1 "abc", E2 "7", E3 "1"   proc1 -> CA 1 C7 1 C1 1   proc2 -> CA 2 C7 2 C1 2
//! camonitor E1 E2 E3          3 updates each (connect + one per process)
//! ```

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(event, "E1") { field(VAL, "abc") }
record(event, "E2") { field(VAL, "7") }
record(event, "E3") { field(VAL, "1") }
record(calc, "CA") { field(SCAN, "Event") field(EVNT, "abc") field(CALC, "VAL+1") }
record(calc, "C7") { field(SCAN, "Event") field(EVNT, "7") field(CALC, "VAL+1") }
record(calc, "C1") { field(SCAN, "Event") field(EVNT, "1") field(CALC, "VAL+1") }
"#;

const EVENTS: [&str; 3] = ["E1", "E2", "E3"];

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

fn value_posts(rx: &mut EventReader) -> usize {
    let mut n = 0;
    while let Ok(ev) = rx.try_recv() {
        if ev.mask.contains(EventMask::VALUE) {
            n += 1;
        }
    }
    n
}

/// The post is spawned, so poll until the counters reach `want`, then hold a
/// settle window so a spurious extra post would land before the assert.
async fn counters(db: &PvDatabase, want: [f64; 3]) {
    let read = || ["CA", "C7", "C1"].map(|r| num(db, r));
    for _ in 0..400 {
        if read() == want {
            break;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
    }
    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(read(), want, "CA/C7/C1");
}

#[epics_macros_rs::epics_test]
async fn event_reposts_an_unchanged_name() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let mut rx = EVENTS.map(|name| {
        db.get_record(name)
            .unwrap()
            .write()
            .add_subscriber("VAL", 1, DbFieldType::String, EventMask::VALUE.bits())
            .expect("subscription accepted")
    });
    rx.iter_mut().for_each(|r| {
        value_posts(r);
    });
    counters(&db, [0.0; 3]).await;
    for n in [1.0, 2.0] {
        for rec in EVENTS {
            db.put_record_field_from_ca(rec, "PROC", EpicsValue::Short(1))
                .await
                .unwrap();
        }
        counters(&db, [n; 3]).await;
        for (name, r) in EVENTS.iter().zip(rx.iter_mut()) {
            assert_eq!(value_posts(r), 1, "{name} VAL post on proc {n}");
        }
    }
}
