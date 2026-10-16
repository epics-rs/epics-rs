//! `(OMSL, OIF)` are settled at construction and by the field-put funnels:
//! a runtime put of either changes the next cycle's closed-loop DOL fetch.

use epics_base_rs::error::CaError;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "S1") { field(VAL, "5") }
record(ao, "AO") { field(DOL, "S1 NPP") field(VAL, "1") }
"#;

async fn cycle(db: &epics_base_rs::server::database::PvDatabase) -> Result<(), CaError> {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("AO", &mut visited).await
}

async fn put(db: &epics_base_rs::server::database::PvDatabase, field: &str, v: u16) {
    db.put_record_field_from_ca("AO", field, EpicsValue::Enum(v))
        .await
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn a_runtime_omsl_or_oif_put_changes_the_next_dol_fetch() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    // supervisory: DOL is not fetched.
    cycle(&db).await.unwrap();
    assert_eq!(db.get_pv("AO").unwrap(), EpicsValue::Double(1.0));

    put(&db, "OMSL", 1).await;
    cycle(&db).await.unwrap();
    assert_eq!(db.get_pv("AO").unwrap(), EpicsValue::Double(5.0));

    // OIF incremental: VAL += DOL.
    put(&db, "OIF", 1).await;
    cycle(&db).await.unwrap();
    assert_eq!(db.get_pv("AO").unwrap(), EpicsValue::Double(10.0));

    put(&db, "OIF", 0).await;
    cycle(&db).await.unwrap();
    assert_eq!(db.get_pv("AO").unwrap(), EpicsValue::Double(5.0));

    put(&db, "OMSL", 0).await;
    db.put_record_field_from_ca("AO", "VAL", EpicsValue::Double(2.0))
        .await
        .unwrap();
    cycle(&db).await.unwrap();
    assert_eq!(db.get_pv("AO").unwrap(), EpicsValue::Double(2.0));
}
