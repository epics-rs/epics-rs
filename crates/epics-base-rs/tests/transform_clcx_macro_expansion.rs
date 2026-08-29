//! A `CLCx` is compiled from the CONVERTED expression, not the raw one.
//!
//! C `transformRecord.c` never hands `sCalcPostfix` the stored infix. Both
//! compile sites run `getMacros` and then `convertExpression`
//! (`:426`/`:481-482` in `init_record`, `:682-684` in `special`), and
//! `convertExpression` (`:384-390`) is `convertShortcuts` followed by
//! `convertMacros`. A `CMTx` beginning with `$` defines a macro whose name is
//! `$` plus the comment's leading non-space run and whose replacement is that
//! channel's letter (`getMacros`, `:257-295`), matched case-insensitively and
//! longest-name-first (`sortMacros`, `:241-255`; `convertMacros`, `:297-329`).
//!
//! Compiling the raw text instead makes every macro-using expression a syntax
//! error, so the channel is never evaluated at all.
//!
//! Boundaries: macro defined before vs after the expression in the `.db`, a
//! macro that is a prefix of a longer one, a comment that defines no macro, the
//! case-insensitive match, the name's terminator, and the shortcut pass that
//! must run ahead of the macro pass.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::EpicsValue;

/// `field(...)` puts in `.db` order, then the record is loaded — which is where
/// C runs `getMacros` and compiles.
async fn load(db: &PvDatabase, name: &str, fields: &[(&str, &str)]) {
    let mut rec = TransformRecord::default();
    for (field, value) in fields {
        match value.parse::<f64>() {
            Ok(v) if field.len() == 1 => rec.put_field(field, EpicsValue::Double(v)).unwrap(),
            _ => rec
                .put_field(field, EpicsValue::String((*value).into()))
                .unwrap(),
        }
    }
    // COPT=Always — C's `transformCOPT_Always`, so no channel is gated by the
    // `new_value` test and every valid CLCx runs.
    rec.copt = 1;
    db.add_record(name, Box::new(rec)).await.unwrap();
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

fn channel(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// The lead's trigger: `CMTA="$in"` makes `$in` mean `A`, so `CLCB="$in*2"` is
/// `A*2`. C drives `T.B = 6`; compiling the raw text leaves CLCB uncompiled and
/// B at 0.
#[epics_macros_rs::epics_test]
async fn a_comment_defined_macro_expands_in_the_expression() {
    let db = PvDatabase::new();
    load(&db, "T", &[("CMTA", "$in"), ("A", "3"), ("CLCB", "$in*2")]).await;
    process(&db, "T").await;
    assert_eq!(channel(&db, "T.B"), 6.0, "$in must expand to A");
}

/// C compiles in `init_record`, after the whole record has loaded and after one
/// `getMacros` over the final CMTx set, so a `.db` that names the comment BELOW
/// the expression is no different. Compiling on the way in — what a put must do
/// for C's `special()` — is what would make the load order-dependent.
#[epics_macros_rs::epics_test]
async fn the_macro_lands_even_when_its_comment_loads_after_the_expression() {
    let db = PvDatabase::new();
    load(&db, "T", &[("CLCB", "$in*2"), ("A", "3"), ("CMTA", "$in")]).await;
    process(&db, "T").await;
    assert_eq!(channel(&db, "T.B"), 6.0, "load order must not matter");
}

/// `sortMacros` orders by DESCENDING name length so a longer name is tried
/// before any shorter name it starts with. Declaration order is the opposite of
/// the sorted order here, so an unsorted table would expand `$xy` as `$x`
/// followed by the stray character `y`.
#[epics_macros_rs::epics_test]
async fn the_longer_macro_name_wins_over_the_prefix_it_contains() {
    let db = PvDatabase::new();
    load(
        &db,
        "T",
        &[
            ("CMTA", "$x"),
            ("CMTB", "$xy"),
            ("A", "3"),
            ("B", "7"),
            ("CLCC", "$xy+1"),
        ],
    )
    .await;
    process(&db, "T").await;
    assert_eq!(channel(&db, "T.C"), 8.0, "$xy is B, not A followed by 'y'");
}

/// The name is `$` plus every non-space character (`:276` stops on `isspace`),
/// so a comment may carry prose after it; and a comment NOT starting with `$`
/// defines nothing at all, which leaves the expression uncompilable and the
/// channel untouched.
#[epics_macros_rs::epics_test]
async fn the_macro_name_ends_at_the_first_space_and_needs_the_leading_dollar() {
    let db = PvDatabase::new();
    load(
        &db,
        "T",
        &[("CMTA", "$in  the input"), ("A", "3"), ("CLCB", "$in*2")],
    )
    .await;
    process(&db, "T").await;
    assert_eq!(
        channel(&db, "T.B"),
        6.0,
        "prose after the name is not part of it"
    );

    let db = PvDatabase::new();
    load(
        &db,
        "U",
        &[("CMTA", "in"), ("A", "3"), ("B", "5"), ("CLCB", "$in*2")],
    )
    .await;
    process(&db, "U").await;
    assert_eq!(
        channel(&db, "U.B"),
        5.0,
        "no macro, so CLCB does not compile and B is never evaluated"
    );
}

/// `convertMacros` compares with `epicsStrnCaseCmp` (`:313`).
#[epics_macros_rs::epics_test]
async fn the_macro_match_is_case_insensitive() {
    let db = PvDatabase::new();
    load(&db, "T", &[("CMTA", "$IN"), ("A", "3"), ("CLCB", "$in*2")]).await;
    process(&db, "T").await;
    assert_eq!(channel(&db, "T.B"), 6.0);
}

/// `convertShortcuts` runs BEFORE `convertMacros` (`transformRecord.c:387-388`)
/// so that a user macro cannot shadow a built-in function spelling: with a
/// macro `$S` defined, `$S(` is consumed by the shortcut pass and never reaches
/// the macro pass, which would otherwise leave the uncompilable `A("7","%d")`.
///
/// This case does NOT fail on the pre-fix port — with no conversion at all the
/// expression compiled raw and gave the same 7. It is here to pin the ORDER of
/// the two passes now that both exist.
#[epics_macros_rs::epics_test]
async fn a_shortcut_is_expanded_before_the_macro_pass() {
    let db = PvDatabase::new();
    load(
        &db,
        "T",
        &[
            ("CMTA", "$S"),
            ("A", "3"),
            ("B", "5"),
            ("CLCB", "$S(\"7\",\"%d\")"),
        ],
    )
    .await;
    process(&db, "T").await;
    assert_eq!(
        channel(&db, "T.B"),
        7.0,
        "the shortcut pass consumed `$S(`, so the macro could not make it `A(`"
    );
}

/// The shortcut expansion itself, with no `CMTx` in play.
///
/// C's replacement text carries a leading `$` (`{"$S(", "$SSCANF("}`) and
/// `sCalcPostfix` has no `$SSCANF` element, so on C this expression stops
/// compiling the moment the conversion runs — measured against `libcalc` on
/// this host: `$S("7","%d")` status 0, `$SSCANF("7","%d")` status -1 error 11.
/// The port drops the `$` from the replacement and keeps the expression
/// working, which is the one deliberate deviation in this fix; `SSCANF` and
/// `$S` are the same element, so the value is unchanged.
#[epics_macros_rs::epics_test]
async fn the_shortcut_expansion_keeps_the_expression_compilable() {
    let db = PvDatabase::new();
    load(&db, "T", &[("B", "5"), ("CLCB", "$S(\"7\",\"%d\")")]).await;
    process(&db, "T").await;
    assert_eq!(
        channel(&db, "T.B"),
        7.0,
        "SSCANF( is the same element as $S(, so the value stands"
    );
}
