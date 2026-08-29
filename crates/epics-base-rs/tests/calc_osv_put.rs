//! scalcout `OSV` is a `DBF_STRING` ("Output string value"), not a severity
//! menu — a client put of any string is ACCEPTED and stored, matching C.
//!
//! The port's `shared_menu_choices` name-based map lists `OSV` as
//! `menuAlarmSevr` (correct for the bi/bo severity field of the same name).
//! That heuristic wrongly bit scalcout's string `OSV`, so `caput SCALCOUT.OSV
//! ''` was rejected with `S_db_badChoice` where C accepts it (oracle: OSV
//! put_accepted C=true, port=false, 6 put classes). The fix makes the declared
//! `DBF_STRING` type win over the name-based menu.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"record(scalcout, "S") {}"#;

/// Every put class the oracle exercised on `OSV` is accepted and the string is
/// stored — the field is a plain `DBF_STRING`, never resolved against a menu.
#[epics_macros_rs::epics_test]
async fn scalcout_osv_accepts_string_and_numeric_puts() {
    let db = IocBuilder::new()
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    // A non-severity string — under the old name-based menu this was
    // `S_db_badChoice` ('hi' is not NO_ALARM/MINOR/MAJOR/INVALID).
    db.put_record_field_from_ca("S", "OSV", EpicsValue::String("hi".into()))
        .await
        .expect("caput OSV 'hi' must be accepted (DBF_STRING, not a menu)");
    assert_eq!(db.get_pv("S.OSV").unwrap(), EpicsValue::String("hi".into()));

    // The empty string — the exact oracle repro — must also stand.
    db.put_record_field_from_ca("S", "OSV", EpicsValue::String("".into()))
        .await
        .expect("caput OSV '' must be accepted");
    assert_eq!(db.get_pv("S.OSV").unwrap(), EpicsValue::String("".into()));

    // A numeric put converts to its string form (C `dbFastPutConvert`
    // DBR_DOUBLE→DBF_STRING), also accepted.
    db.put_record_field_from_ca("S", "OSV", EpicsValue::Double(1.5))
        .await
        .expect("caput OSV 1.5 must be accepted");
    assert_eq!(
        db.get_pv("S.OSV").unwrap(),
        EpicsValue::String("1.5".into())
    );
}
