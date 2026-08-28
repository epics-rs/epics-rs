//! R8-2 — calcout/scalcout/acalcout carry the CLCV/OCLV expression-validity
//! fields, and they hold C's `postfix()` RETURN STATUS.
//!
//! C `calcoutRecord.c::special:326-345` (and sCalcoutRecord.c:462-481,
//! aCalcoutRecord.c:469-491) does `prec->clcv = postfix(prec->calc, ...)`,
//! posts DBE_VALUE for the field, and returns 0 — the put is ACCEPTED with a
//! garbage expression, unlike calcRecord, which fails it (R8-1). The stored
//! value is postfix()'s return, i.e. 0 or **-1** (postfix.c:239,507;
//! sCalcPostfix.c:873-881; aCalcPostfix.c:801-809) — never the CALC_ERR_* code
//! and never a generic 1, which is what acalcout.rs used to store.
//!
//! Fields are DBF_LONG (calcoutRecord.dbd.pod:729,1049; sCalcoutRecord.dbd:75,438).

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

/// C's `postfix()` failure status, what CLCV/OCLV read back after a bad put.
const POSTFIX_ERR: i32 = -1;

/// C `db_post_events(prec, &prec->clcv, DBE_VALUE)` (calcoutRecord.c:335) —
/// CLCV is not `pp(TRUE)`, so `special()`'s explicit post is the ONLY thing
/// that tells a monitoring client the expression went invalid.
#[epics_macros_rs::epics_test]
async fn a_calc_put_posts_the_clcv_monitor() {
    let db = PvDatabase::new();
    db.add_record("COM", Box::new(CalcoutRecord::default()))
        .await
        .unwrap();
    let mut sub = DbSubscription::subscribe(&db, "COM.CLCV")
        .await
        .expect("CLCV is a served field");

    db.put_record_field_from_ca("COM", "CALC", EpicsValue::String("1+".into()))
        .await
        .unwrap();

    let posted =
        epics_base_rs::runtime::task::timeout(std::time::Duration::from_secs(1), sub.recv())
            .await
            .expect("special() must post CLCV");
    assert_eq!(posted, Some(EpicsValue::Long(POSTFIX_ERR)));
}

#[epics_macros_rs::epics_test]
async fn calcout_bad_calc_put_is_accepted_and_lands_in_clcv() {
    let db = PvDatabase::new();
    db.add_record("CO", Box::new(CalcoutRecord::default()))
        .await
        .unwrap();

    db.put_record_field_from_ca("CO", "CALC", EpicsValue::String("A+B".into()))
        .await
        .unwrap();
    let rec = db.get_record("CO").unwrap();
    assert_eq!(
        rec.read().record.get_field("CLCV"),
        Some(EpicsValue::Long(0))
    );

    // C ACCEPTS the put (special() returns 0) and records the failure in CLCV.
    db.put_record_field_from_ca("CO", "CALC", EpicsValue::String("1+".into()))
        .await
        .expect("calcout special() returns 0 even for an uncompilable CALC");
    assert_eq!(
        rec.read().record.get_field("CLCV"),
        Some(EpicsValue::Long(POSTFIX_ERR))
    );

    // OCAL drives OCLV the same way.
    db.put_record_field_from_ca("CO", "OCAL", EpicsValue::String("A*(".into()))
        .await
        .unwrap();
    assert_eq!(
        rec.read().record.get_field("OCLV"),
        Some(EpicsValue::Long(POSTFIX_ERR))
    );
    db.put_record_field_from_ca("CO", "OCAL", EpicsValue::String("A*2".into()))
        .await
        .unwrap();
    assert_eq!(
        rec.read().record.get_field("OCLV"),
        Some(EpicsValue::Long(0))
    );
}

#[epics_macros_rs::epics_test]
async fn scalcout_clcv_oclv_track_scalcpostfix_status() {
    let db = PvDatabase::new();
    db.add_record("SC", Box::new(ScalcoutRecord::default()))
        .await
        .unwrap();
    let rec = db.get_record("SC").unwrap();

    db.put_record_field_from_ca("SC", "CALC", EpicsValue::String("A+B".into()))
        .await
        .unwrap();
    assert_eq!(
        rec.read().record.get_field("CLCV"),
        Some(EpicsValue::Long(0))
    );

    db.put_record_field_from_ca("SC", "CALC", EpicsValue::String("1+".into()))
        .await
        .expect("scalcout accepts the put");
    assert_eq!(
        rec.read().record.get_field("CLCV"),
        Some(EpicsValue::Long(POSTFIX_ERR))
    );

    // sCalcPostfix("") returns 0 with an empty program (sCalcPostfix.c:432-434),
    // unlike base postfix(), which calls the empty expression CALC_ERR_NULL_ARG.
    db.put_record_field_from_ca("SC", "CALC", EpicsValue::String("".into()))
        .await
        .unwrap();
    assert_eq!(
        rec.read().record.get_field("CLCV"),
        Some(EpicsValue::Long(0))
    );

    db.put_record_field_from_ca("SC", "OCAL", EpicsValue::String("AA[".into()))
        .await
        .unwrap();
    assert_eq!(
        rec.read().record.get_field("OCLV"),
        Some(EpicsValue::Long(POSTFIX_ERR))
    );
}

#[epics_macros_rs::epics_test]
async fn acalcout_bad_calc_stores_minus_one_not_a_generic_one() {
    let db = PvDatabase::new();
    db.add_record("AC", Box::new(AcalcoutRecord::default()))
        .await
        .unwrap();
    let rec = db.get_record("AC").unwrap();

    db.put_record_field_from_ca("AC", "CALC", EpicsValue::String("1+".into()))
        .await
        .unwrap();
    // Pre-fix this read back 1; C `aCalcPostfix` returns -1
    // (aCalcPostfix.c:801-809).
    assert_eq!(
        rec.read().record.get_field("CLCV"),
        Some(EpicsValue::Long(POSTFIX_ERR))
    );

    db.put_record_field_from_ca("AC", "OCAL", EpicsValue::String("A*(".into()))
        .await
        .unwrap();
    assert_eq!(
        rec.read().record.get_field("OCLV"),
        Some(EpicsValue::Long(POSTFIX_ERR))
    );
}
