//! C `callbackRequestDelayed` (`callback.c:410-419`) starts an `epicsTimer` at
//! the request, and the timer's `callbackRequest` is the callback's one band
//! request: it joins the tail of the band however small the delay, and runs
//! as soon as the band frees once the delay has lapsed.

use std::time::Duration;

use epics_base_rs::runtime::task::{CallbackPriority, spawn_background};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;

async fn build(db: &str) -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(db, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn val(db: &PvDatabase, rec: &str) -> Option<f64> {
    db.get_record(rec)
        .unwrap()
        .read()
        .record
        .val()
        .and_then(|v| v.to_f64())
}

/// C `wdogCallback` re-arms with `callbackRequestDelayed(.., prec->sdel)`
/// (`histogramRecord.c:119-121`), so a `seq` group requested meanwhile runs
/// between two ticks. C softIoc on this db reads T = 1.
#[epics_macros_rs::epics_test]
async fn a_lapsed_watchdog_rearm_leaves_the_band_to_a_seq_group() {
    let db = build(
        r#"
record(histogram, "H") { field(SDEL, "1e-9") }
record(calc, "T") { field(CALC, "VAL+1") }
record(seq, "S") { field(SELM, "All") field(LNK0, "T.PROC") }
"#,
    )
    .await;
    db.process_record_with_links("S", &mut ProcStack::new())
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while val(&db, "T") != Some(1.0) {
        assert!(
            std::time::Instant::now() < deadline,
            "S's group never ran: the 1 ns watchdog re-arm held the Low band"
        );
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

/// The SDLY defer of an output's `writeValue` is
/// `callbackRequestProcessCallbackDelayed` (`aoRecord.c:585`), whose timer
/// request is the completing pass itself: on a band held past SDLY the pass
/// runs as soon as the band frees, ahead of a request queued after the delay
/// lapsed.
#[epics_macros_rs::epics_test]
async fn an_sdly_pass_runs_on_its_timer_request() {
    const SDLY: Duration = Duration::from_millis(200);
    let db = build(
        r#"
record(ao, "S") {}
record(ao, "A") { field(VAL, "5") field(SIMM, "YES") field(SIOL, "S") field(SDLY, "0.2") }
"#,
    )
    .await;
    let (started_tx, started) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel::<()>();
    drop(spawn_background(CallbackPriority::Low, async move {
        started_tx.send(()).unwrap();
        let _ = held.recv();
    }));
    started.recv_timeout(Duration::from_secs(3)).unwrap();

    db.process_record_with_links("A", &mut ProcStack::new())
        .await
        .unwrap();
    epics_base_rs::runtime::task::sleep(SDLY * 2).await;
    let probe = db.clone();
    let seen = spawn_background(CallbackPriority::Low, async move { val(&probe, "S") });
    release.send(()).unwrap();
    assert_eq!(
        seen.await.unwrap(),
        Some(5.0),
        "the SDLY pass waited from the band's pick-up, or queued a second request"
    );
}
