//! printf, 14 sources x 16 conversions, pinned cell by cell to softIoc
//! R7.0.10.1 (`[conv]` FMT, one INP0 per record). Every double source sits
//! inside every target type, so no cell is a C UB cast. `true` is SEVR
//! INVALID (the LINK alarm of a read C cannot convert).

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

type Row = (&'static str, [(&'static [u8], bool); 16]);

const CONVS: [&str; 16] = [
    "%d", "%hd", "%hhd", "%ld", "%lld", "%u", "%hu", "%hhu", "%x", "%llx", "%o", "%c", "%f", "%hf",
    "%s", "%ls",
];

const SOURCES: &str = r#"
record(int64in, "I64") { field(VAL, "5000000000") }
record(int64in, "I64B") { field(VAL, "9007199254740993") }
record(int64in, "I64N") { field(VAL, "-1") }
record(longin, "L") { field(VAL, "-200") }
record(longin, "L2") { field(VAL, "300") }
record(longin, "L3") { field(VAL, "70000") }
record(ai, "A65") { field(VAL, "65") }
record(ai, "A27") { field(VAL, "2.7") }
record(ai, "A0") { field(VAL, "0") }
record(stringin, "SS") { field(VAL, "abc") }
record(stringin, "SN") { field(VAL, "65") }
record(stringin, "SM") { field(VAL, "-1") }
record(mbbi, "MB") { field(VAL, "2") field(ZRST, "zero") field(ONST, "one") field(TWST, "two") }
record(bi, "BI") { field(VAL, "1") field(ZNAM, "off") field(ONAM, "on") }
"#;

#[rustfmt::skip]
const ROWS: [Row; 14] = [
    ("I64", [(b"705032704", false), (b"-3584", false), (b"0", false), (b"705032704", false), (b"5000000000", false), (b"705032704", false), (b"61952", false), (b"0", false), (b"2a05f200", false), (b"12a05f200", false), (b"5201371000", false), (b"\x00", false), (b"5000000000.000000", false), (b"5000000000.000000", false), (b"5000000000", false), (b"", false)]),
    ("I64B", [(b"1", false), (b"1", false), (b"1", false), (b"1", false), (b"9007199254740993", false), (b"1", false), (b"1", false), (b"1", false), (b"1", false), (b"20000000000001", false), (b"1", false), (b"\x01", false), (b"9007199254740992.000000", false), (b"9007199254740992.000000", false), (b"9007199254740993", false), (b"\x01", false)]),
    ("I64N", [(b"-1", false), (b"-1", false), (b"-1", false), (b"4294967295", false), (b"-1", false), (b"4294967295", false), (b"65535", false), (b"255", false), (b"ffffffff", false), (b"ffffffffffffffff", false), (b"37777777777", false), (b"\xff", false), (b"-1.000000", false), (b"-1.000000", false), (b"-1", false), (b"\xff", false)]),
    ("L", [(b"-200", false), (b"-200", false), (b"56", false), (b"4294967096", false), (b"-200", false), (b"4294967096", false), (b"65336", false), (b"56", false), (b"ffffff38", false), (b"ffffffffffffff38", false), (b"37777777470", false), (b"8", false), (b"-200.000000", false), (b"-200.000000", false), (b"-200", false), (b"8", false)]),
    ("L2", [(b"300", false), (b"300", false), (b"44", false), (b"300", false), (b"300", false), (b"300", false), (b"300", false), (b"44", false), (b"12c", false), (b"12c", false), (b"454", false), (b",", false), (b"300.000000", false), (b"300.000000", false), (b"300", false), (b",", false)]),
    ("L3", [(b"70000", false), (b"4464", false), (b"112", false), (b"70000", false), (b"70000", false), (b"70000", false), (b"4464", false), (b"112", false), (b"11170", false), (b"11170", false), (b"210560", false), (b"p", false), (b"70000.000000", false), (b"70000.000000", false), (b"70000", false), (b"p", false)]),
    ("A65", [(b"65", false), (b"65", false), (b"65", false), (b"65", false), (b"65", false), (b"65", false), (b"65", false), (b"65", false), (b"41", false), (b"41", false), (b"101", false), (b"A", false), (b"65.000000", false), (b"65.000000", false), (b"65", false), (b"A", false)]),
    ("A27", [(b"2", false), (b"2", false), (b"2", false), (b"2", false), (b"2", false), (b"2", false), (b"2", false), (b"2", false), (b"2", false), (b"2", false), (b"2", false), (b"\x02", false), (b"2.700000", false), (b"2.700000", false), (b"3", false), (b"\x02", false)]),
    ("A0", [(b"0", false), (b"0", false), (b"0", false), (b"0", false), (b"0", false), (b"0", false), (b"0", false), (b"0", false), (b"0", false), (b"0", false), (b"0", false), (b"\x00", false), (b"0.000000", false), (b"0.000000", false), (b"0", false), (b"", false)]),
    ("SS", [(b"LNK", true), (b"LNK", true), (b"LNK", true), (b"LNK", true), (b"LNK", true), (b"LNK", true), (b"LNK", true), (b"LNK", true), (b"LNK", true), (b"LNK", true), (b"LNK", true), (b"LNK", true), (b"LNK", true), (b"LNK", true), (b"abc", false), (b"LNK", true)]),
    ("SN", [(b"65", false), (b"65", false), (b"65", false), (b"65", false), (b"65", false), (b"65", false), (b"65", false), (b"65", false), (b"41", false), (b"41", false), (b"101", false), (b"A", false), (b"65.000000", false), (b"65.000000", false), (b"65", false), (b"A", false)]),
    ("SM", [(b"-1", false), (b"-1", false), (b"-1", false), (b"4294967295", false), (b"-1", false), (b"4294967295", false), (b"65535", false), (b"255", false), (b"ffffffff", false), (b"ffffffffffffffff", false), (b"37777777777", false), (b"\xff", false), (b"-1.000000", false), (b"-1.000000", false), (b"-1", false), (b"\xff", false)]),
    ("MB", [(b"2", false), (b"2", false), (b"2", false), (b"2", false), (b"2", false), (b"2", false), (b"2", false), (b"2", false), (b"2", false), (b"2", false), (b"2", false), (b"\x02", false), (b"2.000000", false), (b"2.000000", false), (b"two", false), (b"\x02", false)]),
    ("BI", [(b"1", false), (b"1", false), (b"1", false), (b"1", false), (b"1", false), (b"1", false), (b"1", false), (b"1", false), (b"1", false), (b"1", false), (b"1", false), (b"\x01", false), (b"1.000000", false), (b"1.000000", false), (b"on", false), (b"\x01", false)]),
];

/// The cell links.rs owns, left out: `%s` of an ai at PREC 0 is C's
/// DBR_STRING at precision ("3" for 2.7), `dbr_string_of`.
fn routed(src: &str, conv: &str) -> bool {
    src == "A27" && conv == "%s"
}

async fn diverging(keep: impl Fn(&str, &str) -> bool) -> Vec<String> {
    let mut db_text = String::from(SOURCES);
    for (i, (src, _)) in ROWS.iter().enumerate() {
        for (j, conv) in CONVS.iter().enumerate() {
            db_text.push_str(&format!(
                "record(printf, \"P{i}_{j}\") {{ field(FMT, \"[{conv}]\") field(INP0, \"{src}\") }}\n"
            ));
        }
    }
    let db = IocBuilder::new()
        .db_string(&db_text, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let mut out = Vec::new();
    for (i, (src, cells)) in ROWS.iter().enumerate() {
        for (j, (conv, (want, invalid))) in CONVS.iter().zip(cells).enumerate() {
            if !keep(src, conv) {
                continue;
            }
            let rec = format!("P{i}_{j}");
            let mut visited = epics_base_rs::server::database::ProcStack::new();
            db.process_record_with_links(&rec, &mut visited)
                .await
                .unwrap();
            let EpicsValue::CharArray(got) = db.get_pv(format!("{rec}.VAL")).unwrap() else {
                panic!("{rec}.VAL is not a char array");
            };
            let sevr = db.get_pv(format!("{rec}.SEVR")).unwrap().to_f64().unwrap();
            let mut expect = b"[".to_vec();
            expect.extend_from_slice(want);
            expect.extend_from_slice(b"]\0");
            if got.as_ref() != expect.as_slice() || (sevr == 3.0) != *invalid {
                out.push(format!(
                    "{conv} of {src}: {:?} sevr {sevr}, softIoc {:?} invalid {invalid}",
                    String::from_utf8_lossy(&got),
                    String::from_utf8_lossy(&expect)
                ));
            }
        }
    }
    out
}

#[epics_macros_rs::epics_test]
async fn printf_matches_softioc_across_sources() {
    let failed = diverging(|s, c| !routed(s, c)).await;
    assert!(failed.is_empty(), "{failed:#?}");
}
