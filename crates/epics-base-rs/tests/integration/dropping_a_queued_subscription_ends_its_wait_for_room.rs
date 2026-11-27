//! A [`QueuedSubscription`] whose handler waits for room in its full queue
//! drops: the drop returns, and the waiting value goes unqueued. The thread
//! that drops it may be the queue's only reader; C's
//! `ca_clear_subscription` there waits for good.

use std::collections::HashMap;

use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::server::database::QueuedSubscription;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::DBR_DOUBLE;

#[epics_macros_rs::epics_test]
async fn dropping_a_queued_subscription_ends_its_wait_for_room() {
    let budget = fact_budget::FACT_BUDGET;
    let (db, _) = IocBuilder::new()
        .db_string("record(ao, \"X\") {}\n", &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let ch = db.ca_client().create_channel("X").await.expect("X");
    let (tx, mut queue) = mpsc::channel(1);
    tx.try_send(-1.0).unwrap();
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (sent_tx, sent) = std::sync::mpsc::channel();
    let mask = EventMask::VALUE.bits();
    let sub = QueuedSubscription::new(&*ch, DBR_DOUBLE, 1, mask, tx, move |s, queue| {
        let _ = entered_tx.send(());
        let _ = sent_tx.send(queue.send(s.value.to_f64().unwrap()));
    })
    .await
    .unwrap();
    entered.recv_timeout(budget).expect("X's first value");

    let (dropped_tx, dropped) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(sub);
        let _ = dropped_tx.send(());
    });
    let drop_returned = dropped.recv_timeout(budget);
    if drop_returned.is_err() {
        // Room for the waiting value, so the dropping thread gets past it.
        let _ = queue.recv().await;
    }
    assert_eq!(drop_returned, Ok(()), "the subscription's drop returned");
    assert_eq!(sent.recv_timeout(budget), Ok(false), "the wait's send");
    assert_eq!(queue.try_recv(), Ok(-1.0));
    assert!(queue.try_recv().is_err(), "the waiting value was queued");
}
