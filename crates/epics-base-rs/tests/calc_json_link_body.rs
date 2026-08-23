//! The body of a `{calc:…}` link, at each of its boundaries.
//!
//! C reference, all in `modules/database/src/std/link/`:
//!
//!   * `links.dbd.pod:165` gives the canonical example
//!     `{calc: {expr:"A*B", args:[{pva:"record"}, 1.5], prec:3}}` — unquoted
//!     keys, and a numeric literal sitting beside an embedded link in `args`.
//!     That form is not a courtesy: `yajl_alloc` returns a handle already
//!     flagged `yajl_allow_json5 | yajl_allow_comments` (`yajl.c:77`) and
//!     `dbJLinkParse` never clears it (`dbJLink.c:402-406`).
//!   * `lnkCalc.c:121-144` / `:146-170` — `lnkCalc_integer` and
//!     `lnkCalc_double` store a bare number as `clink->arg[clink->nArgs++]`,
//!     i.e. a constant argument value, with no link behind it.
//!   * `lnkCalc.c:135-139`, `:155-159`, `:346-350` — reaching
//!     `CALCPERFORM_NARGS` args is `jlif_stop`, a refusal rather than a
//!     truncation. `postfix.h:29` defines `CALCPERFORM_NARGS` as 21, which is
//!     also A..U; `links.dbd.pod:131` says "up to 24" and is wrong.
//!   * `lnkCalc.c:180-186` — `time` must be one character, is `toupper`ed,
//!     and must land in `'A' ..< 'A' + CALCPERFORM_NARGS`.

use epics_base_rs::calc::CALC_NARGS;
use epics_base_rs::server::record::{CalcArg, ParsedLink, parse_link_v2};

fn calc_of(text: &str) -> epics_base_rs::server::record::CalcLink {
    match parse_link_v2(text) {
        ParsedLink::Calc(c) => c,
        other => panic!("expected ParsedLink::Calc for {text}, got {other:?}"),
    }
}

/// BOUNDARY: unquoted keys. Base's own documented shape, and the one that
/// made the whole feature unreachable from a normal `.db` — strict
/// `serde_json` rejects it outright.
#[test]
fn an_unquoted_key_parses() {
    let calc = calc_of(r#"{calc: {expr:"A*B", args:[3, 1.5]}}"#);
    assert_eq!(calc.expr, "A*B");
    assert_eq!(
        calc.args,
        vec![CalcArg::Literal(3.0), CalcArg::Literal(1.5)]
    );
}

/// BOUNDARY: quoted keys still parse. Strict JSON is a subset of JSON5, so
/// the relaxed reader must not have traded one dialect for the other.
#[test]
fn a_quoted_key_still_parses() {
    let calc = calc_of(r#"{calc:{"expr":"A*B","args":[3,1.5]}}"#);
    assert_eq!(calc.expr, "A*B");
    assert_eq!(
        calc.args,
        vec![CalcArg::Literal(3.0), CalcArg::Literal(1.5)]
    );
}

/// BOUNDARY: a numeric literal is a stored constant, not a PV name. An
/// integer token and a double token take the same slot — C has two callbacks
/// writing one array (`lnkCalc.c:141`, `:161`).
#[test]
fn a_numeric_literal_arg_is_a_constant_not_a_channel() {
    let calc = calc_of(r#"{calc:{expr:"A+B", args:[3, 1.5]}}"#);
    assert_eq!(
        calc.args,
        vec![CalcArg::Literal(3.0), CalcArg::Literal(1.5)]
    );
    assert!(
        calc.args.iter().all(|a| !matches!(a, CalcArg::Link(_))),
        "a literal must not become a link to a record named \"3\""
    );
}

/// BOUNDARY: an embedded JSON link in `args` — the other half of
/// `links.dbd.pod:131`, "either a numeric literal or an embedded JSON link".
#[test]
fn an_embedded_link_arg_parses_beside_a_literal() {
    let calc = calc_of(r#"{calc: {expr:"A*B", args:[{pva:"record"}, 1.5], prec:3}}"#);
    assert_eq!(
        calc.args,
        vec![
            CalcArg::Link(Box::new(ParsedLink::Pva("record".to_string()))),
            CalcArg::Literal(1.5),
        ]
    );
}

/// BOUNDARY: an embedded link naming an unregistered type refuses the whole
/// link, the same way a top-level one does — the nested parse goes back
/// through the one owner rather than around it.
#[test]
fn an_embedded_link_of_an_unknown_type_refuses_the_calc_link() {
    assert!(
        !matches!(
            parse_link_v2(r#"{calc:{expr:"A", args:[{nosuch:1}]}}"#),
            ParsedLink::Calc(_)
        ),
        "an unknown embedded link type must not yield a Calc link"
    );
}

/// BOUNDARY: exactly `CALC_NARGS` args is the last accepted count.
#[test]
fn calc_nargs_args_are_accepted() {
    let args: Vec<String> = (0..CALC_NARGS).map(|i| i.to_string()).collect();
    let calc = calc_of(&format!(
        r#"{{calc:{{expr:"A", args:[{}]}}}}"#,
        args.join(",")
    ));
    assert_eq!(calc.args.len(), CALC_NARGS);
    assert_eq!(CALC_NARGS, 21, "postfix.h:29 CALCPERFORM_NARGS");
}

/// BOUNDARY: one more than `CALC_NARGS` refuses the link. C stops the parse
/// (`jlif_stop`) rather than dropping the overflow.
#[test]
fn one_arg_over_calc_nargs_refuses_the_link() {
    let args: Vec<String> = (0..=CALC_NARGS).map(|i| i.to_string()).collect();
    let text = format!(r#"{{calc:{{expr:"A", args:[{}]}}}}"#, args.join(","));
    assert!(
        !matches!(parse_link_v2(&text), ParsedLink::Calc(_)),
        "{} args must refuse the link",
        CALC_NARGS + 1
    );
}

/// BOUNDARY: `time` at the top of C's range. `'A' + CALCPERFORM_NARGS - 1`
/// is `'U'`, the last letter `toupper`-folded validation accepts.
#[test]
fn time_u_is_accepted() {
    let calc = calc_of(r#"{calc:{expr:"A", time:"U"}}"#);
    assert_eq!(calc.time_source, Some('U'));
}

/// BOUNDARY: `time` is case-folded. C runs `toupper((int) val[0])` before the
/// range test, so a lower-case letter is the same letter.
#[test]
fn time_lowercase_u_is_accepted_and_folded() {
    let calc = calc_of(r#"{calc:{expr:"A", time:"u"}}"#);
    assert_eq!(calc.time_source, Some('U'));
}

/// BOUNDARY: one past the top of the range. `'V'` is `'A' + 21`, which fails
/// C's `tinp >= 'A' + CALCPERFORM_NARGS` test and stops the parse.
#[test]
fn time_v_refuses_the_link() {
    assert!(
        !matches!(
            parse_link_v2(r#"{calc:{expr:"A", time:"V"}}"#),
            ParsedLink::Calc(_)
        ),
        "'V' is out of the A..U range and must refuse the link"
    );
}

/// The end-to-end trigger, in the shape a `.db` carries it: base's own
/// documented calc form with two numeric literals, loaded through the record
/// database and read back. `3 * 1.5` needs no input records at all, because
/// both arguments are constants.
#[epics_macros_rs::epics_test]
async fn the_documented_two_literal_form_loads_and_evaluates() {
    use epics_base_rs::server::ioc_builder::IocBuilder;
    use epics_base_rs::types::EpicsValue;
    use std::collections::HashMap;

    let db_text = concat!(
        "record(ai, \"A\") {\n",
        "    field(DTYP, \"Soft Channel\")\n",
        "    field(INP, {calc:{expr:\"A*B\", args:[3, 1.5]}})\n",
        "}\n"
    );
    let (db, _handles) = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .expect("base's documented calc form must load")
        .build()
        .await
        .expect("build");

    let mut visited = std::collections::HashSet::new();
    let link = db.get_record("A").unwrap().read().parsed_inp.clone();
    let value = db
        .read_link_value_soft(&link, true, &mut visited, 0)
        .expect("a calc link over two literals needs no records to evaluate");
    match value {
        EpicsValue::Double(v) => assert!((v - 4.5).abs() < 1e-9, "expected 3*1.5=4.5, got {v}"),
        other => panic!("expected Double, got {other:?}"),
    }
}
