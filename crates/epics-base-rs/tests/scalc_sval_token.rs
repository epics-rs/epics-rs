//! R6-71 — the `SVAL` token of the string-calc engine.
//!
//! synApps sCalc has a string counterpart to `VAL`: `SVAL`
//! (`sCalcPostfix.c:188` `{"SVAL", 0, 0, 1, OPERAND, FETCH_SVAL}`), which
//! `sCalcPerform` resolves to `psresult` — the record's *previous string
//! result* (`sCalcPerform.c:927-932`):
//!
//! ```c
//! case FETCH_SVAL:
//!     INC(ps);
//!     ps->s = &(ps->local_string[0]);
//!     ps->s[0] = '\0';
//!     strncpy(ps->s, psresult, SCALC_STRING_SIZE);
//!     break;
//! ```
//!
//! `sCalcoutRecord` passes `&pcalc->val, pcalc->sval` for CALC (`:357-359`)
//! and `&pcalc->oval, pcalc->osv` for OCAL (`:768-770`), so SVAL reads the
//! previous SVAL in CALC and the previous OSV in OCAL. The port had no such
//! token at all: `SVAL+'x'` failed to compile.
//!
//! swait is deliberately absent here: it evaluates its CALC with the *numeric*
//! `calcPerform(&pwait->a, &pwait->val, pwait->rpcl)` (`swaitRecord.c:409`),
//! which takes no `psresult`, and the numeric `postfix()` element table has no
//! SVAL token — so neither C nor the port has an SVAL in swait.

use std::collections::HashSet;

use epics_base_rs::calc::{
    CalcError, ExprKind, StackValue, StringInputs, compile, scalc, scalc_compile,
};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

/// SVAL pushes the previous *string* result, not the previous numeric one —
/// the two live in separate cells (C `psresult` vs `presult`).
#[test]
fn sval_pushes_the_previous_string_result_not_the_previous_val() {
    let mut inputs = StringInputs::new();
    inputs.prev_val = 42.0;
    inputs.prev_sval = "prev".into();

    assert_eq!(
        scalc("SVAL", &mut inputs).unwrap(),
        StackValue::Str("prev".into())
    );
    assert_eq!(
        scalc("VAL", &mut inputs).unwrap(),
        StackValue::Double(42.0),
        "the SVAL keyword must not shadow VAL"
    );
    // sCalc concatenates with `+` (sCalcPerform.c ADD on two strings); `CAT`
    // is an aCalc function and is not part of this engine's string dialect.
    assert_eq!(
        scalc("SVAL+'x'", &mut inputs).unwrap(),
        StackValue::Str("prevx".into())
    );
}

/// Boundary: a fresh evaluation has no previous string result. C zeroes the
/// stack cell before the `strncpy` (`ps->s[0] = '\0'`), and `sCalcoutRecord`
/// starts with an empty SVAL, so SVAL is the empty string on the first pass.
#[test]
fn sval_of_a_fresh_evaluation_is_the_empty_string() {
    let mut inputs = StringInputs::new();
    assert_eq!(
        scalc("SVAL", &mut inputs).unwrap(),
        StackValue::Str(Default::default())
    );
    assert_eq!(
        scalc("SVAL+'a'", &mut inputs).unwrap(),
        StackValue::Str("a".into())
    );
}

/// `CompiledExpr::kind` names the engine the program was compiled FOR — the C
/// compiler being emulated — not a type inferred from the opcodes. Every
/// sCalc program is a `String`-engine program, whether or not it touches a
/// string.
#[test]
fn scalc_programs_carry_the_string_engine_kind() {
    assert_eq!(scalc_compile("SVAL").unwrap().kind, ExprKind::String);
    assert_eq!(scalc_compile("VAL").unwrap().kind, ExprKind::String);
    assert_eq!(compile("VAL").unwrap().kind, ExprKind::Numeric);
}

/// R6-77 — the numeric engine rejects SVAL at COMPILE time.
///
/// C's `postfix()` element table has no SVAL entry, so the symbol is never
/// lexed: the infix text is left unconsumed and `postfix()` returns
/// `CALC_ERR_SYNTAX` (`postfix.c:475-477`). A `calc`/`calcout` record with
/// `CALC = "SVAL"` therefore reports `CLCV != 0` at load — it never reaches
/// `calcPerform`.
///
/// Pre-fix the port shared one compiler across calc/sCalc/aCalc: SVAL compiled
/// into a numeric program and only blew up at first process, with
/// `CalcError::Internal`.
#[test]
fn numeric_calc_rejects_sval() {
    assert_eq!(compile("SVAL").unwrap_err(), CalcError::Syntax);
    assert_eq!(compile("SVAL+'x'").unwrap_err(), CalcError::Syntax);
    // C's CALC_ERR_SYNTAX == 11 — the code a record parks in CLCV.
    assert_eq!(compile("SVAL").unwrap_err().code(), 11);

    // The whole string-only token class, not just SVAL: string literals and
    // the sCalc string functions are equally absent from postfix.c's table.
    for expr in ["'abc'", "\"abc\"", "PRINTF('%d',A)", "LEN(AA)", "STR(A)"] {
        assert_eq!(
            compile(expr).unwrap_err(),
            CalcError::Syntax,
            "numeric calc must reject the string-only expression {expr:?} at compile time"
        );
    }

    // ...and the sCalc engine still accepts every one of them.
    assert!(scalc_compile("SVAL+'x'").is_ok());
    assert!(scalc_compile("PRINTF('%d',A)").is_ok());
}

/// The other direction of the same split: the array-only tokens of
/// `aCalcPostfix.c` are not in `postfix.c`'s table either, and the string
/// engine's table (`sCalcPostfix.c`) has neither the array functions nor,
/// symmetrically, does the array engine have the string functions.
#[test]
fn engines_reject_each_others_tokens_at_compile_time() {
    use epics_base_rs::calc::acalc_compile;

    // Array tokens: aCalc only.
    for expr in ["IX", "AVG(AA)", "CAT(AA,BB)", "ARR(A)"] {
        assert_eq!(
            compile(expr).unwrap_err(),
            CalcError::Syntax,
            "numeric calc must reject array token expression {expr:?}"
        );
        assert_eq!(
            scalc_compile(expr).unwrap_err(),
            CalcError::Syntax,
            "sCalc must reject array token expression {expr:?}"
        );
        assert!(acalc_compile(expr).is_ok(), "aCalc must accept {expr:?}");
    }

    // String tokens: sCalc only.
    for expr in ["SVAL", "PRINTF('%d',A)", "'abc'"] {
        assert_eq!(
            acalc_compile(expr).unwrap_err(),
            CalcError::Syntax,
            "aCalc must reject string token expression {expr:?}"
        );
    }

    // `UNTIL` is in the sCalc and aCalc tables, not in postfix.c's. (R10-9: the
    // body has to be an assignment — UNTIL_END leaves the condition on the stack,
    // so `UNTIL 1; 42` ends at depth 2 and is Incomplete even in sCalc.)
    assert_eq!(compile("UNTIL 1; A:=1").unwrap_err(), CalcError::Syntax);
    assert!(scalc_compile("UNTIL 1; A:=1").is_ok());
}

/// R6-77, swait half — swait's CALC is compiled by the **numeric** `postfix()`
/// (C `swaitRecord.c:304,561`) and evaluated by `calcPerform` (`:409`), so its
/// grammar is epics-base calc's, not sCalc's: a string expression does not
/// compile, CLCV is set, and VAL is never assigned.
///
/// The port previously ran swait through the sCalc engine and then coerced the
/// `StackValue` back to a double with `s.parse().unwrap_or(0.0)`. `'42'` is the
/// discriminator: the string engine compiles it, produces `Str("42")`, and the
/// coercion lands **42.0** in VAL — a value C's swait can never produce, since
/// its compiler cannot lex a string literal at all.
#[epics_macros_rs::epics_test]
async fn swait_calc_uses_the_numeric_engine_and_rejects_a_string_expression() {
    use epics_base_rs::server::records::swait::SwaitRecord;

    let db = PvDatabase::new();

    let mut good = SwaitRecord::default();
    good.put_field("CALC", EpicsValue::String("3+4".into()))
        .unwrap();
    good.special("CALC", true).unwrap();
    db.add_record("SW_NUM", Box::new(good)).await.unwrap();

    let mut bad = SwaitRecord::default();
    bad.put_field("CALC", EpicsValue::String("'42'".into()))
        .unwrap();
    bad.special("CALC", true).unwrap();
    db.add_record("SW_STR", Box::new(bad)).await.unwrap();

    for name in ["SW_NUM", "SW_STR"] {
        let mut visited = HashSet::new();
        db.process_record_with_links(name, &mut visited, 0)
            .await
            .unwrap();
    }

    let num = db.get_record("SW_NUM").unwrap();
    assert_eq!(
        num.read().record.get_field("VAL"),
        Some(EpicsValue::Double(7.0)),
        "a numeric swait CALC must still evaluate"
    );

    let strr = db.get_record("SW_STR").unwrap();
    assert_eq!(
        strr.read().record.get_field("VAL"),
        Some(EpicsValue::Double(0.0)),
        "a string swait CALC must not compile (C postfix() has no string tokens), \
         so VAL keeps its initial value — pre-fix the string engine evaluated it \
         and parked 42.0 here"
    );
}

/// scalcout CALC: SVAL is the record's previous SVAL, so a self-referential
/// `SVAL+'x'` accumulates across process cycles (C `:357-359` passes
/// `pcalc->sval` as `psresult`).
#[epics_macros_rs::epics_test]
async fn scalcout_calc_sval_reads_the_previous_sval() {
    let db = PvDatabase::new();

    let mut sc = ScalcoutRecord::default();
    sc.put_field("CALC", EpicsValue::String("SVAL+'x'".into()))
        .unwrap();
    sc.special("CALC", true).unwrap();
    db.add_record("SC_SVAL", Box::new(sc)).await.unwrap();

    for expected in ["x", "xx", "xxx"] {
        let mut visited = HashSet::new();
        db.process_record_with_links("SC_SVAL", &mut visited, 0)
            .await
            .unwrap();

        let rec = db.get_record("SC_SVAL").unwrap();
        let sval = rec.read().record.get_field("SVAL");
        assert_eq!(
            sval,
            Some(EpicsValue::String(expected.into())),
            "SVAL+'x' must append to the previous SVAL"
        );
    }
}

/// scalcout OCAL (DOPT=Use_OVAL): C passes `pcalc->osv` as `psresult`
/// (`:768-770`), so SVAL inside OCAL is the previous **OSV** — not the SVAL
/// that CALC produced this very cycle.
#[epics_macros_rs::epics_test]
async fn scalcout_ocal_sval_reads_the_previous_osv_not_the_current_sval() {
    let db = PvDatabase::new();

    let mut sc = ScalcoutRecord::default();
    // CALC parks a constant, distinctive SVAL for this cycle.
    sc.put_field("CALC", EpicsValue::String("'abc'".into()))
        .unwrap();
    sc.special("CALC", true).unwrap();
    sc.put_field("OCAL", EpicsValue::String("SVAL+'Z'".into()))
        .unwrap();
    sc.special("OCAL", true).unwrap();
    sc.put_field("DOPT", EpicsValue::Short(1)).unwrap(); // Use_OVAL
    sc.put_field("OOPT", EpicsValue::Short(0)).unwrap(); // Every_Time
    db.add_record("SC_OSV", Box::new(sc)).await.unwrap();

    // If SVAL in OCAL wrongly read the CALC result, OSV would be "abcZ" on
    // every cycle. Reading the previous OSV makes it grow "Z", "ZZ", "ZZZ".
    for expected in ["Z", "ZZ", "ZZZ"] {
        let mut visited = HashSet::new();
        db.process_record_with_links("SC_OSV", &mut visited, 0)
            .await
            .unwrap();

        let rec = db.get_record("SC_OSV").unwrap();
        let inst = rec.read();
        assert_eq!(
            inst.record.get_field("SVAL"),
            Some(EpicsValue::String("abc".into())),
            "CALC still owns SVAL"
        );
        assert_eq!(
            inst.record.get_field("OSV"),
            Some(EpicsValue::String(expected.into())),
            "SVAL inside OCAL is the previous OSV (C psresult = pcalc->osv)"
        );
    }
}
