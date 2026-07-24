//! R8-1 — `caput calc.CALC "<uncompilable>"` must FAIL at the client.
//!
//! C `calcRecord.c::special` (lines 144-155) runs `postfix()` on the CALC
//! string `dbPut` has already stored and returns `S_db_badField` when it does
//! not compile. `dbPut` (dbAccess.c:1399-1405) keeps the stored string but
//! `goto done`s past the field's monitor post, and `dbPutField` skips the
//! `pp(TRUE)` process, so rsrv answers the client `ECA_PUTFAIL`.
//!
//! The port used to do `self.rpcl = compile(&s).ok()` inside `put_field` and
//! return `Ok(())` — the client saw a successful write and the record silently
//! kept an uncompilable expression.

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::types::EpicsValue;

const ECA_PUTFAIL: u32 = 160; // (20 << 3) | CA_K_WARNING

#[epics_macros_rs::epics_test]
async fn bad_calc_put_is_rejected_but_the_string_is_stored() {
    let db = PvDatabase::new();
    db.add_record("CALCREC", Box::new(CalcRecord::new("A+1")))
        .await
        .unwrap();

    let err = db
        .put_record_field_from_ca("CALCREC", "CALC", EpicsValue::String("1+".into()))
        .await
        .expect_err("C special(SPC_CALC) returns S_db_badField for an uncompilable CALC");
    assert!(matches!(err, CaError::BadField(_)), "got {err:?}");
    // rsrv `write_action` answers any non-zero db_put_field status with
    // ECA_PUTFAIL.
    assert_eq!(err.to_eca_status(), ECA_PUTFAIL);

    // C `dbPut` wrote the string BEFORE special() ran and does not roll it
    // back — `caget CALCREC.CALC` reads back what the client sent.
    let rec = db.get_record("CALCREC").unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.record.get_field("CALC"),
        Some(EpicsValue::String("1+".into()))
    );
}

#[epics_macros_rs::epics_test]
async fn empty_calc_put_is_rejected_like_c_postfix_null_arg() {
    let db = PvDatabase::new();
    db.add_record("CALCEMPTY", Box::new(CalcRecord::new("A+1")))
        .await
        .unwrap();

    // C `postfix("")` → CALC_ERR_NULL_ARG, return -1 (postfix.c:235-240), so
    // special() raises S_db_badField for an empty CALC too.
    let err = db
        .put_record_field_from_ca("CALCEMPTY", "CALC", EpicsValue::String("".into()))
        .await
        .expect_err("empty CALC is CALC_ERR_NULL_ARG in C");
    assert!(matches!(err, CaError::BadField(_)), "got {err:?}");
}

#[epics_macros_rs::epics_test]
async fn good_calc_put_still_succeeds_and_recompiles() {
    let db = PvDatabase::new();
    db.add_record("CALCOK", Box::new(CalcRecord::new("A+1")))
        .await
        .unwrap();

    db.put_record_field_from_ca("CALCOK", "A", EpicsValue::Double(2.0))
        .await
        .unwrap();
    db.put_record_field_from_ca("CALCOK", "CALC", EpicsValue::String("A*10".into()))
        .await
        .unwrap();

    // CALC is pp(TRUE) (calcRecord.dbd.pod:569-575), so the accepted put
    // processes the record with the newly compiled RPCL.
    let rec = db.get_record("CALCOK").unwrap();
    let inst = rec.read();
    assert_eq!(inst.record.get_field("VAL"), Some(EpicsValue::Double(20.0)));
}
