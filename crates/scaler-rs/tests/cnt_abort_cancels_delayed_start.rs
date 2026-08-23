//! SC-1 — a `CNT` 1->0 abort must cancel the armed delayed-start timer.
//!
//! C `scalerRecord.c:643-647` cancels it as the FIRST statement of the
//! `USER_STATE_WAITING` abort arm:
//!
//! ```c
//! case USER_STATE_WAITING:
//!     /* We may have a watchdog timer going.  Cancel it. */
//!     if (pdelayCallback->timer) epicsTimerCancel(pdelayCallback->timer);
//!     pscal->us = USER_STATE_IDLE;
//! ```
//!
//! C can also survive a raced callback, because `delayCallbackFunc`
//! (`:216-231`) is a guarded two-line transition that never processes the
//! record. The port's re-entry is a whole `process()`, so a surviving timer
//! reaches the "done counting?" block (`:470-481`) and fires FLNK a second
//! time DLY seconds after the operator stopped the count.

use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServerBuilder;
use scaler_rs::ScalerRecord;
use std::collections::HashMap;
use std::time::Duration;

const DLY: f64 = 0.3;

async fn counter(server: &epics_ca_rs::server::CaServer) -> f64 {
    match server.get("SC:CNTR.VAL").await.unwrap() {
        EpicsValue::Double(v) => v,
        other => panic!("CNTR.VAL is DBF_DOUBLE, got {other:?}"),
    }
}

/// A CA put through the database, which honours `CNT`'s `pp(TRUE)` and
/// processes the record — `CaServer::put` alone does not.
async fn put_cnt(server: &epics_ca_rs::server::CaServer, cnt: i16) {
    server
        .database()
        .put_record_field_from_ca("SC", "CNT", EpicsValue::Short(cnt))
        .await
        .unwrap();
}

async fn abort_fixture() -> epics_ca_rs::server::CaServer {
    let db_str = r#"
record(scaler, "SC") {
    field(FREQ, "1000000")
    field(TP, "10.0")
    field(DLY, "0.3")
    field(FLNK, "SC:CNTR")
}
record(calc, "SC:CNTR") {
    field(CALC, "VAL+1")
}
"#;
    CaServerBuilder::new()
        .port(0)
        .register_record_type("scaler", || Box::new(ScalerRecord::default()))
        .db_string(db_str, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
}

/// The abort's own process cycle fires FLNK once (C `:476-481` — `ss` IDLE,
/// `pcnt` 0, `us` IDLE). Nothing may fire it again when the cancelled timer's
/// deadline passes.
#[tokio::test]
async fn cnt_abort_leaves_no_delayed_reentry() {
    let server = abort_fixture().await;

    put_cnt(&server, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let armed = counter(&server).await;
    assert_eq!(
        armed, 0.0,
        "waiting out DLY reaches no recGblFwdLink (C :470, us != IDLE)"
    );

    // The operator changes their mind well inside DLY.
    put_cnt(&server, 0).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let aborted = counter(&server).await;
    assert_eq!(
        aborted, 1.0,
        "the abort's own process cycle fires FLNK once"
    );

    // Past the cancelled deadline.
    tokio::time::sleep(Duration::from_secs_f64(DLY + 0.3)).await;
    assert_eq!(
        counter(&server).await,
        1.0,
        "the cancelled delayed-start timer must not process the record again"
    );
    let us = server.get("SC.US").await.unwrap();
    assert_eq!(us, EpicsValue::Short(0), "US stays IDLE after the abort");
}

/// The un-aborted timer still starts the count — the cancel must not be
/// reachable from the arming path itself.
#[tokio::test]
async fn an_untouched_delayed_start_still_starts_the_count() {
    let server = abort_fixture().await;

    put_cnt(&server, 1).await;
    tokio::time::sleep(Duration::from_secs_f64(DLY + 0.3)).await;

    let ss = server.get("SC.SS").await.unwrap();
    assert_eq!(
        ss,
        EpicsValue::Short(2),
        "the delayed start must still fire and reach SCALER_STATE_COUNTING"
    );
}
