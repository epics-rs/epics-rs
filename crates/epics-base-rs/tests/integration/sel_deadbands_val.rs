//! `selRecord.c:316,319` runs `recGblCheckDeadband` on VAL against MDEL and
//! ADEL; the port served neither field, so `field(MDEL,"5")` loaded and every
//! change posted.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn sel_val_posts_only_past_mdel_and_adel() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(sel, "S") { field(MDEL, "5") field(ADEL, "2") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let rec = db.get_record("S").unwrap();
    let mut inst = rec.write();
    assert_eq!(inst.record.get_field("MDEL"), Some(EpicsValue::Double(5.0)));
    assert_eq!(inst.record.get_field("ADEL"), Some(EpicsValue::Double(2.0)));
    for (val, want) in [
        (2.0, (false, false)),
        (3.0, (false, true)),
        (5.5, (true, true)),
    ] {
        inst.record
            .put_field("VAL", EpicsValue::Double(val))
            .unwrap();
        assert_eq!(inst.check_deadband_ext(), want, "VAL {val}");
    }
}
