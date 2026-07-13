//! R17-69: link-status classification must not race the database load.
//!
//! C classifies a record's links in `init_record`, which `iocInit` runs only
//! after the WHOLE database is loaded (`dbAccess.c::iocInit` →
//! `initDatabase`/`initialProcess`), so a link that forward-references a record
//! defined further down the same `.db` is a LOCAL link — deterministically.
//! (C then re-polls with `checkLinksCallback` 0.5 s later, which only exists
//! because a CA link needs connection time; it is not what makes the local case
//! deterministic.)
//!
//! The port spawned the refresh from `add_record`, so it read a half-built
//! database: a forward-referenced local link classified as `EXT_NC` (0) or
//! `LOC` (2) depending on task scheduling — measured 1/20 runs EXT_NC before
//! the fix. The load guard (`PvDatabase::begin_load`) is the port's `iocInit`
//! boundary: `classify_link` awaits it, so every classification reads the
//! finished database by construction.
//!
//! softIoc (EPICS 7.0.10, linux-x86_64), a `.db` in which `CO`'s `INPA` points
//! at `TARGET`, defined AFTER it, `dbpr CO 2` right after `iocInit`:
//!
//! ```text
//! INPA: DB_LINK TARGET.VAL NPP NMS   INAV: LOC
//! ```

use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::types::EpicsValue;

/// `menu(calcoutINAV)`: 0 = Ext PV NC, 1 = Ext PV OK, 2 = Local PV, 3 = Constant.
const LINK_EXT_NC: u16 = 0;
const LINK_LOC: u16 = 2;

async fn inav(db: &PvDatabase, rec: &str) -> u16 {
    let inst = db.get_record(rec).await.unwrap();
    let inst = inst.read().await;
    match inst.record.get_field("INAV") {
        Some(EpicsValue::Enum(v)) => v,
        other => panic!("INAV: {other:?}"),
    }
}

/// The whole load is one `iocInit`: a link that forward-references a record
/// created later in the same load is a Local PV, not an unconnected external
/// one — even when the refresh task gets to run in between.
#[tokio::test]
async fn forward_referenced_local_link_classifies_as_local() {
    let db = Arc::new(PvDatabase::new());
    let load = db.begin_load();

    let mut co = CalcoutRecord::default();
    co.calc = "A".to_string();
    co.inpa = "TARGET.VAL".to_string();
    db.add_record("CO", Box::new(co)).await.unwrap();

    // Give the refresh task every chance to run to completion mid-load. This
    // is what made the pre-fix failure a coin flip in production and a
    // certainty here: without the gate the task classifies TARGET — which does
    // not exist yet — as Ext PV NC and never revisits it.
    tokio::time::sleep(Duration::from_millis(50)).await;

    db.add_record("TARGET", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();

    // Still loading: the classification has not been published yet.
    drop(load);
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        inav(&db, "CO").await,
        LINK_LOC,
        "a forward reference resolved inside the same load is a Local PV"
    );
}

/// The gate does not paper over a genuinely absent target: a DB-syntax link to
/// a record that no load ever creates stays Ext PV NC (C `init_record`'s else
/// branch, `dbNameToAddr` failing).
#[tokio::test]
async fn unresolvable_link_still_classifies_as_ext_nc() {
    let db = Arc::new(PvDatabase::new());
    {
        let _load = db.begin_load();
        let mut co = CalcoutRecord::default();
        co.calc = "A".to_string();
        co.inpa = "NOSUCH.VAL".to_string();
        db.add_record("CO", Box::new(co)).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        inav(&db, "CO").await,
        LINK_EXT_NC,
        "a target that never loads is an unconnected external PV"
    );
}

/// A record created outside any load (no guard) classifies immediately — the
/// gate is open when nothing is loading, so runtime `dbCreateRecord` and
/// `special()` re-points are unaffected.
#[tokio::test]
async fn no_load_in_progress_classifies_immediately() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TARGET", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();

    let mut co = CalcoutRecord::default();
    co.calc = "A".to_string();
    co.inpa = "TARGET.VAL".to_string();
    db.add_record("CO", Box::new(co)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(inav(&db, "CO").await, LINK_LOC);
}

/// The `.db` path, which is where C's ordering guarantee actually bites: `CO`'s
/// `INPA` names `TARGET`, defined AFTER it in the same file. `IocBuilder::build`
/// is one load, so the link is Local — the softIoc `dbpr` above.
#[tokio::test]
async fn db_file_forward_reference_is_local() {
    const DB: &str = r#"
record(calcout, "CO") {
    field(CALC, "A")
    field(INPA, "TARGET.VAL")
}
record(ai, "TARGET") {
    field(VAL, "1")
}
"#;
    let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        inav(&db, "CO").await,
        LINK_LOC,
        "a forward reference within one .db is a Local PV"
    );
}
