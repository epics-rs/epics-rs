//! **A parsed record remembers where its HEAD ended, not where its body did.**
//!
//! C decides a record exists in the `record_head` action — `dbCreateRecord`
//! runs from it (`dbYacc.y:227-235`, `dbLexRoutines.c:1128`) — so every
//! refusal of the head is reported with the closing `)` still the last token
//! the lexer matched. Measured on `softIoc` built from `~/work/epics-base`
//! (`R7.0.10`), an unknown record type prints:
//!
//! ```text
//! ERROR: Record type 'nosuchtype' for record 'U1' not found
//! ERROR:  at or before ')' in path "."  file "unk.db" line 1
//!
//!  1 | record(nosuchtype, "U1") {
//! ```
//!
//! and, with the same head split over two lines, ` … line 2` and the echo of
//! line 2 — the line the `)` sits on, never the line of the `{` or of the `}`
//! far below.
//!
//! [`DbRecordDef`] had no line at all, so a consumer refusing a record after
//! the parse had nothing to locate it with and printed the refusal bare. This
//! gate is on the parsed value rather than on a stream because the loader that
//! prints it is a different owner; what this file has to guarantee is that the
//! number handed over is C's.
//!
//! [`DbRecordDef`]: epics_base_rs::server::db_loader::DbRecordDef

use std::collections::HashMap;

use epics_base_rs::server::db_loader::parse_db;

/// One case per shape the head can take, not one per file story.
///
///   * `head_and_body_on_one_line` — the ordinary shape;
///   * `split_head` — the `)` a line below the `record`, which is the case
///     that tells a head line from a record-start line;
///   * `no_body` — `record(ai,"X")` with no braces, C's first `record_body`
///     alternative;
///   * `after_blank_and_comment` — the count is over the FLATTENED text, so
///     lines C reads and discards still advance it;
///   * `body_spanning_lines` — the body's own length must not move the head.
#[test]
fn the_line_is_the_one_the_closing_paren_sits_on() {
    for (what, text, want) in [
        (
            "head_and_body_on_one_line",
            "record(ai, \"R1\") { field(DESC, \"x\") }\n",
            1,
        ),
        (
            "split_head",
            "record(ai,\n       \"R1\")\n{\n    field(DESC, \"x\")\n}\n",
            2,
        ),
        ("no_body", "record(ai, \"R1\")\n", 1),
        (
            "after_blank_and_comment",
            "\n# a comment\n\nrecord(ai, \"R1\") {\n}\n",
            4,
        ),
        (
            "body_spanning_lines",
            "record(ai, \"R1\") {\n    field(DESC, \"x\")\n    field(SCAN, \"1 second\")\n}\n",
            1,
        ),
    ] {
        let recs = parse_db(text, &HashMap::new()).expect("the fixture parses");
        assert_eq!(recs.len(), 1, "{what}: one record");
        assert_eq!(recs[0].line, want, "{what}");
    }
}

/// Two records in one file each get their own head line, and the second is not
/// the first's.
///
/// The boundary a single-record case cannot show: the parser reuses one `line`
/// counter across the whole text, so a head line captured at the wrong moment
/// reads as the PREVIOUS record's, or as the line the loop happened to reach.
#[test]
fn each_record_carries_its_own_head_line() {
    let text = "record(ai, \"R1\") {\n    field(DESC, \"x\")\n}\n\nrecord(bo, \"R2\") {\n}\n";
    let recs = parse_db(text, &HashMap::new()).expect("the fixture parses");
    assert_eq!(
        recs.iter().map(|r| r.line).collect::<Vec<_>>(),
        vec![1, 5],
        "each head line is its own"
    );
}

/// A record with no text behind it carries `0`, which is the loader's "print
/// no position" — the same value [`DbFieldDef::new`] uses for a field built
/// the same way.
///
/// [`DbFieldDef::new`]: epics_base_rs::server::db_loader::DbFieldDef::new
#[test]
fn a_record_built_by_hand_has_no_line() {
    use epics_base_rs::server::db_loader::{DbFieldDef, DbRecordDef};

    let built = DbRecordDef {
        record_type: "ai".into(),
        name: "R1".into(),
        fields: vec![DbFieldDef::new("DESC", "x")],
        aliases: Vec::new(),
        info_tags: Vec::new(),
        line: 0,
    };
    assert_eq!(built.line, 0);
    assert_eq!(built.fields[0].line, 0);
}
