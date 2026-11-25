//! Spec §5 test 14: an NPP OUT put into SCAN goes through `special()`, so
//! the target joins or leaves its scan list at once (`dbAccess.c:127-131`).
//! A bad menu string is refused (`cvt_st_menu`) and alarms the source; an
//! out-of-menu index is stored and the put is not refused (`scanAdd` only
//! complains). Pinned to oracleStdIoc (C ground truth §5.14, its DB
//! verbatim); scan-list membership stands in for `scanppl`.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::ScanType;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(calc,"X"){field(CALC,"VAL+1")}
record(calc,"Y"){field(CALC,"VAL+1")}
record(stringout,"SS"){field(OUT,"X.SCAN NPP")}
record(longout,"SL"){field(OUT,"Y.SCAN NPP")}
"#;

const PASSIVE: f64 = 0.0;
const SEC1: f64 = 6.0;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// `(STAT, SEVR)`.
fn alarm(db: &PvDatabase, rec: &str) -> (f64, f64) {
    (
        num(db, &format!("{rec}.STAT")),
        num(db, &format!("{rec}.SEVR")),
    )
}

async fn in_sec1(db: &PvDatabase, rec: &str) -> bool {
    db.records_for_scan(ScanType::SEC1)
        .await
        .iter()
        .any(|r| r == rec)
}

#[epics_macros_rs::epics_test]
async fn a_scan_put_through_a_link_moves_the_target_between_scan_lists() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut x_scan = {
        let r = db.get_record("X").unwrap();
        let mut g = r.write();
        let mask = (EventMask::VALUE | EventMask::ALARM).bits();
        g.add_subscriber("SCAN", 1, DbFieldType::Enum, mask)
            .unwrap()
    };
    while x_scan.try_recv().is_ok() {}

    db.put_record_field_from_ca("SS", "VAL", EpicsValue::String("1 second".into()))
        .await
        .unwrap();
    assert_eq!(num(&db, "X.SCAN"), SEC1);
    assert!(in_sec1(&db, "X").await);
    assert!(x_scan.try_recv().is_ok(), "X.SCAN posted");
    assert_eq!(alarm(&db, "SS"), (0.0, 0.0));

    db.put_record_field_from_ca("SS", "VAL", EpicsValue::String("Passive".into()))
        .await
        .unwrap();
    assert_eq!(num(&db, "X.SCAN"), PASSIVE);
    assert!(!in_sec1(&db, "X").await);
    assert!(x_scan.try_recv().is_ok(), "X.SCAN posted");

    let _ = db
        .put_record_field_from_ca("SS", "VAL", EpicsValue::String("bogus".into()))
        .await;
    assert_eq!(num(&db, "X.SCAN"), PASSIVE);
    assert!(
        x_scan.try_recv().is_err(),
        "a refused SCAN put posts nothing"
    );
    assert_eq!(alarm(&db, "SS"), (14.0, 3.0));

    db.put_record_field_from_ca("SL", "VAL", EpicsValue::Long(6))
        .await
        .unwrap();
    assert_eq!(num(&db, "Y.SCAN"), SEC1);
    assert!(in_sec1(&db, "Y").await);
    assert_eq!(alarm(&db, "SL"), (0.0, 0.0));

    db.put_record_field_from_ca("SL", "VAL", EpicsValue::Long(0))
        .await
        .unwrap();
    assert_eq!(num(&db, "Y.SCAN"), PASSIVE);
    assert!(!in_sec1(&db, "Y").await);

    db.put_record_field_from_ca("SL", "VAL", EpicsValue::Long(99))
        .await
        .unwrap();
    assert_eq!(num(&db, "Y.SCAN"), 99.0);
    assert!(!in_sec1(&db, "Y").await);
    assert_eq!(alarm(&db, "SL"), (0.0, 0.0));
}
