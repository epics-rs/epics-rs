//! A `busy` record left at VAL=1 withholds its put-callback, exactly as C
//! does — and the NEXT `ca_put_callback` still lands.
//!
//! C `busyRecord.c:271` calls `recGblFwdLink(prec)` only for `val == 0 ||
//! oval == 0`, and `recGblFwdLink` (`recGbl.c:295`) is where
//! `dbNotifyCompletion` lives. So a put that leaves VAL non-zero leaves the
//! client's callback outstanding on purpose; that is the record type's whole
//! contract, and `caput -c` on it is meant to hang until something writes
//! "Done".
//!
//! Two regressions in one file, because they are two halves of one bug:
//!
//! 1. A deleted `is_put_complete() == (self.val == 0)` override said the
//!    right thing through the wrong method — the framework honoured it
//!    without the forward-link half — and, worse, it exposed (2).
//! 2. With the callback legitimately withheld, the client gives up and exits.
//!    If the record's notify slot is not released then (C
//!    `rsrvFreePutNotify` → `dbNotifyCancel`, `camessage.c:1637`), the record
//!    is wedged for good: every later put queues behind a completion that can
//!    never arrive and writes nothing, so `caput 1;2;2;3` posts one event
//!    instead of C's three.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(busy, "B") { field(ZNAM,"Done") field(ONAM,"Busy") }
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn drain(rx: &mut EventReader) -> Vec<EpicsValue> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev.snapshot.value.clone());
    }
    out
}

/// The oracle's `camonitor` + `caput -c 1;2;2;3` on VAL: three VALUE events
/// (1, 2, 3), the repeated 2 coalesced by the `mlst != val` monitor gate.
/// Every put after the first arrives while the previous client's callback is
/// still outstanding and its receiver already dropped — the sequential
/// `caput -c` the oracle runs — and every one of them must still write.
#[epics_macros_rs::epics_test]
async fn busy_val_notify_sequence_posts_each_change() {
    let db = build().await;
    let r = db.get_record("B").unwrap();
    let mut rx = {
        let mut inst = r.write();
        inst.add_subscriber("VAL", 1, DbFieldType::Enum, EventMask::VALUE.bits())
            .unwrap()
    };

    for v in [1.0f64, 2.0, 2.0, 3.0] {
        let held = db
            .put_record_field_from_ca("B", "VAL", EpicsValue::Double(v))
            .await
            .unwrap_or_else(|e| panic!("caput B {v}: {e:?}"));
        // VAL is non-zero on every one of these, so C skips `recGblFwdLink`
        // and the callback stays owed. Dropping `held` at the end of the
        // iteration is the client giving up and exiting.
        assert!(
            !held.is_sync(),
            "busy withholds the put-callback while VAL is non-zero (val={v})"
        );
    }

    assert_eq!(
        drain(&mut rx),
        vec![
            EpicsValue::Enum(1),
            EpicsValue::Enum(2),
            EpicsValue::Enum(3)
        ],
        "each out-of-range VAL put stores raw and posts DBE_VALUE (2, 3 render \
         as \"Illegal_Value\"); the repeated 2 coalesces on the mlst==val gate"
    );

    // The raw out-of-range index round-trips (C stores it; get_enum_str renders
    // "Illegal_Value").
    let inst = r.read();
    assert_eq!(inst.record.get_field("VAL"), Some(EpicsValue::Enum(3)));
}

/// The other direction of the same gate: a put that lands VAL back on 0 runs
/// `recGblFwdLink`, so the callback completes on that cycle.
#[epics_macros_rs::epics_test]
async fn a_busy_put_back_to_done_completes_its_callback() {
    let db = build().await;

    let held = db
        .put_record_field_from_ca("B", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert!(!held.is_sync(), "VAL=1 withholds the callback");
    drop(held);

    let held = db
        .put_record_field_from_ca("B", "VAL", EpicsValue::Double(0.0))
        .await
        .unwrap();
    assert!(
        held.is_sync(),
        "VAL=0 satisfies busyRecord.c:271, so the cycle reaches recGblFwdLink \
         and completes the put-notify"
    );
}
