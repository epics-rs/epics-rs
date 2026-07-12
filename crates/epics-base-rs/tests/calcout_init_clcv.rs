//! R9-3 — calcout compiles CALC and OCAL at INIT unconditionally, so an empty
//! field inits with the validity code C's `postfix()` returns for it: -1.
//!
//! C `calcoutRecord.c::init_record:190,199`:
//!
//!     prec->clcv = postfix(prec->calc, prec->rpcl, &error_number);
//!     ...
//!     prec->oclv = postfix(prec->ocal, prec->orpc, &error_number);
//!
//! There is no emptiness test on either line, and base `postfix()`
//! (`postfix.c:235-240`) answers an empty expression with CALC_ERR_NULL_ARG and
//! `return -1`. A stock calcout — whose OCAL defaults to "" — therefore comes up
//! with OCLV = -1 in C, and `field(CALC,"")` comes up with CLCV = -1.
//!
//! The port compiled only a NON-empty field at init, leaving the code at 0. That
//! also made CLCV depend on where the value came from: a put of "" (which always
//! went through `special` -> compile) landed -1, while the same "" from the db
//! file landed 0. C makes no such distinction.

use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::types::EpicsValue;

/// C `postfix()`'s failure status.
const POSTFIX_ERR: i64 = -1;

fn clcv(rec: &CalcoutRecord) -> i64 {
    match rec.get_field("CLCV") {
        Some(EpicsValue::Long(v)) => i64::from(v),
        other => panic!("expected Long CLCV, got {other:?}"),
    }
}

fn oclv(rec: &CalcoutRecord) -> i64 {
    match rec.get_field("OCLV") {
        Some(EpicsValue::Long(v)) => i64::from(v),
        other => panic!("expected Long OCLV, got {other:?}"),
    }
}

/// The stock record: CALC empty, OCAL empty -> C inits BOTH validity codes to -1.
#[test]
fn empty_calc_and_ocal_init_to_postfix_err() {
    let mut rec = CalcoutRecord::default();
    rec.init_record(0).unwrap();
    assert_eq!(clcv(&rec), POSTFIX_ERR, "C: clcv = postfix(\"\") = -1");
    assert_eq!(oclv(&rec), POSTFIX_ERR, "C: oclv = postfix(\"\") = -1");
}

/// A record whose CALC came from the db file compiles at init like any other —
/// a good CALC is 0, and an empty OCAL beside it is still -1.
#[test]
fn db_file_calc_compiles_at_init_and_empty_ocal_stays_err() {
    let mut rec = CalcoutRecord::default();
    rec.put_field("CALC", EpicsValue::String("A+B".into()))
        .unwrap();
    rec.init_record(0).unwrap();
    assert_eq!(clcv(&rec), 0, "C: clcv = postfix(\"A+B\") = 0");
    assert_eq!(oclv(&rec), POSTFIX_ERR, "C: oclv = postfix(\"\") = -1");
}

/// A garbage CALC from the db file is logged, never fatal, and lands in CLCV
/// (`calcoutRecord.c:191-196` — recGblRecordError + errlogPrintf, no return).
#[test]
fn bad_db_file_calc_lands_in_clcv_and_init_still_succeeds() {
    let mut rec = CalcoutRecord::default();
    rec.put_field("CALC", EpicsValue::String("A+".into()))
        .unwrap();
    assert!(
        rec.init_record(0).is_ok(),
        "C: init_record does not fail on a bad CALC"
    );
    assert_eq!(clcv(&rec), POSTFIX_ERR);
}

/// The init path and the put path now agree, which is the point: the same empty
/// string produces the same CLCV whichever way it arrived.
#[test]
fn init_and_put_agree_on_the_same_expression() {
    let mut from_db = CalcoutRecord::default();
    from_db
        .put_field("CALC", EpicsValue::String("".into()))
        .unwrap();
    from_db.init_record(0).unwrap();

    let mut from_put = CalcoutRecord::default();
    from_put.init_record(0).unwrap();
    from_put
        .put_field("CALC", EpicsValue::String("".into()))
        .unwrap();
    from_put.special("CALC", true).unwrap();

    assert_eq!(clcv(&from_db), clcv(&from_put));
    assert_eq!(clcv(&from_db), POSTFIX_ERR);
}
