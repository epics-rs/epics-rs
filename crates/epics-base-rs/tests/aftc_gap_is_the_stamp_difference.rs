//! AFTC smooths over the gap between the record's previous and new stamps
//! (`biRecord.c:134-150,259`). softIocPVX with this DB, P/PSIM seeded at state
//! 1 and processed again at state 0 after a pause, reads SEVR=MINOR for both;
//! a zero gap would leave them MAJOR.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

const DB: &str = r#"
record(longin, "PS") { }
record(bi, "P") { field(INP, "PS") field(AFTC, "2") field(OSV, "MAJOR") }
record(bi, "PSIM") { field(INP, "PS") field(SIOL, "PS") field(SIMM, "YES") field(AFTC, "2") field(OSV, "MAJOR") }
"#;

#[epics_macros_rs::epics_test]
async fn a_pause_reaches_the_filter_as_the_stamp_gap_birecord_259() {
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let put = |rec: &'static str, f: &'static str, v: EpicsValue| {
        let db = db.clone();
        async move {
            db.put_record_field_from_ca_no_notify(rec, f, v)
                .await
                .unwrap()
        }
    };

    put("PS", "VAL", EpicsValue::Long(1)).await;
    for rec in ["P", "PSIM"] {
        put(rec, "PROC", EpicsValue::Short(1)).await;
        assert_eq!(
            db.get_pv(&format!("{rec}.SEVR")).ok(),
            Some(EpicsValue::Short(2)),
            "{rec}"
        );
    }
    // alpha = 2 / (0.5 + 2) = 0.8: AFVL 1.6 floors to MINOR; a zero gap keeps 2.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    put("PS", "VAL", EpicsValue::Long(0)).await;
    for rec in ["P", "PSIM"] {
        put(rec, "PROC", EpicsValue::Short(1)).await;
        assert_eq!(
            db.get_pv(&format!("{rec}.SEVR")).ok(),
            Some(EpicsValue::Short(1)),
            "{rec}"
        );
    }
}
