//! `stdSupport.dbd:18-19` appends "Time of Day" to stringin's and
//! "Sec Past Epoch" to ai's `dbDeviceMenu`, after base's own lines. rs-only
//! pin: no built C std module is present to run softIoc with.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::dbd_generated::device_menu;
use epics_base_rs::types::EpicsValue;

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the exec
// backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p std-rs
// --all-features --test time_of_day_dtyp_menu`). No CA server is built; the
// reactor comes from `#[tokio::test]` itself.
#[tokio::test]
async fn std_time_of_day_dtyps_extend_the_base_menus_stdsupport_dbd_18() {
    let db_content = r#"
record(ai, "SEC") { field(DTYP, "Sec Past Epoch") }
record(stringin, "STR") { field(DTYP, "Time of Day") }
record(ai, "PLAIN") { }
"#;
    let mut builder = IocBuilder::new();
    for (record_type, devsup, factory) in std_rs::std_device_supports() {
        builder = builder.register_device_support(record_type, devsup, factory);
    }
    let (db, _) = builder
        .db_string(db_content, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let index = |rt: &str| device_menu(rt).unwrap().len() as u16;
    assert_eq!(
        db.get_pv("SEC.DTYP").ok(),
        Some(EpicsValue::Enum(index("ai")))
    );
    assert_eq!(
        db.get_pv("STR.DTYP").ok(),
        Some(EpicsValue::Enum(index("stringin")))
    );
    // The name resolves through the menu for a record bound to something else.
    db.put_record_field_from_ca_no_notify(
        "PLAIN",
        "DTYP",
        EpicsValue::String("Sec Past Epoch".into()),
    )
    .await
    .unwrap();
    assert_eq!(
        db.get_pv("PLAIN.DTYP").ok(),
        Some(EpicsValue::Enum(index("ai")))
    );
}
