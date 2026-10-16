//! printf directives C's `doPrintf` ends early or rejects, pinned to softIoc
//! R7.0.10.1: VAL, LEN and SEVR after one process.

use epics_base_rs::server::database::ProcStack;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai, "SRC") { field(VAL, "5") }
record(printf, "K0") { field(FMT, "%*d|") field(INP0, "5") field(INP1, "6") }
record(printf, "K1") { field(FMT, "%%d %d") field(INP0, "5") }
record(printf, "K2") { field(FMT, "%hd %ld %lf %lld") field(INP0, "5") field(INP1, "6") field(INP2, "7") field(INP3, "8") }
record(printf, "K3") { field(FMT, "%") field(INP0, "5") }
record(printf, "K4") { field(FMT, "abc%") field(INP0, "5") }
record(printf, "K5") { field(FMT, "%5") field(INP0, "5") }
record(printf, "A0") { field(FMT, "%*q %d") field(INP0, "5") field(INP1, "6") }
record(printf, "A1") { field(FMT, "x%*") field(INP0, "NOPE") }
record(printf, "A2") { field(FMT, "%*q %d") field(INP0, "NOPE") field(INP1, "6") }
record(printf, "A3") { field(FMT, "x%*") field(INP0, "5") }
record(printf, "Q0") { field(FMT, "%*d|") field(INP0, "SRC") field(INP1, "SRC") }
record(printf, "Q1") { field(FMT, "abc%") field(INP0, "SRC") }
"#;

/// (record, VAL, SEVR INVALID).
const CASES: [(&str, &str, bool); 12] = [
    ("K0", "    6|", false),
    ("K1", "%d 5", false),
    ("K2", "5 6 7.000000 8", false),
    ("K3", "", false),
    ("K4", "abc", false),
    ("K5", "", false),
    ("A0", "%5q 6", false),
    ("A1", "x", true),
    ("A2", "LNK 6", true),
    ("A3", "x", false),
    ("Q0", "    5|", false),
    ("Q1", "abc", false),
];

#[epics_macros_rs::epics_test]
async fn directive_shapes_match_softioc() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let mut diverging = Vec::new();
    for (rec, want, invalid) in CASES {
        db.process_record_with_links(rec, &mut ProcStack::new())
            .await
            .unwrap();
        let got = match db.get_pv(format!("{rec}.VAL")).unwrap() {
            EpicsValue::CharArray(b) => b.to_vec(),
            other => panic!("{rec}.VAL {other:?}"),
        };
        let len = db.get_pv(format!("{rec}.LEN")).unwrap();
        let sevr = db.get_pv(format!("{rec}.SEVR")).unwrap();
        let want_sevr = EpicsValue::Short(if invalid { 3 } else { 0 });
        if got != [want.as_bytes(), b"\0"].concat()
            || len != EpicsValue::ULong(want.len() as u32 + 1)
            || sevr != want_sevr
        {
            diverging.push(format!(
                "{rec}: {:?} {len:?} {sevr:?}",
                String::from_utf8_lossy(&got)
            ));
        }
    }
    assert!(diverging.is_empty(), "{diverging:#?}");
}
