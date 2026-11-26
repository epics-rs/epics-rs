//! A `.db` `DOn` is stored as loaded: C `dbPutString` runs no `special()`,
//! and `init_record` only renders an empty `STRn` from it at PREC
//! (`sseqRecord.c:243-249`), never writing `DOn` back.
//!
//! C oracleIoc (calc R2-x, softIoc R7.0.10), read after iocInit:
//!
//! ```text
//! PREC 2, DO1 1.23456         ->  DO1 1.23456  STR1 "1.23"
//! DO1 1.5, then PREC 2        ->  DO1 1.5      STR1 "1.50"
//! PREC 2, STR1 "5", DO1 7     ->  DO1 5        STR1 "5"
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(sseq, "SQ1") { field(PREC, "2") field(DO1, "1.23456") }
record(sseq, "SQ2") { field(DO1, "1.5") field(PREC, "2") }
record(sseq, "SQ3") { field(PREC, "2") field(STR1, "5") field(DO1, "7") }
"#;

async fn build() -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn pair(db: &PvDatabase, rec: &str) -> (EpicsValue, EpicsValue) {
    (
        db.get_pv(format!("{rec}.DO1")).unwrap(),
        db.get_pv(format!("{rec}.STR1")).unwrap(),
    )
}

#[epics_macros_rs::epics_test]
async fn a_loaded_do_is_not_rounded_to_prec() {
    let db = build().await;
    assert_eq!(
        pair(&db, "SQ1"),
        (
            EpicsValue::Double(1.23456),
            EpicsValue::String("1.23".into())
        )
    );
}

#[epics_macros_rs::epics_test]
async fn a_do_loaded_before_prec_renders_str_at_prec() {
    let db = build().await;
    assert_eq!(
        pair(&db, "SQ2"),
        (EpicsValue::Double(1.5), EpicsValue::String("1.50".into()))
    );
}

#[epics_macros_rs::epics_test]
async fn a_loaded_str_wins_over_a_later_do() {
    let db = build().await;
    assert_eq!(
        pair(&db, "SQ3"),
        (EpicsValue::Double(5.0), EpicsValue::String("5".into()))
    );
}
