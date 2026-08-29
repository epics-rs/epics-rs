//! A cycle that ENDS owes the drain its queued put-notify, whether or not it
//! ends well.
//!
//! C reaches `dbNotifyCompletion` from `recGblFwdLink` (`recGbl.c:295-302`) on
//! every path that ends a cycle, and `dbNotifyCompletion` (`dbNotify.c:445-475`)
//! is what restarts the next `processNotify` queued on the record. A record
//! support that returned a non-zero status has still ended its cycle, so C
//! still replays the queue.
//!
//! In the port the release token was minted at the head of
//! `process_record_with_links_body` and consumed only at that function's
//! explicit tails. Three exits sit ABOVE those tails —
//! `instance.run_registered_subroutine()?`, `instance.record.process()?`, and
//! the async-output `write_begin` `Ok(Some(completion))` early return — and
//! each dropped a `let`-bound token, so a put queued behind such a cycle waited
//! for a restart that was never armed. `#[must_use]` on `PactExit` does not see
//! this: it fires on an unused *expression*, never on a bound value going out
//! of scope. `processing::CycleEndGuard` is what closes it, and it closes all
//! three at once without an edit at any of them — which is the point of putting
//! the debt in a `Drop` rather than at each site.
//!
//! The SNAM put below only RELEASES the park; the cycle driven after it is what
//! arms the first restart, because C reaches `restartCheck` from
//! `recGblFwdLink` and never from the `pact = FALSE` store — see
//! `pact_park_release_defers_the_restart`.
//!
//! Driven through the `sub` empty-SNAM park, the one standing PACT a put can
//! walk into (`subRecord.c:119-122`), because it is the only way to get two
//! puts queued on one record from a test without device support.

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::ProcessCompletion;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;
use std::sync::Arc;

const DB: &str = r#"
record(sub, "PARKED") { }
"#;

/// C's `do_sub` returns a `long`; a Rust subroutine can also fail outright,
/// and that failure propagates out of `run_registered_subroutine` as the `?`
/// this test drives.
fn boom(_rec: &mut dyn epics_base_rs::server::record::Record) -> CaResult<i64> {
    Err(CaError::InvalidValue("boom".into()))
}

/// The control's subroutine: the same shape, ending well.
fn fine(_rec: &mut dyn epics_base_rs::server::record::Record) -> CaResult<i64> {
    Ok(0)
}

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

async fn build() -> Db {
    IocBuilder::new()
        .register_subroutine("boom", boom)
        .register_subroutine("fine", fine)
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn pact(db: &Db, rec: &str) -> u8 {
    match db
        .get_record(rec)
        .unwrap()
        .read()
        .client_field_value("PACT")
    {
        Some(EpicsValue::UChar(v)) => v,
        other => panic!("{rec}.PACT: {other:?}"),
    }
}

/// The restart is queued, not recursed (C `callbackRequest`), so give the
/// executor turns to run it.
///
/// `Closed` is the regression, not a settlement. It means the notify sender was
/// DROPPED without signalling — precisely what a lost `PactExit` does — so folding
/// it into the `true` arm made every assertion in this file pass on the defect they
/// exist to catch. It is also terminal, unlike `Empty`, so it gets its own exit.
async fn settled(rx: epics_base_rs::runtime::sync::oneshot::Receiver<()>) -> bool {
    use epics_base_rs::runtime::sync::oneshot::error::TryRecvError;
    let mut rx = rx;
    for _ in 0..400 {
        match rx.try_recv() {
            Ok(()) => return true,
            Err(TryRecvError::Empty) => {
                epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
            }
            Err(TryRecvError::Closed) => {
                panic!(
                    "the notify sender was dropped without signalling: the restart was never armed"
                )
            }
        }
    }
    false
}

#[epics_macros_rs::epics_test]
async fn a_put_queued_behind_a_failing_cycle_still_gets_its_restart() {
    let db = build().await;
    assert_eq!(pact(&db, "PARKED"), 1, "an empty SNAM parks at init");

    let first = db
        .put_record_field_from_ca("PARKED", "VAL", EpicsValue::Double(7.0))
        .await
        .expect("a put-notify onto a PACT record parks, it does not fail");
    let second = db
        .put_record_field_from_ca("PARKED", "VAL", EpicsValue::Double(9.0))
        .await
        .expect("the second queues behind the first");
    let (ProcessCompletion::Async(first), ProcessCompletion::Async(second)) = (first, second)
    else {
        panic!("both puts must be waiting on a restart");
    };

    // Releasing the park lets the record process again; the FIRST restart is
    // armed by the tail of the next cycle, which is C's owner
    // (`recGblFwdLink` → `dbNotifyCompletion`) and never the `pact = FALSE`
    // store. Its replay then runs `boom`, whose `Err` leaves
    // `process_record_with_links_body` at `run_registered_subroutine()?` —
    // above every explicit release site.
    db.put_record_field_from_ca_no_notify("PARKED", "SNAM", EpicsValue::String("boom".into()))
        .await
        .expect("the no-notify route has no PACT gate");
    assert_eq!(pact(&db, "PARKED"), 0, "the park is released");
    let mut visited = std::collections::HashSet::new();
    let _ = db
        .process_record_with_links("PARKED", &mut visited, 0)
        .await;

    assert!(
        settled(first).await,
        "the released park replays the first queued put"
    );
    assert!(
        settled(second).await,
        "a failing cycle still ends, so C still restarts the next queued put"
    );
}

/// Negative control: the identical queue, released onto a subroutine that ends
/// well. It pins that the case above measures the FAILING exit and not the
/// harness — this one reaches the tail by the ordinary route, so it passes with
/// `CycleEndGuard::drop` defused and the case above does not.
#[epics_macros_rs::epics_test]
async fn a_put_queued_behind_a_succeeding_cycle_gets_its_restart_the_ordinary_way() {
    let db = build().await;

    let first = db
        .put_record_field_from_ca("PARKED", "VAL", EpicsValue::Double(7.0))
        .await
        .expect("parks");
    let second = db
        .put_record_field_from_ca("PARKED", "VAL", EpicsValue::Double(9.0))
        .await
        .expect("queues");
    let (ProcessCompletion::Async(first), ProcessCompletion::Async(second)) = (first, second)
    else {
        panic!("both puts must be waiting on a restart");
    };

    db.put_record_field_from_ca_no_notify("PARKED", "SNAM", EpicsValue::String("fine".into()))
        .await
        .expect("the no-notify route has no PACT gate");
    let mut visited = std::collections::HashSet::new();
    let _ = db
        .process_record_with_links("PARKED", &mut visited, 0)
        .await;

    assert!(settled(first).await, "the first replays");
    assert!(settled(second).await, "and its tail restarts the second");
}

/// The formerly-bypassing path, pinned: a sender dropped without signalling is the
/// lost restart, and [`settled`] must not report it as one. With the old
/// `Err(_) => return true` this returned `true` and every assertion above passed on
/// the defect.
#[epics_macros_rs::epics_test]
#[should_panic(expected = "dropped without signalling")]
async fn a_dropped_notify_sender_is_not_a_settlement() {
    let (tx, rx) = epics_base_rs::runtime::sync::oneshot::channel::<()>();
    drop(tx);
    let _ = settled(rx).await;
}
