//! busy completes its put-callback SYNCHRONOUSLY (like bo), so a sequence of
//! `ca_put_callback` writes to VAL each processes and posts `DBE_VALUE`.
//!
//! C `busyRecord.c:273` clears `pact = FALSE` at the tail of every process
//! cycle; the soft device support this port models (`devBusySoft.c::write_busy`
//! is a bare `dbPutLink`, never touching `pact`) leaves the record synchronous,
//! so `dbNotifyCompletion` fires each `ca_put_callback` immediately and the
//! next put is not refused.
//!
//! Regression: busy previously overrode `is_put_complete() == (val == 0)`,
//! modelling the asynBusy hold. But busy's `process()` is synchronous (never
//! `AsyncPendingNotify`), so the hold was a phantom: once VAL was driven to 1
//! the put-callback never completed, and every following `ca_put_callback` was
//! refused with `PutCallbackInProgress`. The oracle's `caput 1;2;2;3` then
//! posted only the first event (VAL=1 "Busy") instead of C's three
//! (Busy → Illegal_Value(2) → Illegal_Value(3)).

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const DB: &str = r#"
record(busy, "B") { field(ZNAM,"Done") field(ONAM,"Busy") }
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
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

/// The oracle's `camonitor` + `caput 1;2;2;3` on VAL: three VALUE events
/// (1, 2, 3), the repeated 2 coalesced by the `mlst != val` monitor gate. Each
/// `ca_put_callback` (WRITE_NOTIFY) must complete synchronously so the next one
/// is not refused with `PutCallbackInProgress`.
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
        // The WRITE_NOTIFY (`ca_put_callback`) path: `Ok` — never refused with
        // PutCallbackInProgress — and no held completion receiver (synchronous).
        let held = db
            .put_record_field_from_ca("B", "VAL", EpicsValue::Double(v))
            .await
            .unwrap_or_else(|e| panic!("caput B {v}: {e:?}"));
        assert!(
            held.is_sync(),
            "busy put-callback must complete synchronously (val={v})"
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
