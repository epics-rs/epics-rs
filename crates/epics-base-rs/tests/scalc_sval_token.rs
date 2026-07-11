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
    CalcError, ExprKind, NumericInputs, StackValue, StringInputs, compile, eval, scalc,
    scalc_compile,
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
        StackValue::Str(String::new())
    );
    assert_eq!(
        scalc("SVAL+'a'", &mut inputs).unwrap(),
        StackValue::Str("a".into())
    );
}

/// C `sCalcPostfix.c:452` lists FETCH_SVAL among the opcodes that mark the
/// postfix USES_STRING, so an SVAL-only expression is a string expression.
#[test]
fn sval_marks_the_expression_as_string_typed() {
    assert_eq!(scalc_compile("SVAL").unwrap().kind, ExprKind::String);
    assert_eq!(scalc_compile("VAL").unwrap().kind, ExprKind::Numeric);
}

/// The numeric evaluator has no SVAL: C's `postfix()` element table cannot
/// emit FETCH_SVAL, so `calcPerform` never sees it. The port shares one
/// tokenizer across calc/sCalc/aCalc, so the token compiles — but the numeric
/// evaluator rejects it, exactly as it rejects every other string-only opcode.
#[test]
fn numeric_calc_rejects_sval() {
    let compiled = compile("SVAL").unwrap();
    let mut inputs = NumericInputs::new();
    assert_eq!(eval(&compiled, &mut inputs), Err(CalcError::Internal));
}

/// scalcout CALC: SVAL is the record's previous SVAL, so a self-referential
/// `SVAL+'x'` accumulates across process cycles (C `:357-359` passes
/// `pcalc->sval` as `psresult`).
#[tokio::test]
async fn scalcout_calc_sval_reads_the_previous_sval() {
    let db = PvDatabase::new();

    let mut sc = ScalcoutRecord::default();
    sc.put_field("CALC", EpicsValue::String("SVAL+'x'".into()))
        .unwrap();
    db.add_record("SC_SVAL", Box::new(sc)).await.unwrap();

    for expected in ["x", "xx", "xxx"] {
        let mut visited = HashSet::new();
        db.process_record_with_links("SC_SVAL", &mut visited, 0)
            .await
            .unwrap();

        let rec = db.get_record("SC_SVAL").await.unwrap();
        let sval = rec.read().await.record.get_field("SVAL");
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
#[tokio::test]
async fn scalcout_ocal_sval_reads_the_previous_osv_not_the_current_sval() {
    let db = PvDatabase::new();

    let mut sc = ScalcoutRecord::default();
    // CALC parks a constant, distinctive SVAL for this cycle.
    sc.put_field("CALC", EpicsValue::String("'abc'".into()))
        .unwrap();
    sc.put_field("OCAL", EpicsValue::String("SVAL+'Z'".into()))
        .unwrap();
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

        let rec = db.get_record("SC_OSV").await.unwrap();
        let inst = rec.read().await;
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
