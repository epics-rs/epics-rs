//! `PvDatabase::process_record_with_notify` — the QSRV
//! `record[process=true,block=true]` (Force + block) completion barrier.
//!
//! pvxs routes a blocking forced put through `dbProcessNotify`
//! (`ioc/singlesource.cpp:360-369`; `if forceProcessing==False doWait=false`
//! clears the wait for Inhibit only, never for Force), so the reply is withheld
//! until processing — including async device completion — finishes. The Rust
//! primitive mints a put-notify wait-set, registers it into the record's
//! `notify` slot, runs the full unconditional `process_record_with_links`
//! cycle (C `dbProcess`), and returns the completion receiver only when the
//! chain went async. A fully synchronous chain drains the wait-set inside
//! processing and returns `Ok(None)`.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

/// A fully synchronous forced process completes the barrier inside the
/// processing call: the wait-set drains before `process_record_with_notify`
/// returns, so it yields `Ok(None)` (no receiver to await) — and it processed
/// UNCONDITIONALLY (Force = C `dbProcess`), driving the OUT link synchronously.
#[epics_macros_rs::epics_test]
async fn force_block_sync_record_returns_none_and_processes() {
    let db = PvDatabase::new();
    db.add_record("TGT0", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    // scalcout with ODLY=0: CALC="42" → VAL=42=OVAL, OOPT=Every → OUT due,
    // no delay, so the whole cycle (incl. the OUT write) runs synchronously.
    let mut sc = ScalcoutRecord::default();
    sc.put_field("CALC", EpicsValue::String("42".into()))
        .unwrap();
    sc.special("CALC", true).unwrap();
    sc.oopt = 0;
    sc.put_field("ODLY", EpicsValue::Double(0.0)).unwrap();
    sc.put_field("OUT", EpicsValue::String("TGT0".into()))
        .unwrap();
    db.add_record("SC0", Box::new(sc)).await.unwrap();

    let completion = db
        .process_record_with_notify("SC0")
        .await
        .expect("process must succeed");
    assert!(
        completion.is_sync(),
        "a synchronous forced process drains the put-notify wait-set inside \
         processing, so no completion receiver is handed back"
    );

    // Force processed unconditionally: the OUT link drove TGT0.VAL = 42.
    let tgt = db.get_record("TGT0").unwrap();
    let v = tgt.read().record.get_field("VAL");
    assert_eq!(
        v,
        Some(EpicsValue::Double(42.0)),
        "Force = unconditional dbProcess must have driven the OUT write, got {v:?}"
    );
}

/// A forced process of an async (ODLY-PACT) record holds the barrier: the
/// record stays ACTIVE across the delay, so the wait-set is NOT drained inside
/// processing and `process_record_with_notify` hands back a completion
/// receiver (`Ok(Some(_))`). Getting a receiver at all is the barrier proof —
/// a bug that ignored block would have returned before the async work, either
/// `Ok(None)` or with the OUT already fired. The 100 s timer cannot fire in the
/// test, so the OUT stays deferred (DLYA=1, TGT unchanged).
#[epics_macros_rs::epics_test]
async fn force_block_async_record_withholds_completion_until_processing_done() {
    let db = PvDatabase::new();
    db.add_record("TGT1", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    // ODLY=100 s: the record defers the OUT write and stays PACT across the
    // (un-fireable) delay — the async shape a blocking forced put must wait on.
    let mut sc = ScalcoutRecord::default();
    sc.put_field("CALC", EpicsValue::String("42".into()))
        .unwrap();
    sc.special("CALC", true).unwrap();
    sc.oopt = 0;
    sc.put_field("ODLY", EpicsValue::Double(100.0)).unwrap();
    sc.put_field("OUT", EpicsValue::String("TGT1".into()))
        .unwrap();
    db.add_record("SC1", Box::new(sc)).await.unwrap();

    let completion = db
        .process_record_with_notify("SC1")
        .await
        .expect("process must succeed");
    assert!(
        completion.is_async(),
        "a forced process of an async (ODLY-PACT) record must withhold \
         completion — the reply barrier holds until the delay finishes"
    );

    // The barrier is genuinely async-pending: DLYA armed, OUT still deferred.
    let sc_rec = db.get_record("SC1").unwrap();
    let dlya = sc_rec.read().record.get_field("DLYA");
    assert_eq!(
        dlya,
        Some(EpicsValue::Short(1)),
        "ODLY cycle must arm DLYA (record held ACTIVE across the delay), got {dlya:?}"
    );
    let tgt = db.get_record("TGT1").unwrap();
    let v = tgt.read().record.get_field("VAL");
    assert_eq!(
        v,
        Some(EpicsValue::Double(0.0)),
        "OUT must stay deferred until the delay completes, got {v:?}"
    );
}
