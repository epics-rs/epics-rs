//! calcout posts `INAV..INUV`/`OUTV` only from `checkLinks` and `special`
//! (`calcoutRecord.c:404`, `:752`, `:757`); `monitor()` never names them.
//! softIocPVX (R7.0.10) with the DB below, `pvxmonitor C.INAV C.INBV C.OUTV`
//! through `PROC`, `INPA NOPE2`, `PROC`, `INPA SRC`, `OUT NOPE3`, `OUT ""`,
//! `OUT SINK`, `PROC`: no `PROC` put delivers an update on any of the three.
//!
//! So a status value the record holds but has not posted stays unposted by a
//! process: the monitor walk must not change-detect these fields.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use std::collections::HashMap;
use std::time::Duration;

const DB: &str = r#"
record(ai, "SRC") { field(VAL, "1") }
record(ai, "SINK") { }
record(calcout, "C") { field(CALC, "A+1") field(INPA, "SRC") field(INPB, "NOPE") field(OUT, "SINK") }
"#;

async fn settle() {
    epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
}

#[epics_macros_rs::epics_test]
async fn a_process_posts_no_link_status_field_the_record_holds() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    settle().await;

    let fields = ["INAV", "INBV", "OUTV"];
    let rec = db.get_record("C").unwrap();
    let mut readers: Vec<_> = fields
        .into_iter()
        .enumerate()
        .map(|(sid, field)| {
            let mask = (EventMask::VALUE | EventMask::ALARM | EventMask::LOG).bits();
            let reader = rec
                .write()
                .add_subscriber(field, sid as u32, DbFieldType::Enum, mask)
                .unwrap();
            (field, reader)
        })
        .collect();

    // A status the record holds without having posted it: 3 is "Constant".
    for field in fields {
        rec.write()
            .record
            .put_field(field, EpicsValue::Enum(3))
            .unwrap();
    }
    for _ in 0..2 {
        db.put_record_field_from_ca_no_notify("C", "PROC", EpicsValue::Short(1))
            .await
            .unwrap();
    }
    settle().await;

    for (field, reader) in &mut readers {
        assert!(reader.try_recv().is_err(), "C.{field} posted by a process");
    }
}
