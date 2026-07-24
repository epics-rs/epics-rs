//! R19-67: a JSON-brace field value's strings are decoded by yajl, so the
//! escapes MUST be translated before the value reaches the record.
//!
//! C `dbLexRoutines.c:1398` translates a field value's escapes only when the
//! value starts with a quote — a `{…}` value is deliberately left alone,
//! because `dbParseLink` → `dbJLinkInit` → `dbJLinkParse` feeds it to yajl, and
//! `yajl_parser.c:273-281` runs `yajl_string_decode` (`yajl_encode.c:136-215`)
//! before the jlif `string` callback (`lnkConst.c:199`) ever sees it. The port
//! carried the brace text verbatim all the way in, so the backslashes survived.
//!
//! Measured, softIoc 7.0.10 (linux-x86_64). `dbgf` re-escapes for display, so a
//! STORED backslash prints DOUBLED — which is what makes it an oracle here
//! (`caget` is not one: `tool_lib.c:135` escapes DBF_STRING output too, so a
//! stored TAB comes back on screen as `\t` and is indistinguishable from an
//! untranslated escape):
//!
//! ```text
//! record(stringin,"J1"){field(DTYP,"Soft Channel") field(INP,{const:"a\tb"})}
//!     dbgf J1.VAL -> "a\tb"     stored: a, TAB, b
//! record(stringin,"J2"){field(DTYP,"Soft Channel") field(INP,{const:"x\\ny"})}
//!     dbgf J2.VAL -> "x\\ny"    stored: x, BACKSLASH, n, y
//! ```

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

async fn val(db: &epics_base_rs::server::database::PvDatabase, rec: &str) -> String {
    let inst = db.get_record(rec).unwrap_or_else(|| panic!("{rec}"));
    let inst = inst.read();
    match inst.record.get_field("VAL") {
        Some(EpicsValue::String(s)) => s.as_str_lossy().into_owned(),
        other => panic!("{rec}.VAL: {other:?}"),
    }
}

/// The two records softIoc was measured on, byte for byte.
#[epics_macros_rs::epics_test]
async fn a_const_json_link_string_reaches_the_record_decoded() {
    let db_text = concat!(
        r#"record(stringin,"J1") { field(DTYP,"Soft Channel") field(INP,{const:"a\tb"}) }"#,
        "\n",
        r#"record(stringin,"J2") { field(DTYP,"Soft Channel") field(INP,{const:"x\\ny"}) }"#,
        "\n",
    );
    let (db, _) = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    assert_eq!(
        val(&db, "J1").await.as_bytes(),
        b"a\tb",
        "yajl translates \\t: 3 bytes, a TAB b"
    );
    assert_eq!(
        val(&db, "J2").await.as_bytes(),
        b"x\\ny",
        "yajl collapses \\\\ to ONE backslash: 4 bytes"
    );
}

/// The escape set, one case per yajl production — the boundary, not a story.
#[epics_macros_rs::epics_test]
async fn every_yajl_escape_is_translated_in_a_const_seed() {
    let cases: [(&str, &str, &[u8]); 6] = [
        ("E1", r#"{const:"a\nb"}"#, b"a\nb"),
        ("E2", r#"{const:"a\rb"}"#, b"a\rb"),
        ("E3", r#"{const:"a\"b"}"#, b"a\"b"),
        ("E4", r#"{const:"a\x41b"}"#, b"aAb"),
        ("E5", r#"{const:"aAb"}"#, b"aAb"),
        // No escapes at all: the common case must survive untouched.
        ("E6", r#"{const:"plain text"}"#, b"plain text"),
    ];

    let mut db_text = String::new();
    for (name, link, _) in cases {
        db_text.push_str(&format!(
            "record(stringin,\"{name}\") {{ field(DTYP,\"Soft Channel\") field(INP,{link}) }}\n"
        ));
    }
    let (db, _) = IocBuilder::new()
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    for (name, link, want) in cases {
        assert_eq!(
            val(&db, name).await.as_bytes(),
            want,
            "{name}: field(INP,{link})"
        );
    }
}

/// A QUOTED (non-brace) value keeps its own translator — C runs
/// `dbTranslateEscape` on it (R18-91), not yajl. The two regimes must both
/// hold; fixing one must not disturb the other.
#[epics_macros_rs::epics_test]
async fn a_quoted_value_still_uses_the_db_translator() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(stringin,"Q1") { field(VAL,"a\tb") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    assert_eq!(val(&db, "Q1").await.as_bytes(), b"a\tb");
}
