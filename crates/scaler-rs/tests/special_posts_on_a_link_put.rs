//! `special()`'s posts on a DB-link put, not only a CA one.
//!
//! C oracleScalerIoc (epics-base 8f5015b66, scaler 913e2e4), scaler `S` with
//! `FREQ 1e7 TP 1` on drvScalerSoft, `ao W1 OUT "S.TP"`, `longout W2 OUT
//! "S.PR2"`, `longout W3 OUT "S.G3"`, `caput W1 2`, `caput W2 500`,
//! `caput W3 1`; after the connect values, `camonitor -m v`:
//!
//! ```text
//! S.PR1    20000000
//! S.D1     Dn
//! S.G1     Y
//! S.D2     Dn
//! S.G2     Y
//! S.PR3    1000
//! ```
//!
//! and `camonitor -m l` nothing.

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — passes on the exec backend
// (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p scaler-rs
// --all-features --test special_posts_on_a_link_put`); no CA server is built.

use std::collections::HashMap;

use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const FIELDS: [(&str, DbFieldType); 6] = [
    ("PR1", DbFieldType::ULong),
    ("D1", DbFieldType::Short),
    ("G1", DbFieldType::Short),
    ("D2", DbFieldType::Short),
    ("G2", DbFieldType::Short),
    ("PR3", DbFieldType::ULong),
];

fn drain(rx: &mut EventReader) -> Vec<f64> {
    let mut got = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        got.push(ev.snapshot.value.to_f64().unwrap());
    }
    got
}

#[tokio::test]
async fn a_link_put_posts_what_special_posts() {
    let (name, factory) = scaler_rs::scaler_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .db_string(
            r#"
record(scaler, "S") { field(FREQ, "10000000") field(TP, "1") }
record(ao, "W1") { field(OUT, "S.TP") }
record(longout, "W2") { field(OUT, "S.PR2") }
record(longout, "W3") { field(OUT, "S.G3") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut subs = Vec::new();
    {
        let rec = db.get_record("S").unwrap();
        let mut inst = rec.write();
        for (i, (field, dbf)) in FIELDS.into_iter().enumerate() {
            for (j, mask) in [EventMask::VALUE, EventMask::LOG].into_iter().enumerate() {
                let sid = (2 * i + j) as u32 + 1;
                let rx = inst.add_subscriber(field, sid, dbf, mask.bits()).unwrap();
                subs.push((field, mask, rx));
            }
        }
    }
    for (_, _, rx) in &mut subs {
        drain(rx);
    }

    db.put_record_field_from_ca("W1", "VAL", EpicsValue::Double(2.0))
        .await
        .unwrap();
    db.put_record_field_from_ca("W2", "VAL", EpicsValue::Long(500))
        .await
        .unwrap();
    db.put_record_field_from_ca("W3", "VAL", EpicsValue::Long(1))
        .await
        .unwrap();

    let got: Vec<(&str, &str, Vec<f64>)> = subs
        .iter_mut()
        .map(|(field, mask, rx)| {
            let m = if *mask == EventMask::VALUE { "v" } else { "l" };
            (*field, m, drain(rx))
        })
        .collect();
    assert_eq!(
        got,
        [
            ("PR1", "v", vec![2.0e7]),
            ("PR1", "l", vec![]),
            ("D1", "v", vec![1.0]),
            ("D1", "l", vec![]),
            ("G1", "v", vec![1.0]),
            ("G1", "l", vec![]),
            ("D2", "v", vec![1.0]),
            ("D2", "l", vec![]),
            ("G2", "v", vec![1.0]),
            ("G2", "l", vec![]),
            ("PR3", "v", vec![1000.0]),
            ("PR3", "l", vec![]),
        ]
    );
}
