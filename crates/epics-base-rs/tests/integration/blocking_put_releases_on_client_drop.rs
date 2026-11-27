//! A blocking external PUT (`record[block=true]`) whose awaiting future is
//! dropped must hand the record back, the way C's CA server does from
//! `rsrvFreePutNotify` (`rsrv/camessage.c:1630-1638`):
//!
//! ```c
//! void rsrvFreePutNotify(struct client *pClient, ...)
//! {
//!     if (pNotify->busy) { dbNotifyCancel(&pNotify->dbPutNotify); }
//! }
//! ```
//!
//! pvxs has no such call because on that transport the blocking put IS the
//! operation's future: the native PVA server spawns the PUT EXEC body and
//! keeps its abort handle on the op, so DESTROY_CHANNEL and connection
//! teardown alike drop `ChannelState::ops`, abort the task and drop the
//! future mid-await. The release therefore belongs to the future itself.
//!
//! The case that only a teardown release closes is a SECOND client already
//! parked on the record's restart list. Nothing then arrives to test
//! ownership, so a release that runs only at the next put's arrival never
//! runs at all — the queued client waits on a completion that can never come.
//! `busy` at VAL=1 is the record that makes it permanent: it withholds
//! `recGblFwdLink` by contract (`busyRecord.c:271`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use epics_base_rs::server::database::{ProcessMode, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(busy, "B") { field(ZNAM,"Done") field(ONAM,"Busy") }
"#;

async fn build() -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

struct Woken(AtomicBool);

impl Wake for Woken {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Drive `f` in place until it parks — poll it, and keep polling for as long
/// as it asks to be woken again. A blocking put on a `busy` left at VAL=1
/// parks on a completion that never comes, which is exactly the state an
/// aborted PVA PUT EXEC task is in when its future is dropped.
fn drive_until_parked<F: Future + Unpin>(f: &mut F) -> Poll<F::Output> {
    let woken = Arc::new(Woken(AtomicBool::new(false)));
    let waker = Waker::from(woken.clone());
    let mut cx = Context::from_waker(&waker);
    for _ in 0..64 {
        woken.0.store(false, Ordering::SeqCst);
        match Pin::new(&mut *f).poll(&mut cx) {
            Poll::Ready(v) => return Poll::Ready(v),
            Poll::Pending if !woken.0.load(Ordering::SeqCst) => return Poll::Pending,
            Poll::Pending => {}
        }
    }
    panic!("the put never parked");
}

fn val(db: &PvDatabase) -> Option<EpicsValue> {
    db.get_record("B").unwrap().read().record.get_field("VAL")
}

/// Client A blocks on a put that `busy` will never answer; client B blocks
/// behind it on the restart list; A's transport dies. B must be handed the
/// record. Without the release in A's future this deadlocks B forever: the
/// arrival-time sweep needs an arrival, and B arrived before A died.
#[epics_macros_rs::epics_test]
async fn a_dropped_blocking_put_hands_the_record_to_the_queued_client() {
    let db = build().await;

    let mut first = Box::pin(db.put_field_from_client(
        "B",
        "VAL",
        EpicsValue::Double(1.0),
        ProcessMode::Passive,
        true,
    ));
    assert!(
        drive_until_parked(&mut first).is_pending(),
        "VAL=1 withholds the callback, so the blocking put cannot return"
    );
    assert!(
        db.get_record("B").unwrap().read().has_notify(),
        "client A owns B's put-notify slot"
    );

    let mut second = Box::pin(db.put_field_from_client(
        "B",
        "VAL",
        EpicsValue::Double(0.0),
        ProcessMode::Passive,
        true,
    ));
    assert!(
        drive_until_parked(&mut second).is_pending(),
        "client B queues behind A"
    );
    assert_eq!(
        val(&db),
        Some(EpicsValue::Enum(1)),
        "a queued put-notify writes nothing until it is replayed"
    );

    // The PVA op is aborted and its body dropped mid-await.
    drop(first);

    second.await.expect("client B's put must complete");
    assert_eq!(
        val(&db),
        Some(EpicsValue::Enum(0)),
        "restartCheck hands the record to the queued client, which then writes"
    );
}

/// The disarm boundary: a blocking put that DID complete must not sweep on
/// the way out. `process=false` writes without a notify at all, and a
/// settled Passive put returns `ProcessCompletion::Sync`, so the release
/// must key on the await having been reached and abandoned — not on the
/// future being dropped.
#[epics_macros_rs::epics_test]
async fn a_completed_blocking_put_leaves_the_slot_alone() {
    let db = build().await;

    // VAL=0 settles inside the put, so this returns rather than parking.
    db.put_field_from_client(
        "B",
        "VAL",
        EpicsValue::Double(0.0),
        ProcessMode::Passive,
        true,
    )
    .await
    .expect("a settling put completes");
    assert!(
        !db.get_record("B").unwrap().read().has_notify(),
        "a completed put leaves no slot behind"
    );

    // A second client can still park and be answered normally afterwards.
    let mut held = Box::pin(db.put_field_from_client(
        "B",
        "VAL",
        EpicsValue::Double(1.0),
        ProcessMode::Passive,
        true,
    ));
    assert!(drive_until_parked(&mut held).is_pending());
    assert!(db.get_record("B").unwrap().read().has_notify());
}
