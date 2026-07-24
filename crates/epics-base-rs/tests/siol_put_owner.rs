//! R15-63 — the SIMM-mode SIOL write is a `dbPutLink`, not a field poke.
//!
//! C `writeValue` on a simulated output record redirects the device write to
//! `dbPutLink(&prec->siol, DBR_DOUBLE, &prec->oval, 1)` (aoRecord.c:574;
//! `DBR_LONG`/`&prec->rval` in SIMM=RAW, :577). SIOL is a `DBF_OUTLINK`
//! (aoRecord.dbd:286), so that put runs the full `dbDbPutValue` body
//! (dbDbLink.c:372-393): `recGblInheritSevrMsg` MS-class inheritance into the
//! target, then `processTarget` when the link carries `PP` or names `.PROC`,
//! plus PUTF / put-notify propagation — and `setLinkAlarm` on failure.
//!
//! The port open-coded the SIOL write as a bare `put_pv_already_locked` and had
//! the CALLER raise the failure alarm, which skipped all of the above and broke
//! `write_out_link_value`'s single-raise invariant. SIOL now goes through the
//! put owner like every other OUT link.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

async fn process(db: &PvDatabase, name: &str) {
    let mut v = HashSet::new();
    db.process_record_with_links(name, &mut v, 0).await.unwrap();
}

async fn alarm_of(db: &PvDatabase, name: &str) -> (u16, AlarmSeverity) {
    let rec = db.get_record(name).expect("record exists");
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr)
}

/// A simulated ao: SIML names a record whose value is nonzero, so SIMM=YES and
/// the cycle's output is redirected to SIOL.
async fn add_simulated_ao(db: &PvDatabase, name: &str, siol: &str, val: f64) {
    db.add_record("SIM_ON", Box::new(AoRecord::new(1.0)))
        .await
        .ok();
    let mut ao = AoRecord::new(val);
    ao.siml = "SIM_ON".to_string();
    ao.siol = siol.to_string();
    db.add_record(name, Box::new(ao)).await.unwrap();
    let rec = db.get_record(name).unwrap();
    let mut inst = rec.write();
    inst.common.udf = 0;
}

/// Boundary 1 — `SIOL` with an explicit `PP` and a Passive target: C
/// `dbDbPutValue` (dbDbLink.c:387-389) processes the target. The old bare
/// field write never did, so a simulated ao driving a `PP` SIOL left the
/// downstream record unprocessed.
#[epics_macros_rs::epics_test]
async fn r15_63_siol_pp_processes_the_passive_target() {
    let db = PvDatabase::new();
    // Target: an ai whose own OUT-less process copies VAL through; the FLNK
    // it drives is what proves it processed.
    db.add_record("SIOL_TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("FLNK_TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("SIOL_TGT").unwrap();
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("FLNK_TGT".into()))
            .unwrap();
    }
    let before = db.get_record("FLNK_TGT").unwrap().read().common.time;

    add_simulated_ao(&db, "AO_SIM", "SIOL_TGT PP", 42.0).await;
    process(&db, "AO_SIM").await;

    assert_eq!(
        db.get_pv("SIOL_TGT").unwrap(),
        EpicsValue::Double(42.0),
        "the simulated output still reaches the SIOL target"
    );
    let after = db.get_record("FLNK_TGT").unwrap().read().common.time;
    assert_ne!(
        before, after,
        "a `PP` SIOL must processTarget, which runs the target's FLNK \
         (dbDbLink.c:387-389) — a bare field write does not"
    );
}

/// Boundary 2 — `SIOL` with `MS`: the writing record's severity folds into the
/// target, exactly as an MS OUT link's does (`recGblInheritSevrMsg`,
/// dbDbLink.c:382-383). The bare field write propagated nothing.
#[epics_macros_rs::epics_test]
async fn r15_63_siol_ms_inherits_the_writers_severity() {
    let db = PvDatabase::new();
    db.add_record("SIOL_TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    add_simulated_ao(&db, "AO_SIM", "SIOL_TGT MS PP", 200.0).await;
    {
        // HIHI=100/HHSV=MAJOR — the ao goes MAJOR in the cycle that writes SIOL.
        let rec = db.get_record("AO_SIM").unwrap();
        let mut inst = rec.write();
        inst.put_common_field("HIHI", EpicsValue::Double(100.0))
            .unwrap();
        inst.put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Major as i16))
            .unwrap();
    }

    process(&db, "AO_SIM").await;

    assert_eq!(
        alarm_of(&db, "AO_SIM").await.1,
        AlarmSeverity::Major,
        "precondition: VAL=200 over HIHI=100/HHSV=MAJOR"
    );
    assert_eq!(
        alarm_of(&db, "SIOL_TGT").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Major),
        "an MS SIOL folds the writer's severity into the target — SIOL is a \
         DBF_OUTLINK put through dbPutLink, not a field poke"
    );
}

/// Boundary 3 — the failure path still alarms, and now the OWNER raises it
/// (R14-62 must keep passing with the caller's raise removed).
#[epics_macros_rs::epics_test]
async fn r15_63_failed_siol_put_is_alarmed_by_the_put_owner() {
    let db = PvDatabase::new();
    add_simulated_ao(&db, "AO_SIM", "NO_SUCH_SIOL", 5.0).await;

    process(&db, "AO_SIM").await;

    assert_eq!(
        alarm_of(&db, "AO_SIM").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Invalid),
        "a failed SIOL put raises LINK/INVALID from inside write_out_link_value \
         (dbLink.c:444-446 setLinkAlarm), in the same cycle"
    );
}
