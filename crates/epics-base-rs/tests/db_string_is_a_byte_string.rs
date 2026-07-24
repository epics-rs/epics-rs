//! R19-68: a `.db` value is a BYTE STRING, so a `\xHH` escape is one byte —
//! including `HH >= 0x80`.
//!
//! C `epicsString.c:106` — the `\x` arm of `epicsStrnRawFromEscaped` ends in
//! `OUT(u)`, which writes ONE `char` into `dst`. A DBF_STRING is 40 bytes of
//! storage, not 40 characters, so the escape spends exactly one of them.
//!
//! The port modelled the translation as a Rust `String`, so `\xff` became
//! `char::from_u32(0xFF)` = U+00FF = the TWO UTF-8 bytes `c3 bf`: the stored
//! value was one byte too long and the CA/PVA wire carried `h c3 bf z` where C
//! carries `h ff z`.
//!
//! Measured, softIoc 7.0.10 (linux-x86_64). `dbgf` prints through
//! `epicsStrnEscapedFromRaw`, which re-escapes a non-printable byte as `\xHH` —
//! so ONE `\xff` on screen means ONE stored byte, and a UTF-8 encoding of
//! U+00FF would have printed `\xc3\xbf`:
//!
//! ```text
//! record(stringin,"X1") { field(VAL,"h\xffz") }
//!     dbgf X1.VAL -> "h\xffz"     stored: h, 0xFF, z   (3 bytes)
//! ```

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

async fn val_bytes(db: &epics_base_rs::server::database::PvDatabase, rec: &str) -> Vec<u8> {
    let inst = db.get_record(rec).unwrap_or_else(|| panic!("{rec}"));
    let inst = inst.read();
    match inst.record.get_field("VAL") {
        Some(EpicsValue::String(s)) => s.as_bytes().to_vec(),
        other => panic!("{rec}.VAL: {other:?}"),
    }
}

/// The record softIoc was measured on.
#[epics_macros_rs::epics_test]
async fn a_high_hex_escape_in_a_db_value_is_one_byte() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(stringin,"X1") { field(VAL,"h\xffz") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    assert_eq!(
        val_bytes(&db, "X1").await,
        vec![b'h', 0xFF, b'z'],
        "C: OUT(u) writes ONE char (epicsString.c:106)"
    );
}

/// The boundary: below 0x80 the two models agree, at and above it they did not.
#[epics_macros_rs::epics_test]
async fn the_escape_boundary_at_0x80() {
    let (db, _) = IocBuilder::new()
        .db_string(
            concat!(
                r#"record(stringin,"B1") { field(VAL,"\x7f") }"#,
                "\n",
                r#"record(stringin,"B2") { field(VAL,"\x80") }"#,
                "\n",
                r#"record(stringin,"B3") { field(VAL,"\xc3\xa9") }"#,
                "\n",
            ),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    assert_eq!(val_bytes(&db, "B1").await, vec![0x7F], "ASCII: one byte");
    assert_eq!(val_bytes(&db, "B2").await, vec![0x80], "one byte, not two");
    // The two bytes of a UTF-8 é, spelled by hand: they must reach the record
    // as those two bytes, not as four.
    assert_eq!(val_bytes(&db, "B3").await, vec![0xC3, 0xA9]);
}

/// A common (dbCommon) DBF_STRING carries bytes too — DESC has the same budget.
#[epics_macros_rs::epics_test]
async fn a_common_string_field_carries_bytes() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(ai,"D1") { field(DESC,"h\xffz") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let inst = db.get_record("D1").unwrap();
    let desc = inst.read().common.desc.clone();
    assert_eq!(
        desc.as_bytes(),
        [b'h', 0xFF, b'z'],
        "DESC is a 40-BYTE field, and the escape spends one byte"
    );
}
