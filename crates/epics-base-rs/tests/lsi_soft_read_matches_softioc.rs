//! lsi soft INP read pinned to softIoc R7.0.10.1 (`devLsiSoft.c` is
//! `dbGetLinkLS`): a CHAR waveform source copies up to SIZV bytes, a string
//! source reads DBR_STRING (39 bytes), and LEN is `strlen + 1`. A VAL put on
//! the Passive record processes it (VAL is pp) and the INP read replaces it.
//! UDF is pinned by `soft_input_udf_follows_the_device`. A long-string source
//! (lso VAL) is `DBF_STRING` to `dbGetLinkLS` (`lsoRecord.c:184`), so it reads
//! as DBR_STRING; that case fails until links.rs resolves such a source as
//! `DbFieldType::String` rather than CHAR.
//!
//! ```text
//! lso W(61 chars) -> WF CHAR   L  SIZV 80 LEN 62   L2 SIZV 20 LEN 20
//! L4 INP -> lso W              LEN 40
//! L3 INP -> stringin "abc"     init LEN 0 "" INVALID; proc LEN 4 "abc"
//!                              NO_ALARM; put "xy" -> "abc" LEN 4
//! ```

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const X: &str = "0123456789012345678901234567890123456789012345678901234567890";

const DB: &str = r#"
record(lso, "W") { field(SIZV, "100") field(OUT, "WF PP") }
record(waveform, "WF") { field(FTVL, "CHAR") field(NELM, "100") }
record(lsi, "L") { field(SIZV, "80") field(INP, "WF") }
record(lsi, "L2") { field(SIZV, "20") field(INP, "WF") }
record(lsi, "L4") { field(SIZV, "80") field(INP, "W") }
record(stringin, "SI") { field(VAL, "abc") }
record(lsi, "L3") { field(SIZV, "80") field(INP, "SI") }
"#;

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

fn text(db: &PvDatabase, rec: &str) -> String {
    match db.get_pv(&format!("{rec}.VAL")).unwrap() {
        EpicsValue::CharArray(b) => String::from_utf8_lossy(&b).into_owned(),
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{rec}: {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn lsi_soft_read_matches_softioc() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    // `dbpf` puts a long-string field as DBR_CHAR, so all 61 bytes land.
    db.put_record_field_from_ca("W", "VAL", EpicsValue::CharArray(X.as_bytes().into()))
        .await
        .unwrap();
    for (rec, len) in [("L", 62.0), ("L2", 20.0)] {
        process(&db, rec).await;
        assert_eq!(num(&db, &format!("{rec}.LEN")), len, "{rec}.LEN");
        assert_eq!(text(&db, rec), X[..len as usize - 1], "{rec}.VAL");
    }

    let state = |db: &PvDatabase| (num(db, "L3.LEN"), text(db, "L3"), num(db, "L3.SEVR"));
    assert_eq!(state(&db), (0.0, String::new(), 3.0), "init");
    process(&db, "L3").await;
    assert_eq!(state(&db), (4.0, "abc".into(), 0.0), "proc");
    db.put_record_field_from_ca("L3", "VAL", EpicsValue::String("xy".into()))
        .await
        .unwrap();
    assert_eq!(state(&db), (4.0, "abc".into(), 0.0), "put");
    process(&db, "L3").await;
    assert_eq!(state(&db), (4.0, "abc".into(), 0.0), "proc2");
}

#[epics_macros_rs::epics_test]
async fn lsi_reads_a_long_string_source_as_dbr_string() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.put_record_field_from_ca("W", "VAL", EpicsValue::CharArray(X.as_bytes().into()))
        .await
        .unwrap();
    process(&db, "L4").await;
    assert_eq!(num(&db, "L4.LEN"), 40.0, "L4.LEN");
    assert_eq!(text(&db, "L4"), X[..39], "L4.VAL");
}
