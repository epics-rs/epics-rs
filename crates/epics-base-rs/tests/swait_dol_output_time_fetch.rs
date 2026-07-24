//! R9-65 — swait DOPT="Use DOL" fetches the DOL link LIVE, at output time.
//!
//! C `swaitRecord.c::execOutput` (761-774):
//!
//! ```c
//! if (pwait->dopt) {                     /* DOPT = "Use DOL" */
//!     if (!pwait->dolv) {                /* DOL PV connected */
//!         oldDold = pwait->dold;
//!         recDynLinkGet(&pcbst->caLinkStruct[DOL_INDEX], &(pwait->dold), ...);
//!         if (pwait->dold != oldDold)
//!             db_post_events(pwait, &pcbst->pwait->dold, DBE_VALUE);
//!     }
//!     outValue = pwait->dold;
//! } else {
//!     outValue = pwait->val;
//! }
//! ```
//!
//! The port had no DOL link at all (no DOLN field): DOPT=1 wrote out whatever
//! DOLD happened to hold — an init value or a client put — so a swait
//! configured to drive its OUT from another PV drove a constant instead.
//!
//! The fetch is at OUTPUT time, not in the input-fetch phase: it happens only
//! on a cycle whose output fires (ODLY delay-end included), so a non-firing
//! cycle neither refreshes DOLD nor posts it.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

/// swait "W": CALC="A" (VAL := A = 1), OUT→W_TGT, DOLD pre-put to 99 — the
/// stale value the port used to drive out. `dopt`/`doln`/`oopt`/`odly` per arg.
async fn swait_db(dopt: i16, doln: &str, oopt: i16, odly: f32) -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("W_SRC", Box::new(AiRecord::new(5.0)))
        .await
        .unwrap();
    db.add_record("W_TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut w = SwaitRecord::default();
    w.put_field("CALC", EpicsValue::String("A".into())).unwrap();
    w.put_field("A", EpicsValue::Double(1.0)).unwrap();
    w.put_field("OOPT", EpicsValue::Short(oopt)).unwrap();
    w.put_field("ODLY", EpicsValue::Float(odly)).unwrap();
    w.put_field("DOPT", EpicsValue::Short(dopt)).unwrap();
    if !doln.is_empty() {
        w.put_field("DOLN", EpicsValue::String(doln.into()))
            .unwrap();
    }
    w.put_field("DOLD", EpicsValue::Double(99.0)).unwrap();
    db.add_record("W", Box::new(w)).await.unwrap();

    // OUT routes through RecordInstance::put_common_field (populates parsed_out).
    let r = db.get_record("W").unwrap();
    r.write()
        .put_common_field("OUT", EpicsValue::String("W_TGT".into()))
        .unwrap();
    db
}

async fn field(db: &PvDatabase, rec: &str, f: &str) -> Option<f64> {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field(f).and_then(|v| v.to_f64())
}

#[epics_macros_rs::epics_test]
async fn r9_65_use_dol_drives_out_from_the_live_dol_link() {
    let db = swait_db(1, "W_SRC", 0, 0.0).await;

    let mut v = HashSet::new();
    db.process_record_with_links("W", &mut v, 0).await.unwrap();

    assert_eq!(
        field(&db, "W", "DOLD").await,
        Some(5.0),
        "execOutput fetches DOL into DOLD (swaitRecord.c:767); the pre-put 99 \
         is overwritten by the link value"
    );
    assert_eq!(
        db.get_pv("W_TGT").unwrap().to_f64(),
        Some(5.0),
        "DOPT=Use DOL writes DOLD — the freshly fetched 5, not the stale 99 \
         and not VAL (=1)"
    );

    // The fetch is live on EVERY firing cycle: move the source, re-process.
    db.put_pv("W_SRC", EpicsValue::Double(6.5)).await.unwrap();
    let mut v = HashSet::new();
    db.process_record_with_links("W", &mut v, 0).await.unwrap();
    assert_eq!(
        db.get_pv("W_TGT").unwrap().to_f64(),
        Some(6.5),
        "the next firing cycle re-fetches DOL: OUT follows the source"
    );
}

/// DOPT="Use VAL" never reads DOL — C guards the `recDynLinkGet` with
/// `if (pwait->dopt)`. DOLD keeps its client-put value and VAL goes out.
#[epics_macros_rs::epics_test]
async fn r9_65_use_val_never_reads_the_dol_link() {
    let db = swait_db(0, "W_SRC", 0, 0.0).await;

    let mut v = HashSet::new();
    db.process_record_with_links("W", &mut v, 0).await.unwrap();

    assert_eq!(
        field(&db, "W", "DOLD").await,
        Some(99.0),
        "DOPT=Use VAL: C never fetches DOL, so the client-put DOLD stands"
    );
    assert_eq!(
        db.get_pv("W_TGT").unwrap().to_f64(),
        Some(1.0),
        "DOPT=Use VAL writes VAL (=A=1)"
    );
}

/// DOPT="Use DOL" with DOLN unset is C's `dolv == NO_PV`: the get is skipped
/// and the current DOLD — whatever a client put there — is what goes out.
#[epics_macros_rs::epics_test]
async fn r9_65_use_dol_without_a_link_writes_the_client_put_dold() {
    let db = swait_db(1, "", 0, 0.0).await;

    let mut v = HashSet::new();
    db.process_record_with_links("W", &mut v, 0).await.unwrap();

    assert_eq!(
        db.get_pv("W_TGT").unwrap().to_f64(),
        Some(99.0),
        "no DOL PV configured: the DOLD put by the client is the output value"
    );
}

/// A cycle whose output does not fire (OOPT="Never") must not read DOL at all
/// — C reaches the `recDynLinkGet` only from inside `execOutput`, which
/// `process` calls only when the OOPT test passes.
#[epics_macros_rs::epics_test]
async fn r9_65_non_firing_cycle_does_not_refresh_dold() {
    let db = swait_db(1, "W_SRC", 6, 0.0).await; // OOPT=6 = "Never"

    let mut v = HashSet::new();
    db.process_record_with_links("W", &mut v, 0).await.unwrap();

    assert_eq!(
        field(&db, "W", "DOLD").await,
        Some(99.0),
        "OOPT=Never: execOutput never runs, so DOL is never fetched and DOLD \
         keeps 99"
    );
    assert_eq!(
        db.get_pv("W_TGT").unwrap().to_f64(),
        Some(0.0),
        "OOPT=Never writes no output at all"
    );
}

/// ODLY>0: C schedules `execOutput` on the watchdog, so the DOL fetch happens
/// at delay END. A source that moves during the delay window is picked up.
#[epics_macros_rs::epics_test]
async fn r9_65_odly_fetches_dol_at_delay_end_not_delay_start() {
    let db = swait_db(1, "W_SRC", 0, 100.0).await;

    // Delaying cycle: C defers execOutput — no fetch, no write.
    let mut v1 = HashSet::new();
    db.process_record_with_links("W", &mut v1, 0).await.unwrap();
    assert_eq!(
        field(&db, "W", "DOLD").await,
        Some(99.0),
        "the delay-START cycle must not fetch DOL — C's fetch lives in \
         execOutput, which the watchdog has not run yet"
    );
    assert_eq!(
        db.get_pv("W_TGT").unwrap().to_f64(),
        Some(0.0),
        "the delay-start cycle writes no output"
    );

    // Source moves during the delay window.
    db.put_pv("W_SRC", EpicsValue::Double(8.0)).await.unwrap();

    // Continuation (watchdog cycle) = C's execOutput: fetch DOL, then write.
    let mut v2 = HashSet::new();
    db.process_record_continuation("W", &mut v2, 0)
        .await
        .unwrap();
    assert_eq!(
        field(&db, "W", "DOLD").await,
        Some(8.0),
        "the delay-END fetch sees the value the source holds NOW (8), not the \
         one it held when the delay started (5)"
    );
    assert_eq!(
        db.get_pv("W_TGT").unwrap().to_f64(),
        Some(8.0),
        "the continuation writes the freshly fetched DOLD"
    );
}
