//! An output's `recGblGetTimeStampSimm(prec, prec->simm, NULL)` runs before
//! `writeValue` reads SIML, so it sees the SIMM the previous cycle left.
//!
//! softIoc `R7.0.10-146-g8f5015b66`, `TSE=-2`, SIML from a longin: the cycle
//! where SIML turns SIMM on leaves `TIME` undefined, the next cycle stamps
//! the clock, the cycle where SIML turns it off still stamps, and the one
//! after keeps that stamp.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

const STALE: Duration = Duration::new(3_000_000, 333_333_333);
const OUTS: [&str; 4] = ["O_BO", "O_LO", "O_SO", "O_AO"];

const DB_TEXT: &str = r#"
record(longin, "SW") {}
record(bo, "O_BO")        { field(SIML, "SW") field(TSE, "-2") }
record(longout, "O_LO")   { field(SIML, "SW") field(TSE, "-2") }
record(stringout, "O_SO") { field(SIML, "SW") field(TSE, "-2") }
record(ao, "O_AO")        { field(SIML, "SW") field(TSE, "-2") }
"#;

async fn cycle(db: &Db, rec: &str) -> SystemTime {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
    db.get_record(rec).unwrap().read().common.time
}

async fn all(db: &Db) -> Vec<SystemTime> {
    let mut v = Vec::new();
    for r in OUTS {
        v.push(cycle(db, r).await);
    }
    v
}

#[epics_macros_rs::epics_test]
async fn the_stamp_follows_the_simm_of_the_previous_cycle() {
    let db: Db = IocBuilder::new()
        .db_string(DB_TEXT, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let stale = SystemTime::UNIX_EPOCH + STALE;
    for r in OUTS {
        db.get_record(r).unwrap().write().common.time = stale;
    }
    let set_sw = |v: i32| {
        let db = db.clone();
        async move {
            db.put_record_field_from_ca("SW", "VAL", EpicsValue::Long(v))
                .await
                .unwrap();
        }
    };
    let start = SystemTime::now();

    assert_eq!(all(&db).await, vec![stale; 4], "first, SW=0");
    set_sw(1).await;
    assert_eq!(all(&db).await, vec![stale; 4], "SIML turns SIMM on");
    let on = all(&db).await;
    assert!(on.iter().all(|t| *t >= start), "on again: {on:?}");
    set_sw(0).await;
    let off = all(&db).await;
    assert!(
        off.iter().all(|t| *t >= on[0]),
        "SIML turns SIMM off: {off:?}"
    );
    assert_eq!(all(&db).await, off, "off again keeps the stamp");
}
