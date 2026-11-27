//! A link put posts that link's status field alone, against oracleStdIoc
//! (calc R2-x, x86-64 softIoc R7.0.10) under `camonitor -m va` of three
//! status fields per record:
//!
//! ```text
//! caput CO.INPA ""   CO.INAV  Constant
//! caput CO.INPA T    CO.INAV  Constant
//! caput SC.INPA ""   SC.INAV  Constant
//! caput SC.INPA T    SC.INAV  Local PV
//! caput TR.INPA ""   TR.IAV   Constant
//! caput TR.INPA T    TR.IAV   Local PV
//! caput AC.INAA ""   AC.IAAV  Constant
//! caput AC.INAA T    AC.IAAV  Local PV
//! ```
//!
//! `special()` posts the re-pointed link's status even when it did not move
//! (`aCalcoutRecord.c:569`, `sCalcoutRecord.c:569`, `transformRecord.c:741`)
//! and no other status field.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(ao, "T") {}
record(calcout, "CO") { field(CALC, "A") }
record(scalcout, "SC") { field(CALC, "A") }
record(transform, "TR") {}
record(acalcout, "AC") { field(NELM, "2") field(CALC, "A") }
"#;

const WATCHED: [(&str, [&str; 3]); 4] = [
    ("CO", ["INAV", "INBV", "OUTV"]),
    ("SC", ["INAV", "INBV", "OUTV"]),
    ("TR", ["IAV", "IBV", "OAV"]),
    ("AC", ["INAV", "IAAV", "OUTV"]),
];

fn drain(rx: &mut EventReader) -> Vec<EpicsValue> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push(e.value().as_value().cloned().unwrap());
    }
    out
}

#[epics_macros_rs::epics_test]
async fn a_link_put_posts_only_its_own_status() {
    let db = IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::new()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    let mut rxs: Vec<((&str, &str), EventReader)> = Vec::new();
    for (rec, fields) in WATCHED {
        let inst = db.get_record(rec).unwrap();
        let mut g = inst.write();
        for (sid, f) in fields.iter().enumerate() {
            let rx = g
                .add_subscriber(
                    f,
                    sid as u32 + 1,
                    DbFieldType::Enum,
                    EventMask::VALUE.bits(),
                )
                .unwrap();
            rxs.push(((rec, f), rx));
        }
    }
    for (_, rx) in &mut rxs {
        drain(rx);
    }
    const LOC: u16 = 2;
    const CON: u16 = 3;
    let steps: [(&str, &str, &str, &str, u16); 8] = [
        ("CO", "INPA", "", "INAV", CON),
        ("CO", "INPA", "T", "INAV", CON),
        ("SC", "INPA", "", "INAV", CON),
        ("SC", "INPA", "T", "INAV", LOC),
        ("TR", "INPA", "", "IAV", CON),
        ("TR", "INPA", "T", "IAV", LOC),
        ("AC", "INAA", "", "IAAV", CON),
        ("AC", "INAA", "T", "IAAV", LOC),
    ];
    for (rec, field, link, posted, status) in steps {
        let posted = (rec, posted);
        db.put_record_field_from_ca(rec, field, EpicsValue::String(link.into()))
            .await
            .unwrap();
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
        for (watched, rx) in &mut rxs {
            let want = if *watched == posted {
                vec![EpicsValue::Enum(status)]
            } else {
                vec![]
            };
            assert_eq!(
                drain(rx),
                want,
                "{}.{} after {rec}.{field} = {link:?}",
                watched.0,
                watched.1
            );
        }
    }
}
