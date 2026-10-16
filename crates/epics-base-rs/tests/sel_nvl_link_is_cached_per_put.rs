//! SELM=Specified reads NVL through the per-put link cache: a runtime NVL put
//! re-points the next cycle's selector, and a High/Low/Median SELM reads none.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "N0") { field(VAL, "0") }
record(ao, "N1") { field(VAL, "1") }
record(ao, "A") { field(VAL, "10") }
record(ao, "B") { field(VAL, "20") }
record(sel, "SEL") {
    field(SELM, "Specified")
    field(NVL, "N0 NPP")
    field(INPA, "A NPP")
    field(INPB, "B NPP")
}
"#;

async fn cycle(db: &epics_base_rs::server::database::PvDatabase) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("SEL", &mut visited)
        .await
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn a_runtime_nvl_put_repoints_the_selector() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    cycle(&db).await;
    assert_eq!(db.get_pv("SEL").unwrap(), EpicsValue::Double(10.0));

    db.put_record_field_from_ca("SEL", "NVL", EpicsValue::String("N1 NPP".into()))
        .await
        .unwrap();
    cycle(&db).await;
    assert_eq!(db.get_pv("SEL").unwrap(), EpicsValue::Double(20.0));

    // In High mode NVL is not read: the selector keeps its value and the
    // result is the maximum.
    db.put_record_field_from_ca("SEL", "SELM", EpicsValue::Enum(1))
        .await
        .unwrap();
    cycle(&db).await;
    assert_eq!(db.get_pv("SEL").unwrap(), EpicsValue::Double(20.0));

    // Low mode picks the minimum (A), where Specified with NVL=N1 picks B;
    // putting Specified back re-enables the NVL read.
    db.put_record_field_from_ca("SEL", "SELM", EpicsValue::Enum(2))
        .await
        .unwrap();
    cycle(&db).await;
    assert_eq!(db.get_pv("SEL").unwrap(), EpicsValue::Double(10.0));
    db.put_record_field_from_ca("SEL", "SELM", EpicsValue::Enum(0))
        .await
        .unwrap();
    cycle(&db).await;
    assert_eq!(db.get_pv("SEL").unwrap(), EpicsValue::Double(20.0));
}
