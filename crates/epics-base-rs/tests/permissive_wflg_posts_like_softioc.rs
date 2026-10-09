//! permissive `monitor()` posts WFLG on its own `oflg != wflg`
//! (permissiveRecord.c:111-114), so a WFLG put that processes the record is
//! posted twice: by `dbPut`, then by `monitor()`. Events per put, counted on
//! a softIoc R7.0.10 camonitor of P.WFLG and P.VAL.

use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use std::collections::HashMap;

fn drain(rx: &mut EventReader) -> usize {
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

async fn run(record: &str, steps: &[(&str, u16, [usize; 2])]) {
    let db = IocBuilder::new()
        .db_string(record, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let mut rxs: Vec<EventReader> = ["WFLG", "VAL"]
        .iter()
        .enumerate()
        .map(|(sid, f)| {
            let inst = db.get_record("P").unwrap();
            let mut g = inst.write();
            g.add_subscriber(
                f,
                sid as u32 + 1,
                DbFieldType::UShort,
                (EventMask::VALUE | EventMask::LOG).bits(),
            )
            .unwrap()
        })
        .collect();
    for rx in &mut rxs {
        drain(rx);
    }
    for &(field, value, want) in steps {
        let v = if field == "PROC" {
            EpicsValue::Short(value as i16)
        } else {
            EpicsValue::UShort(value)
        };
        db.put_record_field_from_ca("P", field, v).await.unwrap();
        let got: Vec<usize> = rxs.iter_mut().map(drain).collect();
        assert_eq!(got, want, "[WFLG, VAL] events after {field} {value}");
    }
}

#[epics_macros_rs::epics_test]
async fn permissive_wflg_event_counts_match_softioc() {
    let steps = [
        ("WFLG", 1, [2, 0]),
        ("PROC", 1, [0, 0]),
        ("WFLG", 1, [1, 0]),
        ("VAL", 1, [0, 1]),
        ("WFLG", 0, [2, 0]),
        ("PROC", 1, [0, 0]),
    ];
    run(r#"record(permissive, "P") {}"#, &steps).await;
}

/// A WFLG put on a record it does not process is posted by `dbPut` alone;
/// the `oflg != wflg` post waits for the next `monitor()`.
#[epics_macros_rs::epics_test]
async fn permissive_unprocessed_wflg_put_posts_on_the_next_cycle() {
    let steps = [
        ("WFLG", 1, [1, 0]),
        ("PROC", 1, [1, 0]),
        ("PROC", 1, [0, 0]),
    ];
    run(
        r#"record(permissive, "P") { field(SCAN, "Event") field(EVNT, "77") }"#,
        &steps,
    )
    .await;
}
