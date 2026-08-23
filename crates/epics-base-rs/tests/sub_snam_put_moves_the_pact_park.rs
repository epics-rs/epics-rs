//! C `subRecord.c::special` (`:180-200`) is a two-pass state machine over the
//! PACT park, and its whole point is that the park is REVERSIBLE:
//!
//! ```c
//! if (!after) {
//!     if (prec->snam[0] == 0 && prec->pact) {
//!         prec->pact = FALSE;
//!         prec->rpro = FALSE;
//!     }
//!     return 0;
//! }
//! if (prec->snam[0] == 0) {
//!     epicsPrintf("%s.SNAM is empty\n", prec->name);
//!     prec->pact = TRUE;
//!     return 0;
//! }
//! prec->sadr = (SUBFUNCPTR)registryFunctionFind(prec->snam);
//! ```
//!
//! `init_record` (`:119-123`) opens the same park and binds `sadr` the same way.
//! Treating the init verdict as final left `record(sub,"X"){}` unprocessable for
//! the life of the IOC: every PROC took the PACT-active branch and after ten
//! attempts the record went SCAN/INVALID, with no way back short of a restart.
//!
//! Two invariants, asserted here at each of their boundaries. A `sub`'s PACT is
//! parked exactly while its SNAM is empty; SNAM is the only `special(SPC_MOD)`
//! field that bears on it, so a put to any other field must leave the park
//! alone. And the bound subroutine is always the registry's answer for the
//! CURRENT SNAM — aSub's `special` records the failed lookup and then assigns
//! anyway (`aSubRecord.c:564-575`), and only `subRecord.c`'s early return on an
//! empty name leaves a stale pointer behind, unreachable because that same
//! branch parks PACT. aSub's OTHER re-bind, in `fetch_values` under
//! `lflg == aSubLFLG_READ`, is the opposite: it returns `S_db_BadSub`
//! (`aSubRecord.c:265`) before reaching the assignment at `:272`, so "assigns
//! a failed lookup" is `special`'s property alone.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const DB: &str = r#"
record(sub, "BARE") { }
record(sub, "NAMED") { field(SNAM, "bump") }
"#;

/// `bump` adds 1, `leap` adds 100 — which one ran is readable off VAL alone.
fn step(
    by: f64,
) -> impl Fn(&mut dyn epics_base_rs::server::record::Record) -> epics_base_rs::error::CaResult<i64>
{
    move |rec: &mut dyn epics_base_rs::server::record::Record| {
        let v = rec.get_field("VAL").and_then(|v| v.to_f64()).unwrap_or(0.0);
        rec.put_field("VAL", EpicsValue::Double(v + by))?;
        Ok(0)
    }
}

async fn build() -> Arc<epics_base_rs::server::database::PvDatabase> {
    IocBuilder::new()
        .register_subroutine("bump", step(1.0))
        .register_subroutine("leap", step(100.0))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn pact(db: &epics_base_rs::server::database::PvDatabase, rec: &str) -> u8 {
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

fn val(db: &epics_base_rs::server::database::PvDatabase, rec: &str) -> f64 {
    db.get_record(rec)
        .unwrap()
        .read()
        .record
        .get_field("VAL")
        .and_then(|v| v.to_f64())
        .unwrap()
}

async fn proc(db: &epics_base_rs::server::database::PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    let _ = db.process_record_with_links(rec, &mut visited, 0).await;
}

/// Plain `caput REC.SNAM <name>` — the external put boundary, which is where C
/// runs `dbPut`'s two `special()` passes. Deliberately the NO-NOTIFY entry:
/// `dbNotify.c:225-231` defers a put-notify in front of a PACT-active record
/// (`notifyRestartInProgress`, before `putCallback`), so `caput -c` on a parked
/// `sub` waits for a restart that a permanently parked record never delivers,
/// in C exactly as here. Plain `dbPutField` has no such gate.
async fn put_snam(
    db: &epics_base_rs::server::database::PvDatabase,
    rec: &str,
    snam: &str,
) -> epics_base_rs::error::CaResult<()> {
    db.put_record_field_from_ca_no_notify(rec, "SNAM", EpicsValue::String(snam.into()))
        .await
}

/// Boundary: empty -> named. C releases the park in pass 0 and does not re-take
/// it in pass 1, so the record runs again.
#[epics_macros_rs::epics_test]
async fn naming_a_parked_sub_releases_the_park_and_it_processes() {
    let db = build().await;
    assert_eq!(pact(&db, "BARE"), 1, "an empty SNAM parks at init");

    put_snam(&db, "BARE", "bump").await.unwrap();
    assert_eq!(pact(&db, "BARE"), 0, "subRecord.c:175-178 releases it");

    proc(&db, "BARE").await;
    assert_eq!(val(&db, "BARE"), 1.0, "record support runs again");
}

/// Boundary: named -> empty. Pass 0 sees a non-empty SNAM and does nothing;
/// pass 1 sees the stored empty one and parks.
#[epics_macros_rs::epics_test]
async fn emptying_a_running_subs_snam_parks_it() {
    let db = build().await;
    assert_eq!(pact(&db, "NAMED"), 0);
    proc(&db, "NAMED").await;
    assert_eq!(val(&db, "NAMED"), 1.0, "baseline: it was processing");

    put_snam(&db, "NAMED", "").await.unwrap();
    assert_eq!(pact(&db, "NAMED"), 1, "subRecord.c:182-186 re-parks");

    proc(&db, "NAMED").await;
    assert_eq!(val(&db, "NAMED"), 1.0, "a parked record does not run");
}

/// Boundary: empty -> empty. Release then re-take is a no-op end to end.
#[epics_macros_rs::epics_test]
async fn re_emptying_an_already_parked_sub_leaves_it_parked() {
    let db = build().await;
    put_snam(&db, "BARE", "").await.unwrap();
    assert_eq!(pact(&db, "BARE"), 1);
}

/// Boundary: named -> a DIFFERENT registered name. Neither pass touches PACT,
/// and `prec->sadr` must follow the new name: the port bound the subroutine once
/// at build, so the record went on calling what its OLD name named.
#[epics_macros_rs::epics_test]
async fn renaming_a_running_sub_rebinds_it_to_the_new_routine() {
    let db = build().await;
    put_snam(&db, "NAMED", "leap").await.unwrap();
    assert_eq!(
        pact(&db, "NAMED"),
        0,
        "a named -> named put is park-neutral"
    );

    proc(&db, "NAMED").await;
    assert_eq!(val(&db, "NAMED"), 100.0, "subRecord.c:188 re-binds sadr");
}

/// Boundary: named -> a name the registry does not know. C looks it up, stores
/// the NULL, and returns `S_db_BadSub`, so the put fails AND the old binding is
/// gone; the next cycle reaches `do_sub`'s `psubroutine == NULL` arm
/// (`subRecord.c:425-428`) and runs nothing.
#[epics_macros_rs::epics_test]
async fn renaming_a_sub_to_an_unregistered_name_drops_the_old_binding() {
    let db = build().await;
    assert!(
        put_snam(&db, "NAMED", "nosuch").await.is_err(),
        "an unregistered name is S_db_BadSub"
    );

    proc(&db, "NAMED").await;
    assert_eq!(val(&db, "NAMED"), 0.0, "the old routine must not still run");
}

/// Boundary: a put to a field that is NOT `special(SPC_MOD)` for the park. C
/// never calls `special()` for it, so the park must be untouched — this is what
/// `pact_park_fields` buys over re-asserting the invariant on every put.
#[epics_macros_rs::epics_test]
async fn a_put_to_an_unrelated_field_does_not_disturb_the_park() {
    let db = build().await;
    db.put_record_field_from_ca_no_notify("BARE", "DESC", EpicsValue::String("hello".into()))
        .await
        .unwrap();
    assert_eq!(pact(&db, "BARE"), 1, "DESC is not a park field");
}
