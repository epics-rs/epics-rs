//! printf reads each INPn at the DBR type its directive names
//! (`printfRecord.c` `GET_PRINT`), pinned to softIoc R7.0.10.1: an Int64
//! source narrows to the request's width, 2^53+1 survives `%ll*`, and a
//! source the request cannot convert prints IVLS with a LINK/INVALID alarm.
//! A double past the target type is c_cast's saturation, pinned in the unit
//! tests (`double_sources_saturate_where_c_x86_64_prints_int_min_by_ub`).

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const ROWS: &[(&str, &str, &[u8], f64)] = &[
    ("I", "%ld", b"705032704", 0.0),
    ("I", "%li", b"705032704", 0.0),
    ("I", "%hd", b"-3584", 0.0),
    ("I", "%hu", b"61952", 0.0),
    ("I", "%hx", b"f200", 0.0),
    ("I", "%d", b"705032704", 0.0),
    ("I", "%lld", b"5000000000", 0.0),
    ("B", "%lld", b"9007199254740993", 0.0),
    ("B", "%llx", b"20000000000001", 0.0),
    ("B", "%llu", b"9007199254740993", 0.0),
    ("SS", "%d", b"LNK", 3.0),
    ("SS", "%ls", b"LNK", 3.0),
    ("SS", "%c", b"LNK", 3.0),
    ("SS", "%f", b"LNK", 3.0),
    ("SN", "%d", b"65", 0.0),
    ("SN", "%c", b"A", 0.0),
    ("M1", "%hhd", b"-1", 0.0),
    ("M1", "%c", b"\xff", 0.0),
    ("AA", "%c", b"A", 0.0),
    ("SS", "%s", b"abc", 0.0),
    ("B", "%f", b"9007199254740992.000000", 0.0),
    ("I", "%hf", b"5000000000.000000", 0.0),
    ("WF", "%ls", b"hello_world", 0.0),
    ("W", "%s", b"hello_world", 0.0),
];

fn db_text(rows: &[(&str, &str, &[u8], f64)]) -> String {
    let mut db = String::from(
        r#"
record(int64in, "I") { field(VAL, "5000000000") }
record(int64in, "B") { field(VAL, "9007199254740993") }
record(stringin, "SS") { field(VAL, "abc") }
record(stringin, "SN") { field(VAL, "65") }
record(ai, "M1") { field(VAL, "-1") }
record(ai, "AA") { field(VAL, "65") }
record(lso, "W") { field(SIZV, "100") field(OUT, "WF PP") }
record(waveform, "WF") { field(FTVL, "CHAR") field(NELM, "100") }
"#,
    );
    for (i, (src, fmt, _, _)) in rows.iter().enumerate() {
        db.push_str(&format!(
            "record(printf, \"P{i}\") {{ field(FMT, \"[{fmt}]\") field(INP0, \"{src}\") }}\n"
        ));
    }
    db
}

async fn check(rows: &[(&str, &str, &[u8], f64)]) {
    let db = IocBuilder::new()
        .db_string(&db_text(rows), &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.put_record_field_from_ca(
        "W",
        "VAL",
        EpicsValue::CharArray(b"hello_world".as_slice().into()),
    )
    .await
    .unwrap();
    let mut failed = Vec::new();
    for (i, (src, fmt, want, sevr)) in rows.iter().enumerate() {
        let rec = format!("P{i}");
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        db.process_record_with_links(&rec, &mut visited)
            .await
            .unwrap();
        let EpicsValue::CharArray(got) = db.get_pv(format!("{rec}.VAL")).unwrap() else {
            panic!("{rec}.VAL is not a char array");
        };
        let got_sevr = db.get_pv(format!("{rec}.SEVR")).unwrap().to_f64().unwrap();
        let mut want_val = b"[".to_vec();
        want_val.extend_from_slice(want);
        want_val.extend_from_slice(b"]\0");
        if got.as_ref() != want_val.as_slice() || got_sevr != *sevr {
            failed.push(format!(
                "{fmt} of {src}: {:?} sevr {got_sevr}, softIoc {:?} sevr {sevr}",
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(&want_val)
            ));
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

#[epics_macros_rs::epics_test]
async fn printf_reads_each_directive_at_its_dbr_type() {
    check(ROWS).await;
}
