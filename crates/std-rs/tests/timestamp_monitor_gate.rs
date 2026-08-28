//! C `timestampRecord.c:152-163` `monitor()`:
//!
//! ```c
//! monitor_mask = recGblResetAlarms(ptimestamp);
//! monitor_mask |= DBE_VALUE | DBE_LOG;
//! if (strncmp(ptimestamp->oval, ptimestamp->val, sizeof(ptimestamp->val))) {
//!     db_post_events(ptimestamp, &(ptimestamp->val[0]), monitor_mask);
//!     db_post_events(ptimestamp, &ptimestamp->rval,     monitor_mask);
//!     strncpy(ptimestamp->oval, ptimestamp->val, sizeof(ptimestamp->val));
//! }
//! ```
//!
//! The `strncmp` on the rendered VAL string is the ONLY gate: RVAL is posted
//! from inside it, with VAL's mask, and with no test of RVAL's own value. So a
//! process cycle that crosses a second but re-renders the same string (TST
//! `HH:MM`, minute resolution) posts nothing at all — where the Rust port used
//! to route RVAL through generic change-detection and post it on every crossed
//! second (~59 spurious DBE_VALUE|DBE_LOG events a minute per subscriber).

#![cfg(tokio_backend)]
// Both cases read the posted monitor stream off a real CA server. The
// reactor-free `exec_backend` — selected on a host build by
// `EPICS_RS_BUILD_EXEC_BACKEND=thread`, and unconditionally on RTEMS and
// VxWorks — has no `epics_ca_rs::server::CaServer` to build, so this
// file has no subject there. `[[test]] required-features` cannot name a
// build-script cfg, which is why the gate is here and not in
// `Cargo.toml`.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{Local, Timelike};
use epics_base_rs::server::record::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use epics_ca_rs::server::CaServerBuilder;

/// Two process cycles a second apart, with TST=5 (`HH:MM`) so both render the
/// same VAL string: C posts neither VAL nor RVAL on the second cycle.
#[tokio::test]
async fn timestamp_rval_is_not_posted_when_the_val_string_is_unchanged() {
    // Align away from the minute boundary: a minute rollover between the two
    // PROCs would legitimately change VAL (and so post RVAL), which is the
    // other half of the gate — tested below. Waiting for the next minute costs
    // at most a few seconds and removes the flake entirely.
    let secs_left = 60 - Local::now().second();
    if secs_left < 5 {
        tokio::time::sleep(Duration::from_secs(u64::from(secs_left) + 1)).await;
    }

    let db_str = r#"
record(timestamp, "TS:MIN") {
    field(TST, "5")
}
"#;
    let server = CaServerBuilder::new()
        .register_record_type("timestamp", || Box::new(std_rs::TimestampRecord::default()))
        .db_string(db_str, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    let (mut rval_rx, mut val_rx) = {
        let rec = db.get_record("TS:MIN").unwrap();
        let mut inst = rec.write();
        let rval = inst
            .add_subscriber(
                "RVAL",
                1,
                DbFieldType::ULong,
                (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
            )
            .expect("RVAL subscription accepted");
        let val = inst
            .add_subscriber(
                "VAL",
                2,
                DbFieldType::String,
                (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
            )
            .expect("VAL subscription accepted");
        (rval, val)
    };

    // First cycle renders VAL for the first time: VAL changed (from empty), so
    // C's guard fires and both VAL and RVAL post.
    proc(&db, "TS:MIN").await;
    assert!(
        val_rx.try_recv().is_ok(),
        "first render changes VAL, so C's strncmp guard posts VAL"
    );
    let first_rval = rval_rx
        .try_recv()
        .expect("first render posts RVAL from inside the guard");
    let first_secs = ulong_of(&first_rval.snapshot.value);
    let first_val = server.get("TS:MIN.VAL").await.unwrap();

    // Cross a second boundary. RVAL (seconds past the EPICS epoch) necessarily
    // moves; the HH:MM string does not.
    tokio::time::sleep(Duration::from_millis(1_050)).await;
    proc(&db, "TS:MIN").await;

    assert_eq!(
        server.get("TS:MIN.VAL").await.unwrap(),
        first_val,
        "TST=5 renders HH:MM — the second cycle must land in the same minute \
         (test setup bug if not)"
    );
    let now_secs = ulong_of(&server.get("TS:MIN.RVAL").await.unwrap());
    assert!(
        now_secs > first_secs,
        "RVAL is refreshed on every process (C timestampRecord.c:94), so the \
         seconds count moved: {first_secs} -> {now_secs}"
    );

    // ...yet neither field posted: C's only gate is the VAL string.
    assert!(
        rval_rx.try_recv().is_err(),
        "RVAL posts only from inside the VAL-string guard \
         (timestampRecord.c:160) — an unchanged HH:MM string posts nothing, \
         even though RVAL itself changed"
    );
    assert!(
        val_rx.try_recv().is_err(),
        "VAL is unchanged, so C posts no VAL event either"
    );
}

/// The other half of the same gate: when the rendered string DOES change
/// (TST=4, `HH:MM:SS`, second resolution), C posts VAL *and* RVAL together,
/// both carrying VAL's `DBE_VALUE|DBE_LOG` mask.
#[tokio::test]
async fn timestamp_rval_posts_with_val_when_the_string_changes() {
    let db_str = r#"
record(timestamp, "TS:SEC") {
    field(TST, "4")
}
"#;
    let server = CaServerBuilder::new()
        .register_record_type("timestamp", || Box::new(std_rs::TimestampRecord::default()))
        .db_string(db_str, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    let (mut rval_rx, mut val_rx) = {
        let rec = db.get_record("TS:SEC").unwrap();
        let mut inst = rec.write();
        let rval = inst
            .add_subscriber(
                "RVAL",
                1,
                DbFieldType::ULong,
                (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
            )
            .expect("RVAL subscription accepted");
        let val = inst
            .add_subscriber(
                "VAL",
                2,
                DbFieldType::String,
                (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
            )
            .expect("VAL subscription accepted");
        (rval, val)
    };

    proc(&db, "TS:SEC").await;
    let first = rval_rx.try_recv().expect("first render posts RVAL");
    assert!(val_rx.try_recv().is_ok(), "first render posts VAL");

    tokio::time::sleep(Duration::from_millis(1_050)).await;
    proc(&db, "TS:SEC").await;

    let second = rval_rx
        .try_recv()
        .expect("the HH:MM:SS string moved, so the guard fires and RVAL posts");
    assert!(
        val_rx.try_recv().is_ok(),
        "the changed string posts VAL alongside RVAL"
    );
    assert!(
        ulong_of(&second.snapshot.value) > ulong_of(&first.snapshot.value),
        "the posted RVAL carries the fresh seconds count"
    );
    // C posts RVAL with VAL's own monitor_mask: DBE_VALUE|DBE_LOG (plus any
    // alarm bits, of which this record raises none).
    assert_eq!(
        second.mask,
        EventMask::VALUE | EventMask::LOG,
        "RVAL carries VAL's monitor_mask, not a mask of its own"
    );
}

async fn proc(db: &epics_base_rs::server::database::PvDatabase, pv: &str) {
    db.put_record_field_from_ca(pv, "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
}

/// `field(RVAL,DBF_ULONG)` (`timestampRecord.dbd:28`) — the EPICS-epoch seconds
/// are UNSIGNED. This used to unwrap an `EpicsValue::Long` and panic with "RVAL
/// is DBF_LONG": the port's hand-written field table declared it `DBF_LONG`, and
/// the test pinned that. The declaration is the `.dbd` now.
fn ulong_of(v: &EpicsValue) -> u32 {
    match v {
        EpicsValue::ULong(n) => *n,
        other => panic!("RVAL is DBF_ULONG, got {other:?}"),
    }
}
