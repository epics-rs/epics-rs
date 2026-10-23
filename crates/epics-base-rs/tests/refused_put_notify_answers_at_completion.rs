//! A put-notify whose conversion is refused still drives its record
//! (`didPut = 1` with `ppn->status = notifyError`, `dbNotify.c:528-530`), and
//! C answers it from `dbNotifyCompletion`: on an async cycle, at the cycle's
//! end. A blocking PVA put hears the failure there too, as pvxs
//! `doneCallback` does (`ioc/singlesource.cpp:262-263`).

use std::time::Duration;

use epics_base_rs::server::database::{ProcessMode, PvDatabase};
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::types::EpicsValue;

const REC: &str = "RREF:CALCOUT";

async fn async_calcout() -> PvDatabase {
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
    db
}

#[epics_macros_rs::epics_test]
async fn a_refusal_on_an_async_cycle_completes_failed_after_it() {
    let db = async_calcout().await;
    let refused = db
        .put_record_field_notify(REC, "A", EpicsValue::String("nope".into()))
        .await
        .expect_err("the conversion is refused");
    let done = refused
        .completion
        .into_handle()
        .expect("the refused put still drove the async ODLY cycle");
    let done = epics_base_rs::runtime::task::timeout(Duration::from_secs(5), done)
        .await
        .unwrap();
    assert!(done.is_err(), "the completion carries the refusal");
    assert!(!db.get_record(REC).unwrap().read().is_processing());
}

#[epics_macros_rs::epics_test]
async fn a_blocking_client_put_reports_the_refusal_at_completion() {
    let db = async_calcout().await;
    let put = db.put_field_from_client(
        REC,
        "A",
        EpicsValue::String("nope".into()),
        ProcessMode::Passive,
        true,
    );
    let put = epics_base_rs::runtime::task::timeout(Duration::from_secs(5), put)
        .await
        .unwrap();
    assert!(put.is_err(), "the refused put is an error to the client");
    assert!(!db.get_record(REC).unwrap().read().is_processing());
}
