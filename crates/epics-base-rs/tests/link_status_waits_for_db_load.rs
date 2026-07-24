//! R17-69 / R18-92: link-status classification must observe the database C
//! classifies against — the one `iocInit` sees, after EVERY `dbLoadRecords`.
//!
//! C classifies a record's links in `init_record`, which `iocInit` runs once
//! the whole database is loaded, so a link that forward-references a record
//! defined further down the same `.db` — or in a `.db` loaded by a LATER
//! `dbLoadRecords` — is a LOCAL link, deterministically. The classified value
//! is also FINAL when `iocInit` returns: C refuses `dbgf` before that point and
//! answers it immediately after.
//!
//! R17-69 gated classification on the load GROUP, which left the
//! multi-`dbLoadRecords` case every real `st.cmd` uses racing (measured 9-in-15
//! misclassified as `Ext PV NC`). The boundary is now the ioc-lifecycle
//! `PvDatabase::ioc_init`: a classification issued while records are loading is
//! QUEUED, and `ioc_init` runs the queue against the finished database.
//!
//! softIoc (EPICS 7.0.10.1-DEV, linux-x86_64) — `a.db` holds `CO` with
//! `INPA="LATER.VAL"`, `b.db` holds `LATER`:
//!
//! ```text
//! epics> dbLoadRecords("a.db")
//! epics> dbLoadRecords("b.db")
//! epics> dbgf CO.INAV
//! dbgf only works after iocInit
//! epics> iocInit
//! epics> dbgf CO.INAV
//! DBF_STRING:         "Local PV"
//! ```

// RTEMS-EXEC-MODEL-ALLOW(9): checked - these run and pass in the feature-ON suite.

use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::types::EpicsValue;

/// `menu(calcoutINAV)`: 0 = Ext PV NC, 1 = Ext PV OK, 2 = Local PV, 3 = Constant.
const LINK_EXT_NC: u16 = 0;
const LINK_LOC: u16 = 2;
const LINK_CONST: u16 = 3;

async fn inav(db: &PvDatabase, rec: &str) -> u16 {
    let inst = db.get_record(rec).unwrap();
    let inst = inst.read();
    match inst.record.get_field("INAV") {
        Some(EpicsValue::Enum(v)) => v,
        other => panic!("INAV: {other:?}"),
    }
}

async fn add_calcout(db: &PvDatabase, name: &str, inpa: &str) {
    let mut co = CalcoutRecord::default();
    co.calc = "A".to_string();
    co.inpa = inpa.to_string();
    db.add_record(name, Box::new(co)).await.unwrap();
}

/// The load phase spans every load, not one: `CO` is created by the first load
/// and its target by a SECOND one, exactly as an `st.cmd` with two
/// `dbLoadRecords` lines does. C says Local PV; so must the port.
#[tokio::test]
async fn forward_reference_across_two_loads_is_local() {
    let db = Arc::new(PvDatabase::new());

    // First `dbLoadRecords`.
    db.begin_load().unwrap();
    add_calcout(&db, "CO", "LATER.VAL").await;

    // Between the two loads the pre-fix code had already closed its load group
    // and classified `CO` against a database without `LATER` in it. Give any
    // such task every chance to run.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Second `dbLoadRecords`.
    db.begin_load().unwrap();
    db.add_record("LATER", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();

    db.ioc_init().await;
    assert_eq!(
        inav(&db, "CO").await,
        LINK_LOC,
        "a forward reference resolved by a later dbLoadRecords is a Local PV"
    );
}

/// The classification is FINAL when `iocInit` returns — no sleep, no re-poll.
/// The pre-fix spawn left `INAV` at its struct default (`Constant`) for a
/// caller that read it straight after the load.
#[tokio::test]
async fn link_status_is_final_when_ioc_init_returns() {
    let db = Arc::new(PvDatabase::new());
    db.begin_load().unwrap();
    add_calcout(&db, "CO", "TARGET.VAL").await;
    db.add_record("TARGET", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    db.ioc_init().await;

    assert_eq!(
        inav(&db, "CO").await,
        LINK_LOC,
        "iocInit returned, so INAV is classified — not the struct default"
    );
}

/// A forward reference inside ONE load — the case R17-69 closed — stays closed.
#[tokio::test]
async fn forward_referenced_local_link_classifies_as_local() {
    let db = Arc::new(PvDatabase::new());
    db.begin_load().unwrap();

    add_calcout(&db, "CO", "TARGET.VAL").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    db.add_record("TARGET", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();

    db.ioc_init().await;
    assert_eq!(inav(&db, "CO").await, LINK_LOC);
}

/// The barrier does not paper over a genuinely absent target: a DB-syntax link
/// to a record that no load ever creates stays Ext PV NC (C `init_record`'s
/// else branch, `dbNameToAddr` failing).
#[tokio::test]
async fn unresolvable_link_still_classifies_as_ext_nc() {
    let db = Arc::new(PvDatabase::new());
    db.begin_load().unwrap();
    add_calcout(&db, "CO", "NOSUCH.VAL").await;
    db.ioc_init().await;

    assert_eq!(
        inav(&db, "CO").await,
        LINK_EXT_NC,
        "a target that never loads is an unconnected external PV"
    );
}

/// A record created on a COMPLETE database (no load phase) classifies at once —
/// the runtime `dbCreateRecord` / `special()` re-point path, which must not wait
/// for an `iocInit` that already happened.
#[tokio::test]
async fn no_load_in_progress_classifies_immediately() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TARGET", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    add_calcout(&db, "CO", "TARGET.VAL").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(inav(&db, "CO").await, LINK_LOC);
}

/// Records added AFTER the barrier classify immediately as well — `ioc_init`
/// leaves the database complete, it does not park later work.
#[tokio::test]
async fn record_added_after_ioc_init_classifies_immediately() {
    let db = Arc::new(PvDatabase::new());
    db.begin_load().unwrap();
    db.add_record("TARGET", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    db.ioc_init().await;

    add_calcout(&db, "CO", "TARGET.VAL").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(inav(&db, "CO").await, LINK_LOC);
}

/// R19-62 — the lifecycle is ONE-WAY. `begin_load` after `iocInit` must not
/// re-open the LOAD phase: the queue it would re-arm is drained by `ioc_init`
/// alone, so everything pushed into it afterwards — every later record's
/// classification — would sit there forever.
///
/// Measured on the port before the fix: `iocInit; dbLoadRecords(b.db); dbpf
/// CO.INPA "9.5"` left `CO.INAV` at 0, where a plain `iocInit; dbpf CO.INPA
/// "9.5"` gives 3 (Constant).
#[tokio::test]
async fn begin_load_after_ioc_init_does_not_re_open_the_load_phase() {
    let db = Arc::new(PvDatabase::new());
    db.begin_load().unwrap();
    db.add_record("TARGET", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    db.ioc_init().await;

    // A `dbLoadRecords` typed at the running iocsh prompt. C refuses it
    // (R19-63) — and even if a caller ignores the refusal, the phase is
    // terminal.
    assert!(db.begin_load().is_err());

    // Anything classified from here on must still run: the phase is terminal, so
    // this is a spawn, not a push into a queue nothing drains.
    add_calcout(&db, "CO", "TARGET.VAL").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        inav(&db, "CO").await,
        LINK_LOC,
        "a post-iocInit begin_load re-armed the queue: the classification was stranded"
    );
}

/// The same boundary from the other owner of `schedule_record_init` — the
/// runtime `special()` link re-point (`calcout.rs`, `sseq.rs`, `swait.rs`,
/// std-rs `throttle.rs`). A put to `INPA` re-classifies the link; after a
/// post-iocInit `begin_load` it was stranded, freezing `INAV` at its old value.
#[tokio::test]
async fn runtime_link_re_point_still_classifies_after_a_post_init_load() {
    let db = Arc::new(PvDatabase::new());
    db.begin_load().unwrap();
    add_calcout(&db, "CO", "TARGET.VAL").await;
    db.add_record("TARGET", Box::new(AiRecord::new(1.0)))
        .await
        .unwrap();
    db.ioc_init().await;
    assert_eq!(inav(&db, "CO").await, LINK_LOC);

    assert!(db.begin_load().is_err());

    // `dbpf CO.INPA "9.5"` — the link becomes a CONSTANT.
    db.put_pv("CO.INPA", EpicsValue::String("9.5".into()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        inav(&db, "CO").await,
        LINK_CONST,
        "the special() re-point must classify against the running database"
    );
}

/// The `.db` path: `CO`'s `INPA` names `TARGET`, defined AFTER it in the same
/// file. `IocBuilder::build` runs the barrier, so the status is Local — and
/// final the moment `build` returns.
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

    assert_eq!(
        inav(&db, "CO").await,
        LINK_LOC,
        "a forward reference within one .db is a Local PV, classified by build's iocInit"
    );
}
