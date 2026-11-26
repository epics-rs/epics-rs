//! A QSRV read of a `$` string channel is `dbGet` at the type and count the
//! channel serves, so a slice ends in the NUL of `dbGet`'s DBR_CHAR tail and a
//! `ts` filter keeps its own type (`dbAccess.c:1037-1044`). softIocPVX 1.5.3,
//! `pvxget -F tree`, on the records below.
#![cfg(feature = "qsrv-core")]

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_bridge_rs::qsrv::{AnyChannel, BridgeProvider, Channel};
use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};

const DB: &str = r#"
record(calc, "SRC") { field(DESC, "abcdefgh") }
record(calc, "SRC40") { field(DESC, "0123456789abcdefghijklmnopqrstuvwxyzABCD") }
"#;

/// `value` as pvxget prints it: its element type and elements.
fn value(s: &PvStructure) -> String {
    let Some(PvField::ScalarArrayTyped(a)) = s.get_field("value") else {
        panic!("no array value in {s:?}");
    };
    let elements: Vec<ScalarValue> = a.to_scalar_values();
    let ty = match elements.first() {
        Some(ScalarValue::Byte(_)) => "int8_t",
        Some(ScalarValue::UInt(_)) => "uint32_t",
        other => panic!("{other:?}"),
    };
    let text: Vec<String> = elements
        .iter()
        .map(|e| match e {
            ScalarValue::Byte(c) => c.to_string(),
            ScalarValue::UInt(n) => n.to_string(),
            other => panic!("{other:?}"),
        })
        .collect();
    format!("{ty}[] {{{}}}[{}]", text.len(), text.join(", "))
}

// RTEMS-EXEC-MODEL-ALLOW(1): checked - runs and passes in the exec-backend
// suite.
#[tokio::test]
async fn a_qsrv_dollar_string_read_ends_like_dbget() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let provider = BridgeProvider::new(db);
    let src40: Vec<String> = b"0123456789abcdefghijklmnopqrstuvwxyzABCD\0"
        .iter()
        .map(|c| c.to_string())
        .collect();
    let src40 = format!("int8_t[] {{41}}[{}]", src40.join(", "));
    let want = [
        (
            "SRC.DESC$",
            "int8_t[] {9}[97, 98, 99, 100, 101, 102, 103, 104, 0]",
        ),
        ("SRC.DESC$[2:5]", "int8_t[] {4}[99, 100, 101, 0]"),
        (
            r#"SRC.DESC${"arr":{"s":2,"e":5}}"#,
            "int8_t[] {4}[99, 100, 101, 0]",
        ),
        (
            r#"SRC.DESC${"ts":{"num":"ts","epoch":"unix"}}"#,
            "uint32_t[] {2}[631152000, 0]",
        ),
        ("SRC40.DESC$", src40.as_str()),
    ];
    let mut got = Vec::new();
    for (pv, _) in want {
        let AnyChannel::Single(ch) = provider
            .create_channel_for(pv.as_bytes(), "u", "h")
            .await
            .unwrap_or_else(|e| panic!("{pv}: {e:?}"))
        else {
            panic!("{pv}: not a single-record channel");
        };
        let read = ch
            .get(&PvStructure::new("epics:nt/NTRequest:1.0"))
            .await
            .unwrap_or_else(|e| panic!("{pv}: {e:?}"));
        got.push((pv, value(&read)));
    }
    let want: Vec<_> = want.iter().map(|(pv, v)| (*pv, v.to_string())).collect();
    assert_eq!(got, want);
}
