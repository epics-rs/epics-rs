//! The link groups of two `seq` records interleave on their callback band.
//!
//! C `processNextLink` (`seqRecord.c:199-216`) queues every group, zero-delay
//! ones included, as its own `callbackRequest(&pcb->callback)` — "Always use
//! the callback task to avoid recursion" — and `processCallback` (`:244-275`)
//! queues the next group from inside the one it just ran. Each group therefore
//! goes to the tail of the band, behind whatever another `seq` queued
//! meanwhile. C softIoc on this db reads TA0..TB2 = 1, 2, 3, 4, 5, 6.

use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

/// `GO`'s one group processes `FAN` on the band worker, so `SA` and `SB` are
/// both queued before either runs a group. Each target counts itself through
/// `CNT`, recording the order the groups ran in.
const DB: &str = r#"
record(calc, "CNT") { field(CALC, "VAL+1") }
record(calc, "TA0") { field(INPA, "CNT PP") field(CALC, "A") }
record(calc, "TA1") { field(INPA, "CNT PP") field(CALC, "A") }
record(calc, "TA2") { field(INPA, "CNT PP") field(CALC, "A") }
record(calc, "TB0") { field(INPA, "CNT PP") field(CALC, "A") }
record(calc, "TB1") { field(INPA, "CNT PP") field(CALC, "A") }
record(calc, "TB2") { field(INPA, "CNT PP") field(CALC, "A") }
record(seq, "SA") {
    field(SELM, "All")
    field(LNK0, "TA0.PROC") field(LNK1, "TA1.PROC") field(LNK2, "TA2.PROC")
}
record(seq, "SB") {
    field(SELM, "All")
    field(LNK0, "TB0.PROC") field(LNK1, "TB1.PROC") field(LNK2, "TB2.PROC")
}
record(fanout, "FAN") { field(LNK0, "SA") field(LNK1, "SB") }
record(seq, "GO") { field(SELM, "All") field(LNK0, "FAN.PROC") }
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

async fn wait_idle(db: &PvDatabase, recs: &[&str]) {
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        if recs
            .iter()
            .all(|rec| !db.get_record(rec).unwrap().read().is_processing())
        {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("{recs:?} never finished their group walks");
}

#[epics_macros_rs::epics_test]
async fn zero_delay_groups_of_two_seqs_alternate() {
    let db = build().await;
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("GO", &mut visited)
        .await
        .unwrap();
    wait_idle(&db, &["GO", "SA", "SB"]).await;

    let order: Vec<f64> = ["TA0", "TB0", "TA1", "TB1", "TA2", "TB2"]
        .iter()
        .map(|rec| {
            db.get_record(rec)
                .unwrap()
                .read()
                .record
                .val()
                .and_then(|v| v.to_f64())
                .unwrap()
        })
        .collect();
    assert_eq!(
        order,
        [1.0, 2.0, 3.0, 4.0, 5.0, 6.0],
        "each group is its own band request (seqRecord.c:210-215), so SA's \
         and SB's groups alternate as on C softIoc"
    );
}

/// C `processNextLink` requests a first group with DLY>0 through
/// `callbackRequestDelayed` (`seqRecord.c:210-213`), whose timer starts at
/// the record's pass: on a band held past DLY the group runs as soon as the
/// band frees, before a request queued after the delay lapsed.
#[epics_macros_rs::epics_test]
async fn first_group_delay_runs_from_the_pass() {
    use epics_base_rs::runtime::task::{CallbackPriority, spawn_background};
    const DLY: Duration = Duration::from_millis(200);
    let db = IocBuilder::new()
        .db_string(
            r#"
record(calc, "T") { field(CALC, "VAL+1") }
record(seq, "S") { field(SELM, "All") field(DLY0, "0.2") field(LNK0, "T.PROC") }
"#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let (started_tx, started) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel::<()>();
    drop(spawn_background(CallbackPriority::Low, async move {
        started_tx.send(()).unwrap();
        let _ = held.recv();
    }));
    started.recv_timeout(Duration::from_secs(3)).unwrap();

    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("S", &mut visited)
        .await
        .unwrap();
    epics_base_rs::runtime::task::sleep(DLY * 2).await;
    let t = db.get_record("T").unwrap();
    let seen = spawn_background(CallbackPriority::Low, async move {
        t.read().record.val().and_then(|v| v.to_f64()).unwrap()
    });
    release.send(()).unwrap();
    assert_eq!(
        seen.await.unwrap(),
        1.0,
        "the first group waited DLY0 from the band's pick-up, not from S's pass"
    );
    wait_idle(&db, &["S"]).await;
}
