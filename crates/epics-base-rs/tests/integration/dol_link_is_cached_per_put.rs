//! The closed-loop DOL is parsed once per put (`RecordInstance::record_link`):
//! a runtime DOL put re-points the next cycle's fetch, a constant DOL fetches
//! nothing.

use epics_base_rs::error::CaError;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "S1") { field(VAL, "1") }
record(ao, "S2") { field(VAL, "2") }
record(ao, "AO") { field(OMSL, "closed_loop") field(DOL, "S1 NPP") }
"#;

async fn cycle(db: &epics_base_rs::server::database::PvDatabase) -> Result<(), CaError> {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("AO", &mut visited).await
}

#[epics_macros_rs::epics_test]
async fn a_runtime_dol_put_repoints_the_closed_loop_fetch() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    cycle(&db).await.unwrap();
    assert_eq!(db.get_pv("AO").unwrap(), EpicsValue::Double(1.0));

    db.put_record_field_from_ca("AO", "DOL", EpicsValue::String("S2 NPP".into()))
        .await
        .unwrap();
    cycle(&db).await.unwrap();
    assert_eq!(db.get_pv("AO").unwrap(), EpicsValue::Double(2.0));

    // A constant DOL is applied once at init, never re-sourced at process.
    db.put_record_field_from_ca("AO", "DOL", EpicsValue::String("9".into()))
        .await
        .unwrap();
    cycle(&db).await.unwrap();
    assert_eq!(db.get_pv("AO").unwrap(), EpicsValue::Double(2.0));
}
