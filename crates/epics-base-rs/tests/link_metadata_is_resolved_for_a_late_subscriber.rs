//! The cycle resolves link-backed metadata when — and only when — a consumer
//! exists, and it re-asks every cycle.
//!
//! A [`LinkBacking`] reaches a consumer only through
//! `RecordInstance::make_monitor_snapshot`, and every caller of that sits
//! inside a `self.subscribers.get(field)` hit, so a cycle on an unsubscribed
//! record resolved a map nothing could read. C never had that cost: its
//! metadata slots are `dbDb_lset` entries (`dbDbLink.c:414-415`) reached from
//! `dbGet`, not from `dbProcess`.
//!
//! The boundary the gate must not get wrong is the FIRST subscriber: a record
//! that has run unsubscribed cycles must resolve on the cycle after one
//! arrives, which is what fails if the gate is ever cached rather than asked
//! per cycle.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// PREC 1 and `mm`, deliberately unlike the calc's own PREC 7 and `V`, so
/// serving the record instead of the link is a visible failure.
async fn add_pair(db: &PvDatabase) {
    let mut src = AiRecord::new(1.0);
    src.egu = "mm".into();
    src.prec = 1;
    db.add_record("SRC", Box::new(src)).await.unwrap();

    let mut calc = CalcRecord::default();
    calc.egu = "V".into();
    calc.prec = 7;
    calc.set_inp_link(0, "SRC");
    calc.calc = "A+1".into();
    db.add_record("CALC", Box::new(calc)).await.unwrap();
}

/// The gated path: cycles with nothing subscribed still process.
#[epics_macros_rs::epics_test]
async fn an_unsubscribed_record_still_processes() {
    let db = PvDatabase::new();
    add_pair(&db).await;
    db.ioc_init().await;

    db.put_pv("SRC", EpicsValue::Double(4.0)).await.unwrap();
    db.process_record("CALC").await.unwrap();

    let val: EpicsValue = db.get_pv("CALC").unwrap();
    assert_eq!(
        val.to_f64(),
        Some(5.0),
        "a cycle that resolves no metadata still computes A+1"
    );
}

/// The owner path across the boundary: the gate is asked per cycle, so the
/// first subscriber's first post carries the TARGET's metadata even though
/// every earlier cycle skipped the resolve.
#[epics_macros_rs::epics_test]
async fn a_subscriber_arriving_after_unsubscribed_cycles_still_gets_the_targets_metadata() {
    let db = PvDatabase::new();
    add_pair(&db).await;
    db.ioc_init().await;

    // Cycles nobody is watching — the gate skips the resolve on every one.
    for _ in 0..3 {
        db.process_record("CALC").await.unwrap();
    }

    let rec = db.get_record("CALC").expect("record exists");
    let mut rx = rec
        .write()
        .add_subscriber(
            "A",
            1,
            DbFieldType::Double,
            (EventMask::VALUE | EventMask::LOG).bits(),
        )
        .expect("subscriber");

    db.put_pv("SRC", EpicsValue::Double(7.0)).await.unwrap();
    db.process_record("CALC").await.unwrap();

    let event = rx.try_recv().expect("A posts to its new subscriber");
    assert_eq!(
        event.snapshot.precision(),
        Some(1),
        "A is link-backed: the post must carry SRC's precision, never CALC's own"
    );
    assert_eq!(
        event
            .snapshot
            .units()
            .map(|u| u.as_str_lossy().into_owned()),
        Some("mm".to_string()),
        "and SRC's units, which only a resolved backing can supply"
    );
}
