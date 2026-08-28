//! A put-notify addressed by an alias reaches the record it names.
//!
//! C addresses a notify by `dbCommon *`: `dbProcessNotify` resolves the name
//! through `dbNameToAddr` (`dbNotify.c:326-327` takes `ppn->chan`, already a
//! `dbChannel` on the canonical record) and then compares record POINTERS —
//! `dbNotifyAdd` (`:492-499`) and the park test (`:225-232`) both do. An alias
//! is a second name for one `dbCommon`, so it is indistinguishable there.
//!
//! In the port the canonical name is what the records map is keyed by, so every
//! entry that reaches the map has to resolve first. `acquire_put_gate`
//! (`field_io.rs:922-926`) does, `get_record` does, and
//! `put_record_field_from_ca_body` shadows its parameter with the resolved name
//! (`:1986-1992`); `install_notify_and_process_already_locked` was the one that
//! did not, so the gate locked the record and the lookup one line later missed
//! it.
//!
//! Both paths that reach it are driven: the fresh arrival
//! (`process_record_with_notify`) and the parked replay, which arrives through
//! `restart_next_notify_put` carrying whatever name the CYCLE that armed it
//! was driven by — C's restart owner is `recGblFwdLink` → `dbNotifyCompletion`,
//! not the put that releases the park (see
//! `pact_park_release_defers_the_restart`), so the replay below is driven by an
//! explicit process and the alias is carried through that entry.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::ProcessCompletion;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;
use std::sync::Arc;

const DB: &str = r#"
record(sub, "REAL:NAME") { field(SNAM, "bump") alias("NICK") }
record(sub, "PARKED") { alias("PARKED:NICK") }
"#;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

fn bump(
    rec: &mut dyn epics_base_rs::server::record::Record,
) -> epics_base_rs::error::CaResult<i64> {
    let v = rec.get_field("VAL").and_then(|v| v.to_f64()).unwrap_or(0.0);
    rec.put_field("VAL", EpicsValue::Double(v + 1.0))?;
    Ok(0)
}

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

fn val(db: &Db, rec: &str) -> f64 {
    db.get_record(rec)
        .unwrap()
        .read()
        .record
        .get_field("VAL")
        .and_then(|v| v.to_f64())
        .unwrap()
}

/// Fresh arrival. The alias and the canonical name must behave identically —
/// in C they are the same `dbCommon *`.
///
/// The `.expect` is the line that fails on revert: the unresolved lookup
/// returns `ChannelNotFound` for `NICK`. The VAL assertion carries the other
/// half — that the entry actually ran a cycle rather than returning `Ok` —
/// which is why `REAL:NAME` is a `sub` running `bump` and not a record whose
/// VAL the `.db` already seeded to the expected answer.
#[epics_macros_rs::epics_test]
async fn a_put_notify_addressed_by_an_alias_processes_the_record() {
    let db = build().await;
    assert_eq!(val(&db, "REAL:NAME"), 0.0, "nothing has processed it yet");

    db.process_record_with_notify("NICK")
        .await
        .expect("an alias names a loaded record");

    assert_eq!(
        val(&db, "REAL:NAME"),
        1.0,
        "`bump` ran, so the cycle reached the aliased record"
    );
}

/// The canonical spelling, as the control: if this ever fails the test above
/// is measuring the wrong thing.
#[epics_macros_rs::epics_test]
async fn a_put_notify_addressed_by_the_canonical_name_processes_the_record() {
    let db = build().await;

    db.process_record_with_notify("REAL:NAME")
        .await
        .expect("the canonical name names a loaded record");

    assert_eq!(
        val(&db, "REAL:NAME"),
        1.0,
        "`bump` ran on the canonical name"
    );
}

/// The replay path. A `sub` with an empty SNAM is parked for the life of the
/// IOC (`subRecord.c:119-122`), so a put-notify onto it waits; the SNAM put
/// releases the park and the next cycle's tail replays the parked notify —
/// through the same already-locked entry, with the name that cycle was driven
/// by.
#[epics_macros_rs::epics_test]
async fn a_parked_notify_replays_when_the_release_arrives_by_alias() {
    let db = build().await;

    let parked = db
        .process_record_with_notify("PARKED:NICK")
        .await
        .expect("a notify onto a PACT record parks, it does not fail");
    assert!(
        matches!(parked, ProcessCompletion::Async(_)),
        "C `notifyRestartInProgress`: the client waits for the restart"
    );

    db.put_record_field_from_ca_no_notify("PARKED:NICK", "SNAM", EpicsValue::String("bump".into()))
        .await
        .expect("the no-notify route has no PACT gate");

    // The release runs no cycle of its own (SNAM is not `pp(TRUE)`), so drive
    // one — by ALIAS, which is the entry under test.
    let mut visited = std::collections::HashSet::new();
    let _ = db
        .process_record_with_links("PARKED:NICK", &mut visited, 0)
        .await;

    // `bump` runs twice: once for the cycle just driven, and once more for the
    // notify its tail replays. 1.0 is the first alone — the state this test
    // must not settle for — so poll for the terminal 2.0 rather than for any
    // increment, or a fast replay and a slow one measure different things.
    for _ in 0..400 {
        if val(&db, "PARKED") >= 2.0 {
            break;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        val(&db, "PARKED"),
        2.0,
        "the cycle owes the parked notify a restart, alias or not"
    );
}
