//! A failed or empty compile leaves a PROGRAM behind, and running it fails.
//!
//! Every failure path in all three C compilers writes `END_EXPRESSION` into the
//! output buffer before returning -1 (`postfix.c:238,506`; `sCalcPostfix.c:429,880`;
//! `aCalcPostfix.c:434,808`), and the empty expression IS that program in sCalc
//! and aCalc (`sCalcPostfix.c:432-434`, `aCalcPostfix.c:439-441`). Every
//! evaluator then refuses to run it: `calcPerform` leaves the stack empty and
//! fails its closing `ptop != stack + 1` test (`calcPerform.c:418-419`), and the
//! synApps two check `*post == END_EXPRESSION` up front (`sCalcPerform.c:396`,
//! `aCalcPerform.c:312-314`).
//!
//! So in C a broken CALC alarms on EVERY process, not once at compile time, and
//! VAL keeps its previous value. The port modelled the same state as "no program"
//! (`Option::None`) and each record then skipped the evaluation, so a record C
//! reports as INVALID looked healthy and quietly kept computing nothing.
//!
//! Compiled ground truth (postfix + perform of all three engines, linked against
//! the real sources), `res` pre-set to 12345:
//!
//! ```text
//! infix ""     base  compile=-1 err=12  rpn[0]=END  perform=-1  res=12345
//!              sCalc compile= 0 err= 0  rpn[0]=END  perform=-1  res=12345
//!              aCalc compile= 0 err= 0  rpn[0]=END  perform=-1  res=12345
//! infix "1+"   all three: compile=-1 err= 8  rpn[0]=END  perform=-1  res=12345
//! infix "@#$"  all three: compile=-1 err=11  rpn[0]=END  perform=-1  res=12345
//! infix "   "  all three: compile=-1 err= 8  rpn[0]=END  perform=-1  res=12345
//! ```

use epics_base_rs::calc::{ArrayInputs, NumericInputs, StringInputs};
use epics_base_rs::calc::{
    CalcError, CompiledExpr, ExprKind, acalc_compile, acalc_eval, compile, eval, scalc_compile,
    scalc_eval,
};

// ---------------------------------------------------------------- the engines

/// base `postfix("")` is CALC_ERR_NULL_ARG; sCalc's and aCalc's ACCEPT it. That
/// is the one place the three compilers disagree, and it is wire-visible in
/// CLCV (0 vs -1).
#[test]
fn the_empty_expression_compiles_only_in_scalc_and_acalc() {
    assert_eq!(compile("").err(), Some(CalcError::NullArg));
    assert!(scalc_compile("").is_ok());
    assert!(acalc_compile("").is_ok());
    assert!(scalc_compile("").unwrap().is_empty());
    assert!(acalc_compile("").unwrap().is_empty());
}

/// Whitespace is NOT the empty expression: C only short-circuits `*psrc == '\0'`,
/// so "   " walks the normal path and comes out CALC_ERR_INCOMPLETE (8) — the
/// port used to compile it to a program that quietly returned 0.
#[test]
fn a_whitespace_only_expression_is_incomplete_not_empty() {
    assert_eq!(compile("   ").err(), Some(CalcError::Incomplete));
    assert_eq!(scalc_compile("   ").err(), Some(CalcError::Incomplete));
    assert_eq!(acalc_compile("\t").err(), Some(CalcError::Incomplete));
}

/// The core of the finding: the empty program RUNS, and running it FAILS. Not
/// 0.0, not "skip the evaluation" — an error, in all three engines.
#[test]
fn every_engine_refuses_to_run_the_empty_program() {
    assert_eq!(
        eval(
            &CompiledExpr::empty(ExprKind::Numeric),
            &mut NumericInputs::new()
        ),
        Err(CalcError::EmptyProgram)
    );
    assert_eq!(
        scalc_eval(
            &CompiledExpr::empty(ExprKind::String),
            &mut StringInputs::new()
        ),
        Err(CalcError::EmptyProgram)
    );
    assert_eq!(
        acalc_eval(
            &CompiledExpr::empty(ExprKind::Array),
            &mut ArrayInputs::new(4)
        ),
        Err(CalcError::EmptyProgram)
    );
}

/// An accepted-but-empty sCalc/aCalc CALC still fails at run time — the compile
/// status (CLCV=0) and the run-time status (-1) are different answers, and C
/// gives both.
#[test]
fn an_accepted_empty_scalc_acalc_expression_still_fails_to_run() {
    assert_eq!(
        scalc_eval(&scalc_compile("").unwrap(), &mut StringInputs::new()),
        Err(CalcError::EmptyProgram)
    );
    assert_eq!(
        acalc_eval(&acalc_compile("").unwrap(), &mut ArrayInputs::new(4)),
        Err(CalcError::EmptyProgram)
    );
}

// ---------------------------------------------------------------- the records
//
// C runs perform() unconditionally in all five records, so an empty or
// uncompilable CALC is CALC_ALARM on every process with VAL untouched.

use epics_base_rs::server::record::{AlarmSeverity, CommonFields, Record};
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::swait::SwaitRecord;

/// The real observable (R10-67): the alarm the record RAISES, read out of the
/// pending `nsta`/`nsev` its own `check_alarms` — C's `checkAlarms()`, which the
/// framework calls on every cycle right before `evaluate_alarms` — writes.
/// This used to be a `CALC_ALARM` pseudo-field no DBD declares, which the
/// framework read back through a hardcoded record-type list.
fn alarmed(rec: &mut dyn Record) -> bool {
    let mut common = CommonFields::default();
    rec.check_alarms(&mut common);
    common.nsta == epics_base_rs::server::recgbl::alarm_status::CALC_ALARM
        && common.nsev == AlarmSeverity::Invalid
}

/// calc: `calcRecord.c:121-123`.
#[test]
fn calc_alarms_every_process_on_an_empty_or_broken_calc() {
    for expr in ["", "   ", "1+", "@#$"] {
        let mut rec = CalcRecord::default();
        rec.calc = expr.into();
        rec.init_record(0).unwrap();
        rec.val = 42.0;

        rec.process().unwrap();
        assert!(
            alarmed(&mut rec),
            "calc CALC={expr:?} must raise CALC_ALARM"
        );
        assert_eq!(rec.val, 42.0, "a failed calcPerform must not write VAL");

        // "every process", not just the first.
        rec.process().unwrap();
        assert!(
            alarmed(&mut rec),
            "calc CALC={expr:?} must alarm on EVERY process"
        );
    }
}

/// calcout: `calcoutRecord.c:238-241`.
#[test]
fn calcout_alarms_every_process_on_an_empty_or_broken_calc() {
    for expr in ["", "1+"] {
        let mut rec = CalcoutRecord::default();
        rec.calc = expr.into();
        rec.init_record(0).unwrap();
        rec.val = 42.0;

        rec.process().unwrap();
        assert!(
            alarmed(&mut rec),
            "calcout CALC={expr:?} must raise CALC_ALARM"
        );
        assert_eq!(rec.val, 42.0);
        rec.process().unwrap();
        assert!(alarmed(&mut rec));
    }
}

/// swait: `swaitRecord.c:409-410`. The port discarded both the compile error and
/// the eval error here, so a broken CALC was completely silent.
#[test]
fn swait_alarms_every_process_on_an_empty_or_broken_calc() {
    for expr in ["", "1+"] {
        let mut rec = SwaitRecord::default();
        rec.calc = expr.into();
        rec.init_record(0).unwrap();
        rec.val = 42.0;

        rec.process().unwrap();
        assert!(
            alarmed(&mut rec),
            "swait CALC={expr:?} must raise CALC_ALARM"
        );
        assert_eq!(rec.val, 42.0, "a failed calcPerform must not write VAL");
        rec.process().unwrap();
        assert!(alarmed(&mut rec));
    }
}

/// scalcout: `sCalcoutRecord.c:357-363`. A failed sCalcPerform also forces the
/// VAL=-1 / SVAL="***ERROR***" sentinel, which the record already implements —
/// this pins that the EMPTY CALC now reaches it.
#[test]
fn scalcout_alarms_and_takes_the_error_sentinel_on_an_empty_calc() {
    for expr in ["", "1+"] {
        let mut rec = ScalcoutRecord::default();
        rec.calc = expr.into();
        rec.init_record(0).unwrap();
        rec.process().unwrap();

        assert!(
            alarmed(&mut rec),
            "scalcout CALC={expr:?} must raise CALC_ALARM"
        );
        assert_eq!(rec.val, -1.0, "C forces VAL=-1 on a failed sCalcPerform");
        assert_eq!(rec.sval.as_str_lossy(), "***ERROR***");
    }
    // ...and CLCV still distinguishes them: sCalcPostfix ACCEPTS the empty
    // expression (0) and rejects the incomplete one (-1).
    let mut rec = ScalcoutRecord::default();
    rec.calc = "".into();
    rec.init_record(0).unwrap();
    assert_eq!(rec.clcv, 0, "sCalcPostfix(\"\") returns 0, not -1");

    let mut rec = ScalcoutRecord::default();
    rec.calc = "1+".into();
    rec.init_record(0).unwrap();
    assert_eq!(rec.clcv, -1);
}

/// acalcout: `aCalcoutRecord.c:263` + `afterCalc:304-305` (cstat != 0 →
/// CALC_ALARM).
#[test]
fn acalcout_alarms_every_process_on_an_empty_or_broken_calc() {
    for expr in ["", "1+"] {
        let mut rec = AcalcoutRecord::default();
        rec.calc = expr.into();
        rec.init_record(0).unwrap();

        rec.process().unwrap();
        assert!(
            alarmed(&mut rec),
            "acalcout CALC={expr:?} must raise CALC_ALARM"
        );
        rec.process().unwrap();
        assert!(alarmed(&mut rec));
    }
}

/// A CALC that DOES compile keeps working — the guard must not fire on real
/// programs.
#[test]
fn a_valid_calc_is_untouched() {
    let mut rec = CalcRecord::default();
    rec.calc = "A+1".into();
    rec.a = 41.0;
    rec.init_record(0).unwrap();
    rec.process().unwrap();
    assert_eq!(rec.val, 42.0);
    assert!(!alarmed(&mut rec));
}
