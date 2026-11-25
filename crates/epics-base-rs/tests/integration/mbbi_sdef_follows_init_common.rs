//! mbbi SDEF is set by `init_common`, after the dset's `init_record`
//! (`mbbiRecord.c:135`), and again by `special()` after a put to a state
//! field (`:221`). softIoc with this DB reads the SDEF values below.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

const DB: &str = r#"
record(mbbi, "A") { }
record(mbbi, "B") { field(ONVL, "3") }
record(mbbi, "D") { field(ONST, "x") }
"#;

#[epics_macros_rs::epics_test]
async fn a_state_field_put_recomputes_sdef_mbbirecord_221() {
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let sdef = |rec: &str| db.get_pv(format!("{rec}.SDEF")).ok();
    let put = |rec: &'static str, f: &'static str, v: EpicsValue| {
        let db = db.clone();
        async move {
            db.put_record_field_from_ca_no_notify(rec, f, v)
                .await
                .unwrap()
        }
    };

    assert_eq!(sdef("A"), Some(EpicsValue::Short(0)));
    assert_eq!(sdef("B"), Some(EpicsValue::Short(1)));
    assert_eq!(sdef("D"), Some(EpicsValue::Short(1)));

    put("A", "ZRVL", EpicsValue::ULong(5)).await;
    assert_eq!(sdef("A"), Some(EpicsValue::Short(1)));
    put("A", "ZRVL", EpicsValue::ULong(0)).await;
    assert_eq!(sdef("A"), Some(EpicsValue::Short(0)));
    put("A", "FFST", EpicsValue::String("y".into())).await;
    assert_eq!(sdef("A"), Some(EpicsValue::Short(1)));
    put("B", "ONVL", EpicsValue::ULong(0)).await;
    assert_eq!(sdef("B"), Some(EpicsValue::Short(0)));
}
