//! The C simulation-mode contract — `recGbl.c:421-457` (`recGblSaveSimm`,
//! `recGblCheckSimm`, `recGblInitSimm`, `recGblGetSimm`) — as the single owner
//! of a record's SIMM transition.
//!
//! # R12-61 — SIMM=YES with an unset SIML and an unset SIOL must still simulate
//!
//! C's `readValue` dispatches on SIMM alone; the SIML/SIOL links are read
//! INSIDE that dispatch, never as a precondition for it. And an unset link is a
//! CONSTANT link (`dbLink.c::dbLinkIsConstant`), whose `dbConstGetValue`
//! (`dbConstLink.c:219-225`) returns SUCCESS with the caller's buffer
//! untouched. So for `longinRecord.c:411-421`:
//!
//! ```c
//! case menuYesNoYES: {
//!     recGblSetSevr(prec, SIMM_ALARM, prec->sims);       /* unconditional */
//!     status = dbGetLink(&prec->siol, DBR_LONG, &prec->sval, 0, 0);
//!     if (status == 0) {                                 /* constant: yes */
//!         prec->val = prec->sval;
//!         prec->udf = FALSE;
//!     }
//! ```
//!
//! an unset SIOL yields `val = sval` — the "simulate against a constant" idiom
//! (`caput REC.SIMM 1; caput REC.SVAL 42`). The pre-fix port returned
//! `NotSimulated` before SIMM was even read whenever SIML and SIOL were both
//! empty, so the idiom was a complete no-op on every record type.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::longin::LonginRecord;
use epics_base_rs::types::EpicsValue;

/// `caput REC.SIMM 1; caput REC.SVAL 42; caput REC.PROC 1` on a `longin` with
/// NO SIML and NO SIOL: C reads SIMM (YES), raises SIMM_ALARM at SIMS, reads
/// the (constant, unset) SIOL — status 0, SVAL untouched — and publishes
/// `VAL = SVAL = 42` with UDF cleared.
#[tokio::test]
async fn simm_yes_with_unset_siml_and_siol_simulates_from_sval() {
    let db = PvDatabase::new();
    let mut li = LonginRecord::new(7); // VAL = 7 (the pre-simulation value)
    li.sims = 2; // SIMS = MAJOR, so the SIMM_ALARM is observable
    // SIML and SIOL are left unset — the case the pre-fix gate short-circuited.
    db.add_record("SIMCONST", Box::new(li)).await.unwrap();

    db.put_pv("SIMCONST.SVAL", EpicsValue::Long(42))
        .await
        .unwrap();
    db.put_pv("SIMCONST.SIMM", EpicsValue::Short(1))
        .await
        .unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("SIMCONST", &mut v, 0)
        .await
        .unwrap();

    let val = db.get_pv("SIMCONST").await.unwrap();
    assert_eq!(
        val,
        EpicsValue::Long(42),
        "C `val = sval` on the status-0 constant SIOL read; got {val:?}"
    );

    let rec = db.get_record("SIMCONST").await.unwrap();
    let inst = rec.read().await;
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Major,
        "C raises recGblSetSevr(SIMM_ALARM, prec->sims) independently of the SIOL read"
    );
    assert_eq!(
        inst.common.stat,
        epics_base_rs::server::recgbl::alarm_status::SIMM_ALARM
    );
    assert!(!inst.common.udf, "C clears UDF on the status-0 SIOL read");
}

/// The same record with SIMM back at NO must NOT simulate: the SVAL is ignored
/// and the real (soft, empty INP) device path runs, leaving VAL alone.
#[tokio::test]
async fn simm_no_with_unset_links_does_not_simulate() {
    let db = PvDatabase::new();
    let mut li = LonginRecord::new(7);
    li.sims = 2;
    db.add_record("SIMOFF", Box::new(li)).await.unwrap();

    db.put_pv("SIMOFF.SVAL", EpicsValue::Long(42))
        .await
        .unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("SIMOFF", &mut v, 0)
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("SIMOFF").await.unwrap(),
        EpicsValue::Long(7),
        "SIMM=NO must not copy SVAL into VAL"
    );
    let rec = db.get_record("SIMOFF").await.unwrap();
    assert_ne!(
        rec.read().await.common.stat,
        epics_base_rs::server::recgbl::alarm_status::SIMM_ALARM,
        "SIMM=NO raises no SIMM_ALARM"
    );
}
