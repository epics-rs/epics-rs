//! event INP (`devEventSoft.c:56`) and SIOL (`eventRecord.c:185`) read
//! `DBR_STRING`, so an ENUM source delivers its label. softIoc R7.0.10.1,
//! one `dbtr` each:
//!
//! ```text
//! bi VAL 1 ONAM RUNNING      -> EB "RUNNING", ES (SIOL) "RUNNING"
//! mbbi VAL 2 TWST two        -> EM "two"
//! ```

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(bi, "BI") { field(ZNAM, "STOPPED") field(ONAM, "RUNNING") field(VAL, "1") }
record(mbbi, "MB") { field(ZRST, "zero") field(ONST, "one") field(TWST, "two") field(VAL, "2") }
record(event, "EB") { field(INP, "BI") }
record(event, "EM") { field(INP, "MB") }
record(event, "ES") { field(SIMM, "YES") field(SIOL, "BI") }
"#;

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn inp_and_siol_read_dbr_string_like_softioc() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for (rec, want) in [("EB", "RUNNING"), ("EM", "two"), ("ES", "RUNNING")] {
        process(&db, rec).await;
        assert_eq!(
            db.get_pv(format!("{rec}.VAL")).unwrap(),
            EpicsValue::String(want.into()),
            "{rec}"
        );
    }
}
