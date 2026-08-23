//! The PACT release `special_before_put` performs, and the put-notify restart
//! that release owes C.
//!
//! C `dbNotify.c:225-231` parks a put-notify that lands on a PACT record — it
//! writes nothing and becomes the record's owner (`notifyRestartInProgress`) —
//! and `dbNotifyCompletion` (`:468-470`) replays it, from `recGblFwdLink`
//! (`recGbl.c:295`) at the TAIL of the cycle that released PACT, never from the
//! `pact = FALSE` store itself.
//!
//! A `sub` with an empty SNAM is parked for the life of the IOC
//! (`subRecord.c:119-122`), so it is the one record where the park is a
//! standing state a put can walk into. `subRecord.c::special` pass 0
//! (`:183-187`) is what releases it, and that release is the only place in the
//! port where a `PactExit` is produced under the put's own record write guard.
//!
//! The boundaries are the two the release has to satisfy at once: the parked
//! put must reach the restart consumer on every path out of the put body, and
//! the consumer must run with the record's DATA lock DOWN — it re-enters the
//! record, and `parking_lot::RwLock` is not reentrant, so arming from inside
//! the guard is a hang rather than an error. Each entry path that can reach
//! `special_before_put` is driven once: the CA route
//! (`put_record_field_from_ca_body`) and the `dbPutLink` route (`put_pv`).

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::ProcessCompletion;
use epics_base_rs::types::EpicsValue;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const DB: &str = r#"
record(sub, "PARKED") { }
record(sub, "VIALINK") { }
record(stringout, "NAMER") { field(OUT, "VIALINK.SNAM PP") field(VAL, "bump") }
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

/// The restart is queued, not recursed (C `callbackRequest`), so it lands on a
/// later turn of the executor.
async fn settle_until(db: &Db, rec: &str, want: f64) -> f64 {
    for _ in 0..400 {
        let v = val(db, rec);
        if v == want {
            return v;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
    }
    val(db, rec)
}

/// CA route. A put-notify onto the parked record writes nothing and waits; the
/// SNAM put that releases the park is what replays it.
#[epics_macros_rs::epics_test]
async fn a_parked_put_notify_replays_when_the_snam_put_releases_the_park() {
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
        "dbNotify.c:225-231 tests PACT ABOVE the put — nothing is written"
    );

    db.put_record_field_from_ca_no_notify("PARKED", "SNAM", EpicsValue::String("bump".into()))
        .await
        .expect("the no-notify route has no PACT gate");
    assert_eq!(pact(&db, "PARKED"), 0, "the park is released");

    // 7.0 from the replayed put, then +1.0 from `bump` on the process the
    // replay drives (sub VAL is `pp(TRUE)`).
    assert_eq!(
        settle_until(&db, "PARKED", 8.0).await,
        8.0,
        "the release owes the parked put a restart"
    );
}

/// `dbPutLink` route — an OUT link driving the same SNAM field. Same release,
/// different put body, so the restart has to be armed there too.
#[epics_macros_rs::epics_test]
async fn a_parked_put_notify_replays_when_an_out_link_releases_the_park() {
    let db = build().await;
    assert_eq!(pact(&db, "VIALINK"), 1);

    let parked = db
        .put_record_field_from_ca("VIALINK", "VAL", EpicsValue::Double(3.0))
        .await
        .expect("parks");
    assert!(matches!(parked, ProcessCompletion::Async(_)));

    let mut visited = HashSet::new();
    let _ = db.process_record_with_links("NAMER", &mut visited, 0).await;
    assert_eq!(pact(&db, "VIALINK"), 0, "the OUT link named it");

    assert_eq!(settle_until(&db, "VIALINK", 4.0).await, 4.0);
}

/// Negative control: a put to a field that is not `special(SPC_MOD)` for the
/// park releases nothing, so there is no restart to arm and the parked put is
/// still waiting.
#[epics_macros_rs::epics_test]
async fn a_put_to_an_unrelated_field_arms_no_restart() {
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
        "no release, no restart: the put is still parked"
    );
}
