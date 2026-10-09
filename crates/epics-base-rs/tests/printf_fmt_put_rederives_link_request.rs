//! printf's `%s` reads its link as `DBR_STRING` (`printfRecord.c:291`), so the
//! request is FMT's, and an FMT put mid-run must re-derive it on the next
//! cycle. softIoc, mbbi source (VAL 1, ONST "one"), `dbpf P.PROC 1` after each
//! FMT:
//!
//! ```text
//! FMT "v=%d" : VAL "v=1"   LEN 4
//! FMT "v=%s" : VAL "v=one" LEN 6
//! FMT "v=%d" : VAL "v=1"
//! ```

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(mbbi, "SRC") { field(VAL, "1") field(ZRST, "zero") field(ONST, "one") field(UDF, "0") }
record(printf, "P") { field(FMT, "v=%d") field(INP0, "SRC") }
"#;

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

fn text(db: &PvDatabase) -> String {
    match db.get_pv("P").unwrap() {
        EpicsValue::CharArray(b) => String::from_utf8_lossy(&b).into_owned(),
        other => panic!("printf VAL must be a CharArray, got {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn an_fmt_put_mid_run_rederives_the_link_request() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    process(&db, "P").await;
    assert_eq!(text(&db), "v=1");
    assert_eq!(db.get_pv("P.LEN").unwrap().to_f64(), Some(4.0));

    db.put_pv("P.FMT", EpicsValue::String("v=%s".into()))
        .await
        .unwrap();
    process(&db, "P").await;
    assert_eq!(text(&db), "v=one");
    assert_eq!(db.get_pv("P.LEN").unwrap().to_f64(), Some(6.0));

    db.put_pv("P.FMT", EpicsValue::String("v=%d".into()))
        .await
        .unwrap();
    process(&db, "P").await;
    assert_eq!(text(&db), "v=1");
}
