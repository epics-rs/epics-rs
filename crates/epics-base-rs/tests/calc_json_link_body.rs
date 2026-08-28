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

/// BOUNDARY: a single-quoted string. yajl lexes `'…'` and `"…"` with the same
/// routine, so both are strings in this dialect, and base's own test database
/// writes the calc expression that way: `linkRetargetLink.db:20-23` loads
/// `rec:j1.INP` as `{calc:{expr:'A+5', args:5}}` with `PINI YES`.
#[test]
fn a_single_quoted_expression_parses() {
    let calc = calc_of(r#"{calc:{expr:'A+5', args:[3]}}"#);
    assert_eq!(calc.expr, "A+5");
    assert_eq!(calc.args, vec![CalcArg::Literal(3.0)]);
}

/// BOUNDARY: a single-quoted string with a `"` inside it. The quote characters
/// are the delimiter, not part of the value, so re-spelling the token must
/// escape what the new delimiter would otherwise end.
#[test]
fn a_single_quoted_string_may_contain_a_double_quote() {
    let calc = calc_of(r#"{calc:{expr:'A', args:[{const:'say "hi"'}]}}"#);
    assert_eq!(calc.expr, "A");
    assert_eq!(
        calc.args,
        vec![CalcArg::Link(Box::new(ParsedLink::Constant(
            "say \"hi\"".to_string()
        )))]
    );
}

/// BOUNDARY: `args` as a bare scalar rather than an array. C reaches
/// `lnkCalc_integer` (`lnkCalc.c:130-141`) / `lnkCalc_double` (`:150-161`) at
/// `ps_args` and appends the number, and `lnkCalc_start_array` (`:319-329`)
/// only makes the bracketed spelling legal at the same state — so both are
/// accepted, and `linkRetargetLink.db:22` uses the scalar one.
#[test]
fn a_scalar_args_value_is_one_argument() {
    let calc = calc_of(r#"{calc:{expr:'A+5', args:5}}"#);
    assert_eq!(calc.expr, "A+5");
    assert_eq!(calc.args, vec![CalcArg::Literal(5.0)]);
}

/// BOUNDARY: the exact text base loads in its own test suite
/// (`linkRetargetLink.db:20-23`), whitespace and line breaks included.
#[test]
fn base_link_retarget_test_database_form_parses() {
    let calc =
        calc_of("{calc:{\n                expr:'A+5',\n                args:5\n             }}");
    assert_eq!(calc.expr, "A+5");
    assert_eq!(calc.args, vec![CalcArg::Literal(5.0)]);
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

/// BOUNDARY: the relaxed form must not DEGRADE. The failure this pins is not
/// "refused" but "accepted as something else": a reader that demands strict
/// JSON falls through the brace test and builds a `ParsedLink::Db` whose
/// record name is the whole JSON blob — a channel that can never connect, and
/// one that reports no error at load.
///
/// The three places base itself writes the relaxed form, at `R7.0.10`:
/// `linkRetargetLink.db:20-23` (single-quoted `expr`, unbracketed `args`,
/// across four lines), `lnkCalcTest.c:56-59`, and `linkRetargetLinkTest.c:81`
/// (`{calc:{expr:'A+5',args:[7]}}`, put at run time; `:78` is the `args:5`
/// spelling of the same link).
#[test]
fn the_relaxed_form_does_not_degrade_to_a_db_link() {
    for body in [
        "{calc:{\n                expr:'A+5',\n                args:5\n             }}",
        "{calc:{expr:'A+5',args:5}}",
        "{calc:{expr:'A+5',args:[7]}}",
        "{calc:{expr:'a',args:[{const:1}]}}",
    ] {
        let parsed = parse_link_v2(body);
        assert!(
            !matches!(parsed, ParsedLink::Db(_)),
            "{body} must not degrade to a Db link whose record name is the \
             JSON blob, got {parsed:?}"
        );
        assert!(
            matches!(parsed, ParsedLink::Calc(_)),
            "{body} is a calc link base loads, got {parsed:?}"
        );
    }
}

/// BOUNDARY: every arm of C's `time` guard refuses the WHOLE link.
///
/// `lnkCalc.c:180-182` is one test with three arms —
/// `len != 1 || (tinp = toupper(val[0])) < 'A' || tinp >= 'A' + CALCPERFORM_NARGS`
/// — and each returns `jlif_stop`, which `dbJLinkParse` turns into
/// `S_db_badField` (`dbJLink.c:426-438`): the field never loads. A `time`
/// value that is not a string at all reaches `lnkCalc_integer` (`:130-133`)
/// or `lnkCalc_double` (`:150-153`) in `ps_time` and stops there.
///
/// The refusal is the point, and it is why this asserts `ParsedLink::None`
/// rather than merely "not a Calc". The defect this pins had the port set
/// `time_source: None` and CONTINUE, which builds a live calc link that
/// silently takes the record's own stamp instead of the input's; the other
/// reachable wrong answer is a `ParsedLink::Db` whose record name is the
/// whole JSON blob. Both are excluded here by naming the outcome.
#[test]
fn every_bad_time_value_refuses_the_whole_link() {
    for (body, why) in [
        (
            r#"{calc:{expr:"A", time:"V"}}"#,
            "'V' is 'A' + 21, one past the top",
        ),
        (r#"{calc:{expr:"A", time:"Z"}}"#, "well past the top"),
        (r#"{calc:{expr:"A", time:""}}"#, "len != 1 (empty)"),
        (
            r#"{calc:{expr:"A", time:"AB"}}"#,
            "len != 1 (two characters)",
        ),
        (r#"{calc:{expr:"A", time:"1"}}"#, "toupper('1') < 'A'"),
        (r#"{calc:{expr:"A", time:"@"}}"#, "'@' is 'A' - 1"),
        (r#"{calc:{expr:"A", time:5}}"#, "not a string at all"),
    ] {
        assert_eq!(
            parse_link_v2(body),
            ParsedLink::None,
            "{body} must refuse the link — {why}"
        );
    }
}

/// The counter-boundary, so the fix cannot be "refuse everything": the two
/// ends of the accepted range and the case fold still build the link.
#[test]
fn the_accepted_time_range_still_builds_the_link() {
    assert_eq!(
        calc_of(r#"{calc:{expr:"A", time:"A"}}"#).time_source,
        Some('A')
    );
    assert_eq!(
        calc_of(r#"{calc:{expr:"A", time:"U"}}"#).time_source,
        Some('U')
    );
    assert_eq!(
        calc_of(r#"{calc:{expr:"A", time:"a"}}"#).time_source,
        Some('A')
    );
    assert_eq!(calc_of(r#"{calc:{expr:"A"}}"#).time_source, None);
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
