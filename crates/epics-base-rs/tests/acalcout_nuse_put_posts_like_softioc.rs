//! A NUSE put into an aCalcout, against oracleStdIoc (calc R2-x, x86-64
//! softIoc R7.0.10) under `camonitor -m va AC.AVAL AC.NUSE AC.AA`:
//!
//! ```text
//! caput AC.NUSE 2   AC.NUSE 2, then AC.AVAL [1,1] (the put processes)
//! caput AC.NUSE 9   AC.NUSE 4 (refused, no process)
//! caput SRC 3       AC.NUSE 3 (DB link, no PP)
//! caput SRC 9       AC.NUSE 4
//! caput SRCPP 2     AC.NUSE 2 (AVAL unchanged, no AVAL post)
//! caput SRCPP 9     AC.NUSE 4
//! ```
//!
//! `special(NUSE)` posts NUSE alone (`aCalcoutRecord.c:495-499`); AVAL and AA
//! post only from a process that moves them.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(acalcout, "AC") { field(NELM, "4") field(CALC, "AA+1") }
record(ao, "SRC") { field(OUT, "AC.NUSE") }
record(ao, "SRCPP") { field(OUT, "AC.NUSE PP") }
"#;

async fn build() -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::new()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn subscribe(db: &PvDatabase, field: &str, sid: u32) -> EventReader {
    let inst = db.get_record("AC").unwrap();
    let mut g = inst.write();
    g.add_subscriber(
        field,
        sid,
        DbFieldType::Double,
        (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
    )
    .unwrap()
}

fn drain(rx: &mut EventReader) -> Vec<EpicsValue> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push(e.snapshot.value.clone());
    }
    out
}

#[epics_macros_rs::epics_test]
async fn a_nuse_put_posts_nuse_alone() {
    let db = build().await;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    let mut rxs = [
        subscribe(&db, "NUSE", 1),
        subscribe(&db, "AVAL", 2),
        subscribe(&db, "AA", 3),
    ];
    for rx in &mut rxs {
        drain(rx);
    }
    let d = |v: u32| vec![EpicsValue::ULong(v)];
    let none = Vec::<EpicsValue>::new;
    let steps: [(&str, &str, f64, [Vec<EpicsValue>; 3]); 6] = [
        (
            "AC",
            "NUSE",
            2.0,
            [
                d(2),
                vec![EpicsValue::DoubleArray(vec![1.0, 1.0].into())],
                none(),
            ],
        ),
        ("AC", "NUSE", 9.0, [d(4), none(), none()]),
        ("SRC", "VAL", 3.0, [d(3), none(), none()]),
        ("SRC", "VAL", 9.0, [d(4), none(), none()]),
        ("SRCPP", "VAL", 2.0, [d(2), none(), none()]),
        ("SRCPP", "VAL", 9.0, [d(4), none(), none()]),
    ];
    for (rec, field, v, want) in steps {
        let _ = db
            .put_record_field_from_ca_no_notify(rec, field, EpicsValue::Double(v))
            .await;
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
        for (rx, (f, want)) in rxs.iter_mut().zip(["NUSE", "AVAL", "AA"].iter().zip(want)) {
            assert_eq!(drain(rx), want, "AC.{f} after {rec}.{field}={v}");
        }
    }
}

/// The same record under `caput -c` (put-notify), oracleStdIoc:
///
/// ```text
/// caput -c AC.NUSE 2   AC.NUSE 2, AC.AVAL [1,1]
/// caput -c AC.NUSE 9   AC.NUSE 4, AC.AVAL [1,1,1,1] (refused, still processed)
/// caput -c AC.NUSE 3   AC.NUSE 3 (AVAL's NUSE window unchanged)
/// caput -c SRC 2       AC.NUSE 2
/// caput -c SRC 9       AC.NUSE 4
/// ```
#[epics_macros_rs::epics_test]
async fn a_put_notify_of_nuse_posts_aval_only_from_its_process() {
    let db = build().await;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    let mut rxs = [
        subscribe(&db, "NUSE", 1),
        subscribe(&db, "AVAL", 2),
        subscribe(&db, "AA", 3),
    ];
    for rx in &mut rxs {
        drain(rx);
    }
    let d = |v: u32| vec![EpicsValue::ULong(v)];
    let a = |n: usize| vec![EpicsValue::DoubleArray(vec![1.0; n].into())];
    let none = Vec::<EpicsValue>::new;
    let steps: [(&str, &str, f64, [Vec<EpicsValue>; 3]); 5] = [
        ("AC", "NUSE", 2.0, [d(2), a(2), none()]),
        ("AC", "NUSE", 9.0, [d(4), a(4), none()]),
        ("AC", "NUSE", 3.0, [d(3), none(), none()]),
        ("SRC", "VAL", 2.0, [d(2), none(), none()]),
        ("SRC", "VAL", 9.0, [d(4), none(), none()]),
    ];
    for (rec, field, v, want) in steps {
        let _ = db
            .put_record_field_from_ca(rec, field, EpicsValue::Double(v))
            .await;
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
        for (rx, (f, want)) in rxs.iter_mut().zip(["NUSE", "AVAL", "AA"].iter().zip(want)) {
            assert_eq!(drain(rx), want, "AC.{f} after {rec}.{field}={v}");
        }
    }
}

/// `monitor()` posts AVAL against PAVL with MDEL/ADEL per element, and only a
/// post advances PAVL. oracleStdIoc, `NELM 2 CALC "A" MDEL 1 ADEL 1`,
/// `caput AD.A` 0.5, 2, 2.5, 3.5, 3.5: AVAL posts [2,2] and [3.5,3.5] only.
#[epics_macros_rs::epics_test]
async fn aval_posts_past_mdel_against_the_last_posted_copy() {
    let db = IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::new()))
        .db_string(
            r#"record(acalcout, "AD") { field(NELM, "2") field(CALC, "A") field(MDEL, "1") field(ADEL, "1") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    let inst = db.get_record("AD").unwrap();
    let mut rx = inst
        .write()
        .add_subscriber(
            "AVAL",
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
        )
        .unwrap();
    drain(&mut rx);
    let a = |v: f64| vec![EpicsValue::DoubleArray(vec![v; 2].into())];
    for (v, want) in [
        (0.5, vec![]),
        (2.0, a(2.0)),
        (2.5, vec![]),
        (3.5, a(3.5)),
        (3.5, vec![]),
    ] {
        db.put_record_field_from_ca_no_notify("AD", "A", EpicsValue::Double(v))
            .await
            .unwrap();
        epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
        assert_eq!(drain(&mut rx), want, "AD.AVAL after A={v}");
    }
}

/// oracleStdIoc, `DV` with MDEL 1 ADEL 10 and `DL` with MDEL 10 ADEL 1 (both
/// NELM 2, CALC "A", processed once), under `camonitor -m v` and `-m l`:
/// `caput A 2` posts DV.AVAL on `-m v` only and DL.AVAL on `-m l` only;
/// `caput A 12` posts both on both. C `monitor()` adds each class only for
/// its own deadband crossing (`aCalcoutRecord.c:996-1000`).
#[epics_macros_rs::epics_test]
async fn aval_one_sided_deadband_posts_one_class() {
    let db = IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::new()))
        .db_string(
            r#"
record(acalcout, "DV") { field(NELM, "2") field(CALC, "A") field(MDEL, "1") field(ADEL, "10") }
record(acalcout, "DL") { field(NELM, "2") field(CALC, "A") field(MDEL, "10") field(ADEL, "1") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    let mut rxs = Vec::new();
    for rec in ["DV", "DL"] {
        db.put_record_field_from_ca_no_notify(rec, "PROC", EpicsValue::Short(1))
            .await
            .unwrap();
        let inst = db.get_record(rec).unwrap();
        let mut g = inst.write();
        for (sid, mask) in [(1, EventMask::VALUE), (2, EventMask::LOG)] {
            rxs.push(
                g.add_subscriber("AVAL", sid, DbFieldType::Double, mask.bits())
                    .unwrap(),
            );
        }
    }
    epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
    for rx in &mut rxs {
        drain(rx);
    }
    let a = |v: f64| vec![EpicsValue::DoubleArray(vec![v; 2].into())];
    // DV value, DV log, DL value, DL log.
    for (v, want) in [
        (2.0, [a(2.0), vec![], vec![], a(2.0)]),
        (12.0, [a(12.0), a(12.0), a(12.0), a(12.0)]),
    ] {
        for rec in ["DV", "DL"] {
            db.put_record_field_from_ca_no_notify(rec, "A", EpicsValue::Double(v))
                .await
                .unwrap();
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
        let got: Vec<Vec<EpicsValue>> = rxs.iter_mut().map(drain).collect();
        assert_eq!(got, want, "DV/DL AVAL value/log posts after A={v}");
    }
}
