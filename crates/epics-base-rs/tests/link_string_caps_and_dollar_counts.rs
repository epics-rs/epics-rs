//! A DBR_STRING link read takes at most 39 bytes, and a `$` address reports
//! its char view's element count to the reader. Pinned to oracleStdIoc
//! (R7.0.10 + calc) for the same records.

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(lsi, "L45") { field(SIZV, "100") field(INP, {const:"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHI"}) }
record(aSub, "AS") { field(FTA, "STRING") field(NOA, "1") field(INPA, "L45 NPP") }
record(aSub, "AS4") { field(FTA, "STRING") field(NOA, "4") field(INPA, "L45 NPP") }
record(lsi, "L3") { field(SIZV, "100") field(INP, {const:"abc"}) }
record(sseq, "SQ") { field(DOL1, "L3.VAL$ NPP") field(LNK1, "T.VAL$ NPP") }
record(lsi, "T") { field(SIZV, "100") }
"#;

fn text(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        EpicsValue::StringArray(a) if a.len() == 1 => a[0].as_str_lossy().into_owned(),
        EpicsValue::CharArray(b) => {
            let b: Vec<u8> = b.iter().copied().take_while(|&c| c != 0).collect();
            String::from_utf8(b).unwrap()
        }
        other => panic!("{pv} = {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn a_string_read_caps_at_39_and_a_dollar_source_reads_its_whole_text() {
    let db = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for rec in ["AS", "AS4", "SQ"] {
        db.process_record_with_links(rec, &mut ProcStack::new())
            .await
            .unwrap();
    }
    for _ in 0..200 {
        if text(&db, "T") == "abc" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(text(&db, "AS.A"), "0123456789abcdefghijklmnopqrstuvwxyzABC");
    // A long string is one element to `dbGet`, whatever NOA asks for.
    match db.get_pv("AS4.A").unwrap() {
        EpicsValue::StringArray(a) => assert_eq!(
            a[0].as_str_lossy(),
            "0123456789abcdefghijklmnopqrstuvwxyzABC"
        ),
        other => panic!("AS4.A = {other:?}"),
    }
    assert_eq!(text(&db, "SQ.STR1"), "abc");
    assert_eq!(text(&db, "T"), "abc");
    assert_eq!(db.get_pv("T.LEN").unwrap().to_f64(), Some(4.0));
}
