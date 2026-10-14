//! An ASG `INP*` value that lands before the watcher's subscriber is attached
//! must still reach a re-evaluation. C adds every input channel with
//! `caInitializing` set and then runs `asComputeAllAsg` (asCa.c:204-205), so
//! the attach is itself an evaluation point; the port takes the record's
//! current reading under the same lock that attaches the subscriber, so a
//! post either precedes the attach and is in that reading, or follows it and
//! reaches the subscriber.

use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::task::{sleep, timeout, yield_now};
use epics_base_rs::server::access_security::{
    new_acf_cell, parse_acf, start_acf_watchers, subscribe_asg_changes,
};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;

const ACF: &str = r#"ASG(DEFAULT) { RULE(1, READ) }
    ASG(G) { INPA("GATE") RULE(1, WRITE) { CALC("A=1") } }"#;

const WAIT: Duration = Duration::from_secs(5);

async fn gate_db() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.add_record("GATE", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db
}

#[epics_macros_rs::epics_test]
async fn a_post_before_the_attach_is_reevaluated() {
    let db = gate_db().await;
    let cell = new_acf_cell(Some(parse_acf(ACF).unwrap()));
    // A client that has already evaluated against GATE = 0.
    let mut client = subscribe_asg_changes();
    db.put_pv_and_post("GATE", EpicsValue::Double(1.0))
        .await
        .unwrap();
    start_acf_watchers(&db, &cell);
    assert!(
        timeout(WAIT, client.recv()).await.is_ok(),
        "GATE = 1 was posted before the attach and never re-evaluated"
    );
}

#[epics_macros_rs::epics_test]
async fn a_post_after_the_attach_is_reevaluated() {
    let db = gate_db().await;
    let cell = new_acf_cell(Some(parse_acf(ACF).unwrap()));
    start_acf_watchers(&db, &cell);
    let rec = db.get_record("GATE").unwrap();
    timeout(WAIT, async {
        while rec.read().subscriber_count("VAL") == 0 {
            yield_now().await;
        }
    })
    .await
    .expect("the watcher attaches GATE");
    // Let any notification the attach raises go by before the client listens.
    sleep(Duration::from_millis(200)).await;
    let mut client = subscribe_asg_changes();
    db.put_pv_and_post("GATE", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert!(
        timeout(WAIT, client.recv()).await.is_ok(),
        "GATE = 1 was posted after the attach and never re-evaluated"
    );
}
