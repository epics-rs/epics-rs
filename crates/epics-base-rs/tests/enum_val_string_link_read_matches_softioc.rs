//! A `DBR_STRING` link read of an enum `VAL` renders `get_enum_str` at the
//! source: a defined state, an undefined one (empty), one past the slots
//! (the class's sentinel), and an mbbi with no strings at all. Each source is
//! read by a stringin INP, a stringout DOL and an lso DOL; softIoc R7.0.10
//! prints the same string for all three, the one in `EXPECTED`, for this
//! database plus those readers.

use std::collections::HashMap;

use epics_base_rs::server::database::ProcStack;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const SOURCES: &str = r#"
record(mbbi, "M1") { field(ZRST, "zero") field(ONST, "one") field(VAL, "1") }
record(mbbi, "M5") { field(ZRST, "zero") field(ONST, "one") field(VAL, "5") }
record(mbbi, "M20") { field(ZRST, "zero") field(ONST, "one") field(VAL, "20") }
record(mbbi, "MB0") { field(VAL, "0") }
record(mbbi, "MB3") { field(VAL, "3") }
record(mbbo, "MO2") { field(ZRST, "zero") field(ONST, "one") field(TWST, "two") field(VAL, "2") }
record(mbbo, "MO7") { field(ZRST, "zero") field(VAL, "7") }
record(mbbo, "MO16") { field(ZRST, "zero") field(VAL, "16") }
record(bo, "B1") { field(ZNAM, "off") field(ONAM, "on") field(VAL, "1") }
record(bo, "BZ1") { field(ZNAM, "off") field(VAL, "1") }
record(bo, "B2") { field(ZNAM, "off") field(ONAM, "on") field(VAL, "2") }
record(bi, "BI0") { field(ZNAM, "lo") field(ONAM, "hi") field(VAL, "0") }
record(bi, "BI3") { field(ZNAM, "lo") field(ONAM, "hi") field(VAL, "3") }
"#;

/// `(source, the string every reader of it holds)`, from softIoc.
const EXPECTED: &[(&str, &str)] = &[
    ("M1", "one"),
    ("M5", ""),
    ("M20", "Illegal Value"),
    ("MB0", ""),
    ("MB3", ""),
    ("MO2", "two"),
    ("MO7", ""),
    ("MO16", "Illegal Value"),
    ("B1", "on"),
    ("BZ1", ""),
    ("B2", "Illegal_Value"),
    ("BI0", "lo"),
    ("BI3", "Illegal_Value"),
];

fn text(value: EpicsValue) -> Vec<u8> {
    match value {
        EpicsValue::String(s) => s.as_bytes().to_vec(),
        EpicsValue::CharArray(c) => c.iter().copied().take_while(|&b| b != 0).collect(),
        other => panic!("not a string: {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn every_reader_renders_the_source_get_enum_str() {
    let mut db_text = SOURCES.to_string();
    for (src, _) in EXPECTED {
        db_text.push_str(&format!(
            "record(stringin, \"SI_{src}\") {{ field(INP, \"{src} NPP\") }}\n\
             record(stringout, \"SO_{src}\") {{ field(DOL, \"{src} NPP\") field(OMSL, \"closed_loop\") }}\n\
             record(lso, \"LSO_{src}\") {{ field(DOL, \"{src} NPP\") field(OMSL, \"closed_loop\") }}\n"
        ));
    }
    let (db, _) = IocBuilder::new()
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for &(src, expected) in EXPECTED {
        for reader in ["SI", "SO", "LSO"] {
            let name = format!("{reader}_{src}");
            let mut stack = ProcStack::new();
            db.process_record_with_links(&name, &mut stack)
                .await
                .unwrap();
            let got = text(db.get_pv(format!("{name}.VAL")).unwrap());
            assert_eq!(String::from_utf8_lossy(&got), expected, "{name}");
        }
    }
}
