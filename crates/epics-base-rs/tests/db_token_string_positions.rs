//! R19-61: every grammar position C spells `tokenSTRING` takes a bareword OR a
//! quoted string, and the port must take both in all of them.
//!
//! C `dbLex.l:88-97` — `tokenSTRING` is EITHER the bareword rule
//! (`[a-zA-Z0-9_\-+:.\[\]<>;]+`) OR the double-quoted rule; the grammar
//! (`dbYacc.y`) never distinguishes them:
//!
//! ```text
//! record_head:  '(' tokenSTRING ',' tokenSTRING ')'          :230
//! record_field: tokenFIELD '(' tokenSTRING ',' json_value ')'  :256
//! record_alias: tokenALIAS '(' tokenSTRING ')'                :268
//! alias:        tokenALIAS '(' tokenSTRING ',' tokenSTRING ')' :275
//! ```
//!
//! softIoc 7.0 (linux-x86_64) loads this file and answers every `dbgf`:
//!
//! ```text
//! record("ai", "QT1") { field("VAL", "5") field(DESC, "quoted type and field") }
//! record(ai, QT2) { field(VAL, "6") alias(QT2ALIAS) }
//! alias("QT2", QT2B)
//!
//! dbgf QT1.VAL -> 5   QT1.DESC -> "quoted type and field"
//! dbgf QT2.VAL -> 6   QT2ALIAS.VAL -> 6   QT2B.VAL -> 6
//! ```
//!
//! The port took the type and the field name as barewords ONLY and the record
//! and alias names as quoted strings ONLY — so each position accepted one of
//! C's two forms and rejected the other with a hard `DbParseError`. Tier 1: a
//! `.db` C loads must load.

use std::collections::HashMap;

use epics_base_rs::server::db_loader::parse_db;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// The C file above, verbatim.
const C_MEASURED_DB: &str = r#"
record("ai", "QT1") { field("VAL", "5") field(DESC, "quoted type and field") }
record(ai, QT2) { field(VAL, "6") alias(QT2ALIAS) }
alias("QT2", QT2B)
"#;

#[epics_macros_rs::epics_test]
async fn the_file_softioc_loads_loads() {
    let (db, _) = IocBuilder::new()
        .db_string(C_MEASURED_DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    for (pv, want) in [("QT1", 5.0), ("QT2", 6.0), ("QT2ALIAS", 6.0), ("QT2B", 6.0)] {
        let rec = db.get_record(pv).unwrap_or_else(|| panic!("{pv}"));
        assert_eq!(
            rec.read().record.get_field("VAL"),
            Some(EpicsValue::Double(want)),
            "{pv}.VAL"
        );
    }
    let rec = db.get_record("QT1").unwrap();
    let desc = rec.read().common.desc.clone();
    assert_eq!(desc, "quoted type and field");
}

/// Each `tokenSTRING` position, both forms, one case per position — the
/// bareword and the quoted spelling must parse to the SAME definition.
#[test]
fn both_lexical_forms_are_the_same_token_in_every_position() {
    // (bareword spelling, quoted spelling)
    let pairs = [
        // record type
        (
            r#"record(ai, "R") { field(VAL, "1") }"#,
            r#"record("ai", "R") { field(VAL, "1") }"#,
        ),
        // record name
        (
            r#"record(ai, R) { field(VAL, "1") }"#,
            r#"record(ai, "R") { field(VAL, "1") }"#,
        ),
        // field name
        (
            r#"record(ai, "R") { field(VAL, "1") }"#,
            r#"record(ai, "R") { field("VAL", "1") }"#,
        ),
        // in-record alias name
        (
            r#"record(ai, "R") { field(VAL, "1") alias(A) }"#,
            r#"record(ai, "R") { field(VAL, "1") alias("A") }"#,
        ),
        // info tag
        (
            r#"record(ai, "R") { field(VAL, "1") info(Q:group, "g") }"#,
            r#"record(ai, "R") { field(VAL, "1") info("Q:group", "g") }"#,
        ),
    ];

    for (bare, quoted) in pairs {
        let b = parse_db(bare, &HashMap::new())
            .unwrap_or_else(|e| panic!("bareword form rejected: {bare}\n{e}"));
        let q = parse_db(quoted, &HashMap::new())
            .unwrap_or_else(|e| panic!("quoted form rejected: {quoted}\n{e}"));
        assert_eq!(b.len(), 1);
        assert_eq!(
            (
                &b[0].record_type,
                &b[0].name,
                &b[0].fields,
                &b[0].aliases,
                &b[0].info_tags
            ),
            (
                &q[0].record_type,
                &q[0].name,
                &q[0].fields,
                &q[0].aliases,
                &q[0].info_tags
            ),
            "the two spellings are one token to C:\n  {bare}\n  {quoted}"
        );
    }
}

/// The standalone `alias(record, newname)` directive — both args, both forms.
#[test]
fn standalone_alias_takes_both_forms_in_both_args() {
    for text in [
        r#"record(ai,"R"){} alias("R", "N")"#,
        r#"record(ai,"R"){} alias(R, N)"#,
        r#"record(ai,"R"){} alias("R", N)"#,
        r#"record(ai,"R"){} alias(R, "N")"#,
    ] {
        let defs = parse_db(text, &HashMap::new()).unwrap_or_else(|e| panic!("{text}\n{e}"));
        assert_eq!(defs[0].aliases, vec!["N".to_string()], "{text}");
    }
}

/// A `breaktable` name is a `tokenSTRING` too.
#[test]
fn breaktable_name_takes_both_forms() {
    for text in [
        "breaktable(typeK) { 0 0  1 1 }",
        "breaktable(\"typeK\") { 0 0  1 1 }",
    ] {
        parse_db(text, &HashMap::new()).unwrap_or_else(|e| panic!("{text}\n{e}"));
    }
}

/// The reader is not a free-for-all: a position that C lexes as a keyword
/// (`tokenFIELD`) is still a keyword, and a token that is neither a bareword
/// nor a quoted string is still a parse error.
#[test]
fn a_non_token_string_is_still_refused() {
    assert!(
        parse_db(r#"record(ai,"R") { "field"(VAL,"1") }"#, &HashMap::new()).is_err(),
        "`field` is tokenFIELD, a keyword — quoting it is a syntax error in C too"
    );
    assert!(
        parse_db(r#"record(ai,"R") { field(,"1") }"#, &HashMap::new()).is_err(),
        "an empty field name is not a tokenSTRING"
    );
}
