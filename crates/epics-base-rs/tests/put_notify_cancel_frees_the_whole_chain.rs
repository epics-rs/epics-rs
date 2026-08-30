//! C `dbNotifyCancel` (`dbNotify.c:385-430`) frees EVERY record a dead
//! put-notify owns, not just the one it was issued against:
//!
//! ```c
//! case notifyRestartInProgress:
//! case notifyProcessInProgress:
//!     { /*Take all records out of wait list */
//!         while ((ppnrWait = ellFirst(&pnotifyPvt->waitList))) {
//!             ellSafeDelete(&pnotifyPvt->waitList, &ppnrWait->waitNode);
//!             restartCheck(ppnrWait);          /* :430 */
//!         }
//!     }
//!     if (precord->ppn == ppn) restartCheck(precord->ppnr);   /* :434 */
//! ```
//!
//! The chain target is the case that matters, because the record whose cycle
//! never ends is exactly the one that keeps a dead set: a `busy` at VAL=1
//! withholds `recGblFwdLink` by contract (`busyRecord.c:271`), so its
//! membership outlives the client that started the chain and nothing else
//! would ever free it.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(bo, "A") { field(FLNK,"B") }
record(busy, "B") { field(ZNAM,"Done") field(ONAM,"Busy") }
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// A put-notify on A reaches B through the forward link; B joins the wait-set
/// (C `dbNotifyAdd`) and then declines its own tail because it is a `busy` at
/// VAL=1, so it keeps the membership. The client gives up.
///
/// The next put addressed to B must land. It did not while the release could
/// only reach the ENTRY record: B's slot held a set nobody could answer, so
/// the put queued on B's restart list behind a completion that could never
/// arrive and wrote nothing.
#[epics_macros_rs::epics_test]
async fn a_dead_notify_frees_its_chain_target() {
    let db = build().await;
    let b = db.get_record("B").unwrap();

    // Park B at VAL=1 with a plain caput, so it has no notify slot of its own.
    db.put_record_field_from_ca_no_notify("B", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(b.read().record.get_field("VAL"), Some(EpicsValue::Enum(1)));

    let held = db
        .put_record_field_from_ca("A", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert!(
        !held.is_sync(),
        "B withholds recGblFwdLink at VAL=1, so the chain has not settled"
    );
    assert!(
        b.read().has_notify(),
        "B joined A's wait-set through the forward link"
    );
    drop(held); // the client gives up and exits

    db.put_record_field_from_ca("B", "VAL", EpicsValue::Double(0.0))
        .await
        .unwrap();
    assert_eq!(
        b.read().record.get_field("VAL"),
        Some(EpicsValue::Enum(0)),
        "the write to the chain target must land"
    );
}

/// The case only a teardown call closes: a SECOND client is already queued on
/// the record's restart list when the first dies. Nothing then arrives to test
/// ownership, so a release that runs only at the next arrival never runs at
/// all and the queued client waits forever.
///
/// The cancel here is what `PendingWriteNotify`'s `Drop` performs when a CA
/// connection with a busy put-callback goes away (C `rsrvFreePutNotify`,
/// `camessage.c:1630-1638`), and it is C's `restartCheck` that then hands the
/// record to the queued client.
#[epics_macros_rs::epics_test]
async fn a_cancel_hands_the_record_to_the_queued_client() {
    let db = build().await;
    let b = db.get_record("B").unwrap();

    // Client 1 leaves B at VAL=1, so its callback is withheld and it owns B.
    let first = db
        .put_record_field_from_ca("B", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert!(!first.is_sync(), "VAL=1 withholds the callback");
    assert!(b.read().has_notify());

    // Client 2 arrives while client 1 still owns the record: C
    // `processNotifyCommon` tests ownership above `putCallback`
    // (`dbNotify.c:213-219`), so this writes nothing and queues.
    let second = db
        .put_record_field_from_ca("B", "VAL", EpicsValue::Double(0.0))
        .await
        .unwrap();
    assert!(!second.is_sync(), "queued behind client 1");
    assert_eq!(
        b.read().record.get_field("VAL"),
        Some(EpicsValue::Enum(1)),
        "a queued put-notify writes nothing until it is replayed"
    );

    drop(first); // the CA connection goes away with its put-callback busy
    db.cancel_unanswerable_notify("B");

    second
        .into_handle()
        .expect("client 2 is still waiting")
        .await
        .expect("the cancel must replay the queued put, not drop its sender");
    assert_eq!(
        b.read().record.get_field("VAL"),
        Some(EpicsValue::Enum(0)),
        "restartCheck hands the record to the queued client"
    );
}
