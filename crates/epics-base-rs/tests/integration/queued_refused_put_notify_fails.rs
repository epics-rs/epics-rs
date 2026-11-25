//! A put-notify queued behind another (`dbNotify.c:213-219`) and refused on
//! its replay still processes the record, and its completion reports the
//! failure: `putCallback` sets `ppn->status = notifyError` with `didPut = 1`
//! (`dbNotify.c:528-530`). One case per refusal site: a pp field's conversion
//! and the PROC intercept's.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::types::EpicsValue;

const REC: &str = "QREF:CALCOUT";

/// A queued put-notify of `field` = "nope" behind an ODLY cycle: whether its
/// completion reported success.
async fn queued_refusal_succeeds(field: &str) -> bool {
    let db = PvDatabase::new();
    db.add_record(REC, Box::new(CalcoutRecord::default()))
        .await
        .unwrap();
    db.put_record_field_from_ca_no_notify(REC, "CALC", EpicsValue::String("A".into()))
        .await
        .unwrap();
    db.put_record_field_from_ca_no_notify(REC, "ODLY", EpicsValue::Double(0.2))
        .await
        .unwrap();

    let first = db
        .put_record_field_from_ca(REC, "A", EpicsValue::Double(1.0))
        .await
        .unwrap()
        .into_handle()
        .expect("the ODLY cycle is async");
    let queued = db
        .put_record_field_from_ca(REC, field, EpicsValue::String("nope".into()))
        .await
        .expect("a queued put is answered pending, not refused")
        .into_handle()
        .expect("the queued put waits for its replay");

    let wait = fact_budget::FACT_BUDGET;
    assert!(
        epics_base_rs::runtime::task::timeout(wait, first)
            .await
            .unwrap()
            .is_ok()
    );
    epics_base_rs::runtime::task::timeout(wait, queued)
        .await
        .unwrap()
        .is_ok()
}

#[epics_macros_rs::epics_test]
async fn a_queued_pp_field_refusal_completes_failed() {
    assert!(!queued_refusal_succeeds("A").await);
}

#[epics_macros_rs::epics_test]
async fn a_queued_proc_refusal_completes_failed() {
    assert!(!queued_refusal_succeeds("PROC").await);
}
