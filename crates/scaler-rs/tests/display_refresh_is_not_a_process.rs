//! SC-2 — the periodic display refresh must not run as a whole `process()`.
//!
//! C `updateCallbackFunc` (`scalerRecord.c:203-214`) calls `updateCounts` and
//! nothing else, so a refresh can never reach `recGblFwdLink` at `:480`. And
//! `updateCounts` refuses the call outright when it did not come from
//! `process()` and the count is already over (`:562-568`):
//!
//! ```c
//! called_by_process = (pscal->pact == TRUE);
//! if (!called_by_process) {
//!     if (pscal->ss != SCALER_STATE_IDLE) pscal->pact = TRUE;
//!     else return;
//! }
//! ```
//!
//! The port arms the refresh as a `ProcessAction::ReprocessAfter`, whose
//! re-entry IS a process cycle — so a refresh armed while counting lands after
//! the count has stopped and fires FLNK a second time.
//!
//! Boundaries: refresh re-entry with `ss == IDLE` vs `ss != IDLE`, and a
//! re-entry of the OTHER kind (the delayed start, `delayCallbackFunc`) landing
//! with `ss == IDLE` — which must still start the count, so the gate cannot
//! key on `ss` alone.

#![cfg_attr(exec_backend, allow(unused_imports))]
// This file's other cases are pure record-layer `#[test]`s; only the one
// gated below builds a real CA server, which the reactor-free
// `exec_backend` — selected on a host build by
// `EPICS_RS_BUILD_EXEC_BACKEND=thread`, and unconditionally on RTEMS and
// VxWorks — does not have. Gated per item rather than per file so the
// record-layer cases keep compiling in that configuration.

use epics_base_rs::server::record::{ProcessAction, Record};
use epics_base_rs::types::EpicsValue;
#[cfg(tokio_backend)]
use epics_ca_rs::server::CaServerBuilder;
use scaler_rs::ScalerRecord;
use std::collections::HashMap;
use std::time::Duration;

const SCALER_STATE_IDLE: i16 = 0;
const SCALER_STATE_COUNTING: i16 = 2;

fn has_reprocess(actions: &[ProcessAction]) -> bool {
    actions
        .iter()
        .any(|a| matches!(a, ProcessAction::ReprocessAfter(_)))
}

/// One CA put to CNT: `special()` and the actions it queued, then the
/// `pp(TRUE)` process cycle.
fn put_cnt(rec: &mut ScalerRecord, cnt: i16) -> Vec<ProcessAction> {
    rec.cnt = cnt;
    rec.special("CNT", true).unwrap();
    let mut actions = rec.take_special_actions();
    actions.extend(rec.process().unwrap().actions);
    actions
}

/// The record's own scheduled re-entry, as the framework runs it.
fn fire_timer(rec: &mut ScalerRecord) -> Vec<ProcessAction> {
    rec.set_process_continuation(true);
    rec.process().unwrap().actions
}

/// A user count in progress with the display refresh armed (RATE 10 Hz).
fn counting() -> ScalerRecord {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 100.0;
    rec.rate = 10.0;
    rec.nch = 4;
    rec.init_record(1).unwrap();
    let actions = put_cnt(&mut rec, 1);
    assert_eq!(rec.ss, SCALER_STATE_COUNTING, "the count must have started");
    assert!(
        has_reprocess(&actions),
        "C :590-596 arms the periodic display update while ss == COUNTING"
    );
    rec
}

/// The refresh lands after the operator stopped the count. C returns at
/// `:567` having done nothing.
#[test]
fn a_refresh_landing_after_the_count_stopped_does_nothing() {
    let mut rec = counting();

    put_cnt(&mut rec, 0);
    assert_eq!(rec.ss, SCALER_STATE_IDLE);
    assert!(
        rec.should_fire_forward_link(),
        "the stop's own process cycle fires FLNK once (C :476-481)"
    );

    let actions = fire_timer(&mut rec);
    assert!(
        !rec.should_fire_forward_link(),
        "a display refresh cannot reach recGblFwdLink — C's updateCallbackFunc \
         never calls process()"
    );
    assert!(
        actions.is_empty(),
        "C :567 returns before reading, posting or re-arming: {actions:?}"
    );
}

/// The refresh lands while the count is still live: it runs and re-arms, and
/// still fires no forward link.
#[test]
fn a_refresh_landing_mid_count_refreshes_and_rearms() {
    let mut rec = counting();

    let actions = fire_timer(&mut rec);
    assert_eq!(rec.ss, SCALER_STATE_COUNTING, "the count is untouched");
    assert!(
        has_reprocess(&actions),
        "ss == COUNTING still re-arms the next update (C :590-596)"
    );
    assert!(
        !rec.should_fire_forward_link(),
        "no forward link while ss != IDLE (C :470)"
    );
}

/// The delayed start is the OTHER callback: it lands with `ss == IDLE` too and
/// must still start the count.
#[test]
fn a_delayed_start_landing_with_ss_idle_still_starts_the_count() {
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 100.0;
    rec.dly = 0.01;
    rec.nch = 4;
    rec.init_record(1).unwrap();

    let actions = put_cnt(&mut rec, 1);
    assert!(has_reprocess(&actions), "DLY > 0 arms the delayed start");
    assert_eq!(rec.ss, SCALER_STATE_IDLE, "nothing counts during the wait");

    std::thread::sleep(Duration::from_millis(30));
    fire_timer(&mut rec);
    assert_eq!(
        rec.ss, SCALER_STATE_COUNTING,
        "delayCallbackFunc's re-entry must start the count even though ss was IDLE"
    );
}

/// End to end, the finding's own trigger: RATE=10 arms a refresh at t=0, the
/// operator stops at t=0.05, and the refresh must not fire FLNK at t=0.1.
#[cfg(tokio_backend)]
#[tokio::test]
async fn a_refresh_after_a_stop_does_not_refire_flnk() {
    let db_str = r#"
record(scaler, "SC") {
    field(FREQ, "1000000")
    field(TP, "100.0")
    field(RATE, "10")
    field(DLY, "0")
    field(CONT, "0")
    field(FLNK, "SC:CNTR")
}
record(calc, "SC:CNTR") {
    field(CALC, "VAL+1")
}
"#;
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("scaler", || Box::new(ScalerRecord::default()))
        .db_string(db_str, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let counter = || async {
        match server.get("SC:CNTR.VAL").await.unwrap() {
            EpicsValue::Double(v) => v,
            other => panic!("VAL is DBF_DOUBLE, got {other:?}"),
        }
    };
    let put = |cnt: i16| {
        let db = server.database().clone();
        async move {
            db.put_record_field_from_ca("SC", "CNT", EpicsValue::Short(cnt))
                .await
                .unwrap()
        }
    };

    put(1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(counter().await, 0.0, "no FLNK while counting");

    put(0).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(counter().await, 1.0, "the stop fires FLNK once");

    // Past the refresh armed at t=0, and past two more periods.
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(
        counter().await,
        1.0,
        "the periodic display refresh must not process the record again"
    );
}
