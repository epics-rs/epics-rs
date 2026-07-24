//! The `put_*_process` tier must carry the writer's origin into every event
//! its synchronous put+process cascade posts, so an SNL state machine's
//! origin-filtered subscriptions do not hear the machine's own puts.
//!
//! Regression shape: the optics-rs SNL ports write setpoint PVs they also
//! monitor. Under `put_*_post` the events carried the channel origin and the
//! `DbMultiMonitor` filter dropped them; when the ports moved to the
//! `put_*_process` tier (seq pvPut = dbPutField parity) the origin was not
//! plumbed, every self-put came back as a fresh monitor event, and the kohzu
//! state machine re-triggered itself into an unbounded event storm
//! (clamp → rollback → clamp ping-pong, measured on the mini-beamline
//! example 2026-07-24).

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::{DbChannel, DbSubscription};
use epics_base_rs::server::records::ao::AoRecord;

const WRITER: u64 = 4242;

async fn setup() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("SP", Box::new(AoRecord::default()))
        .await
        .unwrap();
    db
}

/// The writer's own filtered subscription must NOT see its `put_f64_process`:
/// the process-cycle VAL post carries the channel's origin.
#[epics_macros_rs::epics_test]
async fn process_put_is_invisible_to_its_own_origin() {
    let db = setup().await;
    let mut own = DbSubscription::subscribe_filtered(&db, "SP.VAL", WRITER)
        .await
        .expect("VAL is a served field");

    let ch = DbChannel::with_origin(&db, "SP.VAL", WRITER);
    ch.put_f64_process(7.5).await.unwrap();

    assert_eq!(ch.get_f64().await, 7.5, "the put itself must still land");
    assert!(
        own.try_recv_event().is_err(),
        "a process-tier self-put must be filtered by the writer's own origin"
    );
}

/// The same put MUST stay visible to everyone else: a subscription with a
/// different (or no) filter origin receives the process-cycle post.
#[epics_macros_rs::epics_test]
async fn process_put_is_visible_to_other_origins() {
    let db = setup().await;
    let mut other = DbSubscription::subscribe_filtered(&db, "SP.VAL", WRITER + 1)
        .await
        .expect("VAL is a served field");
    let mut unfiltered = DbSubscription::subscribe(&db, "SP.VAL")
        .await
        .expect("VAL is a served field");

    let ch = DbChannel::with_origin(&db, "SP.VAL", WRITER);
    ch.put_f64_process(3.25).await.unwrap();

    let ev = other
        .try_recv_event()
        .expect("a different filter origin must receive the post");
    assert_eq!(ev.snapshot.value.to_f64(), Some(3.25));
    let ev = unfiltered
        .try_recv_event()
        .expect("an unfiltered subscriber must receive the post");
    assert_eq!(ev.snapshot.value.to_f64(), Some(3.25));
}

/// An origin-less channel keeps today's behavior: origin 0 is never filtered.
#[epics_macros_rs::epics_test]
async fn originless_process_put_reaches_a_filtered_subscription() {
    let db = setup().await;
    let mut sub = DbSubscription::subscribe_filtered(&db, "SP.VAL", WRITER)
        .await
        .expect("VAL is a served field");

    let ch = DbChannel::new(&db, "SP.VAL");
    ch.put_f64_process(1.5).await.unwrap();

    let ev = sub
        .try_recv_event()
        .expect("an origin-0 put must not be filtered");
    assert_eq!(ev.snapshot.value.to_f64(), Some(1.5));
}
