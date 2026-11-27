//! A put-notify replayed from the restart list that drives no process cycle
//! ends where C's does (`dbNotify.c:263-268`): `restartCheck` hands the record
//! to the next queued notify and `callDone` answers this one with the put's
//! own status — `notifyOK` for a written non-pp field, `notifyError` for a
//! refused one (`:528-530`).

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::types::EpicsValue;

const REC: &str = "QUNP:CALCOUT";

/// Behind an ODLY cycle, queue `field` = `value` and then a pp put of A; the
/// outcome of each queued completion, or `None` if it never came.
async fn replay(field: &str, value: EpicsValue) -> (Option<bool>, Option<bool>) {
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

    let mut handles = Vec::new();
    for (field, value) in [
        ("A", EpicsValue::Double(1.0)),
        (field, value),
        ("A", EpicsValue::Double(2.0)),
    ] {
        handles.push(
            db.put_record_field_from_ca(REC, field, value)
                .await
                .expect("a queued put is answered pending")
                .into_handle()
                .expect("each put waits on the ODLY cycle ahead of it"),
        );
    }
    let mut outcomes = Vec::new();
    for done in handles {
        let done = epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, done).await;
        outcomes.push(done.ok().map(|r| r.is_ok()));
    }
    assert_eq!(outcomes[0], Some(true), "the first put owns a clean cycle");
    (outcomes[1], outcomes[2])
}

#[epics_macros_rs::epics_test]
async fn a_written_non_pp_replay_completes_ok_and_hands_the_record_on() {
    let (unprocessed, next) = replay("DESC", EpicsValue::String("x".into())).await;
    assert_eq!(unprocessed, Some(true), "the DESC write succeeded");
    assert_eq!(next, Some(true), "the put queued behind it ran");
}

#[epics_macros_rs::epics_test]
async fn a_refused_non_pp_replay_completes_failed_and_hands_the_record_on() {
    let (unprocessed, next) = replay("PREC", EpicsValue::String("nope".into())).await;
    assert_eq!(unprocessed, Some(false), "the PREC conversion was refused");
    assert_eq!(next, Some(true), "the put queued behind it ran");
}
