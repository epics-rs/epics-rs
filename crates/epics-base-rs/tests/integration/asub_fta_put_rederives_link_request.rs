//! An aSub STRING channel reads its link as `DBR_STRING` (`aSubRecord.c:283`),
//! so the request is FTx's, and an FTA put mid-run must re-derive it on the
//! next cycle: the mbbi source (VAL 1, ONST "one") reads "one" under STRING and
//! 1 under DOUBLE.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(mbbi, "SRC") { field(VAL, "1") field(ZRST, "zero") field(ONST, "one") field(UDF, "0") }
record(aSub, "S") { field(INPA, "SRC") field(FTA, "STRING") }
"#;

async fn process(db: &PvDatabase) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links("S", &mut visited).await;
}

/// FTA is `SPC_NOMOD` to a client, so the record's own writer is the one a
/// run-time change goes through.
fn set_fta(db: &PvDatabase, menu: i16) {
    let rec = db.get_record("S").unwrap();
    rec.write()
        .record
        .put_field("FTA", EpicsValue::Short(menu))
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn an_fta_put_mid_run_rederives_the_link_request() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    process(&db).await;
    assert_eq!(
        db.get_pv("S.A").unwrap().text_g(),
        "one",
        "a STRING channel reads the source's label"
    );

    set_fta(&db, 10);
    process(&db).await;
    assert_ne!(db.get_pv("S.A").unwrap().text_g(), "one");

    set_fta(&db, 0);
    process(&db).await;
    assert_eq!(db.get_pv("S.A").unwrap().text_g(), "one");
}
