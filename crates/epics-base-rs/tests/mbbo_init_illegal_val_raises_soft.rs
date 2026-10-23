//! mbbo `init_record` ends with `convert(prec)` (`mbboRecord.c:177`), which
//! raises SOFT/INVALID into NSEV for a VAL over 15 with a state table
//! (`:428-431`). The alarm is not reset before the first process, so that
//! process reports it. softIoc with this DB reads the values below.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

const DB: &str = r#"
record(mbbo, "A") { field(ZRST, "a") field(ONST, "b") field(VAL, "20") }
record(mbbo, "B") { field(ZRST, "a") field(ONST, "b") field(VAL, "20")
                    field(IVOA, "Don't drive outputs") field(OUT, "C PP") }
record(ao, "C") { }
"#;

#[epics_macros_rs::epics_test]
async fn init_convert_alarm_reaches_the_first_process_mbborecord_177() {
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let alarm = |rec: &str, new: bool| {
        let r = db.get_record(rec).unwrap();
        let i = r.read();
        if new {
            (i.common.nsta, i.common.nsev)
        } else {
            (i.common.stat, i.common.sevr)
        }
    };
    let soft = (alarm_status::SOFT_ALARM, AlarmSeverity::Invalid);

    assert_eq!(alarm("A", true), soft);

    db.put_record_field_from_ca("A", "VAL", EpicsValue::Enum(1))
        .await
        .unwrap();
    assert_eq!(alarm("A", false), soft);
    db.process_record("A").await.unwrap();
    assert_eq!(alarm("A", false), (0, AlarmSeverity::NoAlarm));

    // The carried INVALID keeps the first write off the link.
    db.put_record_field_from_ca("B", "VAL", EpicsValue::Enum(1))
        .await
        .unwrap();
    assert_eq!(alarm("B", false), soft);
    assert_eq!(db.get_pv("C.VAL").unwrap().to_f64(), Some(0.0));
}
