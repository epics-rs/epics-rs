//! Releasing a PACT park does not replay the put-notify parked on it. The
//! record's next CYCLE does.
//!
//! C reaches `restartCheck` from `dbNotifyCompletion` ← `recGblFwdLink`
//! (`recGbl.c:295`) and from the cancel paths. Of its six call sites in
//! `dbNotify.c`, `:461` and `:465` are `dbNotifyCompletion` — the cycle tail —
//! and `:430`, `:434` (`dbNotifyCancel`, `:385`) and `:290`
//! (`notifyCallback`'s `cancelWait` branch) are cancels. The sixth, `:266`, is
//! neither: it is in `processNotifyCommon` on the
//! `notifyRestartCallbackRequested` arm, reached only from the callback
//! (`:298`) or from `dbProcessNotify` (`:382`). What matters here is what all
//! six share — the `pact = FALSE` store is not among them, so a put body has no
//! C counterpart for arming a restart at all.
//!
//! A `sub` with an empty SNAM is parked for the life of the IOC
//! (`subRecord.c:119-122`), so it is the one record where the park is a standing
//! state a put can walk into, and `subRecord.c::special` pass 0 (`:174-179`) is
//! what releases it. That release runs no cycle: SNAM carries no `pp`
//! (`subRecord.dbd.pod:430-436`), so `dbPutField`'s only process gate —
//! `paddr->pfield == &precord->proc || (pfldDes->process_passive && scan == 0)`,
//! `dbAccess.c:1263-1267` — does not fire, and neither does `dbDbPutValue`'s
//! (`dbDbLink.c:387-390`) unless the link carries ` PP` or names `.PROC`.
//!
//! So the parked notify survives the release with `precord->ppn` still set and
//! state `notifyRestartInProgress`, and waits for whatever processes the record
//! next. The consequence is the same in C and here: the channel's put-callback
//! stays outstanding, and the next WRITE_NOTIFY on it is refused with
//! ECA_PUTCBINPROG (RSRV's `pciu->pPutNotify` busy check).
//!
//! The port used to arm the restart from the put body, and
//! [`the_snam_put_does_not_replay_the_parked_notify`] is the case that measured
//! it: VAL settled at 8.0 where C leaves 0.0. The queue lives on the record
//! (`RecordInstance::notify_restart_list`) and is drained by one owner,
//! `PvDatabase::end_process_cycle` → `apply_pact_exit`, so nothing is stranded
//! by dropping the token — only deferred, which
//! [`the_next_process_after_the_snam_put_replays_it`] pins.
//!
//! One case per gate, so a change to either one cannot pass unnoticed: the bare
//! `dbPutField` release, the `dbDbPutValue` release with no ` PP` (no cycle
//! either way), the same release WITH ` PP` (a cycle follows, and its tail is
//! what replays), and a put to a field that releases nothing at all.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::ProcessCompletion;
use epics_base_rs::types::EpicsValue;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const DB: &str = r#"
record(sub, "PARKED") { }
record(sub, "NPPTGT") { }
record(sub, "PPTGT") { }
record(stringout, "NPPNAMER") { field(OUT, "NPPTGT.SNAM") field(VAL, "bump") }
record(stringout, "PPNAMER") { field(OUT, "PPTGT.SNAM PP") field(VAL, "bump") }
"#;

fn bump(
    rec: &mut dyn epics_base_rs::server::record::Record,
) -> epics_base_rs::error::CaResult<i64> {
    let v = rec.get_field("VAL").and_then(|v| v.to_f64()).unwrap_or(0.0);
    rec.put_field("VAL", EpicsValue::Double(v + 1.0))?;
    Ok(0)
}

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

async fn build() -> Db {
    IocBuilder::new()
        .register_subroutine("bump", bump)
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

fn val(db: &Db, rec: &str) -> f64 {
    db.get_record(rec)
        .unwrap()
        .read()
        .record
        .get_field("VAL")
        .and_then(|v| v.to_f64())
        .unwrap()
}

/// The restart is queued, not recursed (C `callbackRequest`), so it can only
/// land on a later turn of the executor. Polls until `want` or the window ends,
/// and returns whatever VAL holds then — so a case asserting "no replay" waits
/// the same full window as one asserting a replay.
async fn settle_until(db: &Db, rec: &str, want: f64) -> f64 {
    for _ in 0..400 {
        if val(db, rec) == want {
            return want;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
    }
    val(db, rec)
}

/// Drive one process cycle, the way anything else in the IOC would.
async fn process(db: &Db, rec: &str) {
    let mut visited = HashSet::new();
    let _ = db.process_record_with_links(rec, &mut visited, 0).await;
}

/// The bare `dbPutField` release. It stores SNAM, clears PACT and RPRO
/// (`subRecord.c:175-178`) and returns — `dbAccess.c:1263-1267` runs no cycle
/// for a field that is not `pp(TRUE)`, so `dbNotifyCompletion` is never reached
/// and the parked put is still parked.
#[epics_macros_rs::epics_test]
async fn the_snam_put_does_not_replay_the_parked_notify() {
    let db = build().await;
    assert_eq!(pact(&db, "PARKED"), 1, "an empty SNAM parks at init");

    let parked = db
        .put_record_field_from_ca("PARKED", "VAL", EpicsValue::Double(7.0))
        .await
        .expect("a put-notify onto a PACT record parks, it does not fail");
    assert!(
        matches!(parked, ProcessCompletion::Async(_)),
        "C `notifyRestartInProgress`: the client waits for the restart"
    );
    assert_eq!(
        val(&db, "PARKED"),
        0.0,
        "dbNotify.c:225-232 tests PACT ABOVE the put — nothing is written"
    );

    db.put_record_field_from_ca_no_notify("PARKED", "SNAM", EpicsValue::String("bump".into()))
        .await
        .expect("the no-notify route has no PACT gate");
    assert_eq!(pact(&db, "PARKED"), 0, "the park is released");

    assert_eq!(
        settle_until(&db, "PARKED", 8.0).await,
        0.0,
        "the release runs no cycle, so it reaches no restartCheck: 8.0 here is \
         the put body arming a restart C arms only from recGblFwdLink"
    );
}

/// …and the queue it left behind is intact. The record's next cycle ends at
/// `end_process_cycle`, whose `PactExit` re-derives `restart_pending` from
/// `RecordInstance::notify_restart_list`, so the deferral costs the client one
/// cycle and never the callback.
#[epics_macros_rs::epics_test]
async fn the_next_process_after_the_snam_put_replays_it() {
    let db = build().await;

    let parked = db
        .put_record_field_from_ca("PARKED", "VAL", EpicsValue::Double(7.0))
        .await
        .expect("parks");
    assert!(matches!(parked, ProcessCompletion::Async(_)));

    db.put_record_field_from_ca_no_notify("PARKED", "SNAM", EpicsValue::String("bump".into()))
        .await
        .unwrap();
    process(&db, "PARKED").await;

    // 7.0 from the replayed put, then +1.0 from `bump` on the process the
    // replay drives (sub VAL is `pp(TRUE)`).
    assert_eq!(
        settle_until(&db, "PARKED", 8.0).await,
        8.0,
        "the cycle tail is C's restart owner, and it still owns the drain"
    );
}

/// The `dbDbPutValue` release with no ` PP` and a target field that is not
/// `.PROC`: `dbDbLink.c:387-390` runs no `processTarget`, so this route ends
/// exactly where the bare `dbPutField` one does.
#[epics_macros_rs::epics_test]
async fn an_npp_out_link_release_does_not_replay_either() {
    let db = build().await;
    assert_eq!(pact(&db, "NPPTGT"), 1);

    let parked = db
        .put_record_field_from_ca("NPPTGT", "VAL", EpicsValue::Double(3.0))
        .await
        .expect("parks");
    assert!(matches!(parked, ProcessCompletion::Async(_)));

    process(&db, "NPPNAMER").await;
    assert_eq!(pact(&db, "NPPTGT"), 0, "the OUT link named SNAM");

    assert_eq!(
        settle_until(&db, "NPPTGT", 4.0).await,
        0.0,
        "an NPP dbPutLink drives no cycle on its target, so it replays nothing"
    );
}

/// The same release WITH ` PP`. `dbDbPutValue` calls `processTarget`
/// (`dbDbLink.c:387-390`), that cycle reaches `recGblFwdLink`, and the replay
/// comes from its tail — the one shape in this file where a put IS followed by
/// a restart, and it is the cycle that owns it.
#[epics_macros_rs::epics_test]
async fn a_pp_out_link_release_replays_from_the_cycle_tail() {
    let db = build().await;
    assert_eq!(pact(&db, "PPTGT"), 1);

    let parked = db
        .put_record_field_from_ca("PPTGT", "VAL", EpicsValue::Double(3.0))
        .await
        .expect("parks");
    assert!(matches!(parked, ProcessCompletion::Async(_)));

    process(&db, "PPNAMER").await;
    assert_eq!(pact(&db, "PPTGT"), 0, "the OUT link named SNAM");

    assert_eq!(settle_until(&db, "PPTGT", 4.0).await, 4.0);
}

/// Negative control: a put to a field that is not `special(SPC_MOD)` for the
/// park releases nothing, so the record is still PACT and the put still parked.
#[epics_macros_rs::epics_test]
async fn a_put_to_an_unrelated_field_releases_nothing() {
    let db = build().await;

    let parked = db
        .put_record_field_from_ca("PARKED", "VAL", EpicsValue::Double(7.0))
        .await
        .expect("parks");
    assert!(matches!(parked, ProcessCompletion::Async(_)));

    db.put_record_field_from_ca_no_notify("PARKED", "DESC", EpicsValue::String("hi".into()))
        .await
        .unwrap();

    assert_eq!(pact(&db, "PARKED"), 1, "DESC is not a park field");
    assert_eq!(
        settle_until(&db, "PARKED", 8.0).await,
        0.0,
        "no release, no cycle, no restart"
    );
}
