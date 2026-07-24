//! SDLY ("Sim. Mode Async Delay") makes a simulated read/write asynchronous.
//!
//! C `aiRecord.c::readValue` (488-508) / `aoRecord.c::writeValue` (571-587):
//! when `SIMM` is YES/RAW and `SDLY >= 0` on the fresh cycle (`!pact`), the
//! record schedules `callbackRequestProcessCallbackDelayed(..., sdly)`, sets
//! `pact = TRUE`, and `process()` returns 0 — posting NOTHING (no value, no
//! alarm, no monitor, no forward link). The SIOL round-trip and the SIMM-alarm
//! tail run only on the delayed re-entry, where `pact` is true and the
//! synchronous branch runs (`pact || sdly < 0`). With the default `SDLY = -1.0`
//! the read/write is synchronous on the single cycle.
//!
//! The Rust port models the async path with `SimOutcome::DeferRead`: the fresh
//! `check_simulation_mode` holds PACT and schedules a `ReprocessAfter`; the
//! continuation (`is_continuation = true`, the framework analog of C's entry
//! `pact`) runs the synchronous SIOL branch + alarm tail and releases PACT.
//!
//! These tests drive the continuation directly (`process_record_continuation`,
//! as the swait/scalcout ODLY tests do) so the assertions are deterministic and
//! do not race the real timer (`SDLY = 100s` makes it unfireable here).

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

async fn is_processing(db: &PvDatabase, name: &str) -> bool {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    inst.is_processing()
}

/// Input record: `SDLY >= 0` defers the SIOL read (and the SIMM-alarm tail) to
/// the continuation while holding PACT. On the fresh cycle VAL is untouched and
/// no alarm is raised; the continuation reads SIOL into VAL, raises SIMM_ALARM,
/// and clears PACT.
#[epics_macros_rs::epics_test]
async fn sdly_async_defers_input_sim_read_to_continuation() {
    let db = PvDatabase::new();
    // SIML source reads 1 -> SIMM=YES; SIOL source carries the simulated value.
    db.add_record("SDLY_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("SDLY_SRC", Box::new(AoRecord::new(42.0)))
        .await
        .unwrap();

    let mut ai = AiRecord::new(0.0);
    ai.siml = "SDLY_SW".to_string();
    ai.siol = "SDLY_SRC".to_string();
    ai.sims = 1; // SIMM severity = MINOR
    ai.sdly = 100.0; // async; the real timer is unfireable in this test
    db.add_record("SDLY_AI", Box::new(ai)).await.unwrap();

    // Fresh (delaying) cycle: PACT held, sim read deferred — VAL untouched, no
    // alarm. C `process()` returns 0 on the async-start pass.
    let mut v1 = HashSet::new();
    db.process_record_with_links("SDLY_AI", &mut v1, 0)
        .await
        .unwrap();

    assert!(
        is_processing(&db, "SDLY_AI").await,
        "PACT held across the SDLY delay (foreign process must bail)"
    );
    let val = db.get_pv("SDLY_AI").unwrap();
    assert!(
        matches!(val, EpicsValue::Double(v) if v == 0.0),
        "VAL untouched on the delaying cycle (sim read deferred), got {val:?}"
    );
    // A never-processed record carries C's init UDF status (`doInitRecord0`:
    // STAT=UDF, SEVR=UDFS=INVALID — softIoc reads that on every record right
    // after `iocInit`), and the delaying cycle commits no alarms of its own. So
    // "the SIMM alarm has not been raised yet" is a STATUS check: STAT is still
    // UDF, not SIMM.
    let stat = db.get_pv("SDLY_AI.STAT").unwrap();
    assert!(
        matches!(stat, EpicsValue::Short(s) if s as u16 == alarm_status::UDF_ALARM),
        "SIMM_ALARM not raised on the delaying cycle (alarm tail deferred), got {stat:?}"
    );

    // Continuation: sync SIOL read -> VAL=42, SIMM_ALARM raised, PACT cleared.
    let mut v2 = HashSet::new();
    db.process_record_continuation("SDLY_AI", &mut v2, 0)
        .await
        .unwrap();

    let val = db.get_pv("SDLY_AI").unwrap();
    assert!(
        matches!(val, EpicsValue::Double(v) if (v - 42.0).abs() < 1e-10),
        "VAL read from SIOL on the continuation, got {val:?}"
    );
    assert!(
        !is_processing(&db, "SDLY_AI").await,
        "PACT cleared on the continuation (C readValue sets pact=FALSE)"
    );
    let sevr = db.get_pv("SDLY_AI.SEVR").unwrap();
    assert!(
        matches!(sevr, EpicsValue::Short(1)),
        "SIMM_ALARM (MINOR) raised on the continuation, got {sevr:?}"
    );
}

/// Regression guard: the default `SDLY = -1.0` reads synchronously on the single
/// cycle (`pact || sdly < 0` takes the sync branch) — the async defer is gated
/// strictly on `sdly >= 0`, so VAL is set immediately and PACT is never held.
#[epics_macros_rs::epics_test]
async fn sdly_negative_reads_input_synchronously() {
    let db = PvDatabase::new();
    db.add_record("SDLYN_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("SDLYN_SRC", Box::new(AoRecord::new(42.0)))
        .await
        .unwrap();

    let mut ai = AiRecord::new(0.0);
    ai.siml = "SDLYN_SW".to_string();
    ai.siol = "SDLYN_SRC".to_string();
    ai.sims = 1;
    // sdly left at the default -1.0 (synchronous).
    db.add_record("SDLYN_AI", Box::new(ai)).await.unwrap();

    let mut v1 = HashSet::new();
    db.process_record_with_links("SDLYN_AI", &mut v1, 0)
        .await
        .unwrap();

    let val = db.get_pv("SDLYN_AI").unwrap();
    assert!(
        matches!(val, EpicsValue::Double(v) if (v - 42.0).abs() < 1e-10),
        "VAL read synchronously on the single cycle (sdly < 0), got {val:?}"
    );
    assert!(
        !is_processing(&db, "SDLYN_AI").await,
        "no PACT hold for sdly < 0 (synchronous sim read)"
    );
}

/// C latches the simulation mode on the fresh cycle: `recGblGetSimm`
/// (the SIML -> SIMM resolution) runs only `if (!prec->pact)`
/// (aiRecord.c:475 / aoRecord.c:558), so the async re-entry keeps the SIMM
/// decided on the fresh cycle and is NOT re-resolved from SIML. If the SIML
/// source flips away from YES during the SDLY delay window (a real operator
/// action — SIML is commonly a PV that toggles sim mode), C still completes the
/// deferred SIOL sim read using the latched SIMM. The port mirrors this by
/// gating the SIML read on `!is_continuation`; without that gate the
/// continuation would re-read SIML, switch to `NotSimulated`, and read the real
/// (here empty) device — a value-source divergence.
#[epics_macros_rs::epics_test]
async fn sdly_continuation_keeps_simm_latched_from_fresh_cycle() {
    let db = PvDatabase::new();
    db.add_record("SDLYL_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("SDLYL_SRC", Box::new(AoRecord::new(42.0)))
        .await
        .unwrap();

    let mut ai = AiRecord::new(0.0);
    ai.siml = "SDLYL_SW".to_string();
    ai.siol = "SDLYL_SRC".to_string();
    ai.sims = 1;
    ai.sdly = 100.0;
    db.add_record("SDLYL_AI", Box::new(ai)).await.unwrap();

    // Fresh cycle: SIML reads 1 -> SIMM=YES, defer (PACT held), SIMM latched.
    let mut v1 = HashSet::new();
    db.process_record_with_links("SDLYL_AI", &mut v1, 0)
        .await
        .unwrap();
    assert!(
        is_processing(&db, "SDLYL_AI").await,
        "PACT held across the SDLY delay"
    );
    let simm = db.get_pv("SDLYL_AI.SIMM").unwrap();
    assert!(
        matches!(simm, EpicsValue::Short(1)),
        "SIMM latched YES on the fresh cycle, got {simm:?}"
    );

    // Operator flips the simulation switch OFF during the delay window.
    db.put_pv_no_process("SDLYL_SW", EpicsValue::Double(0.0))
        .await
        .unwrap();

    // Continuation: C keeps SIMM latched (YES) and completes the SIOL sim read.
    // The SIML re-read is gated on `!is_continuation`, so SIMM is NOT
    // re-resolved to NO and the record does not fall through to the real device.
    let mut v2 = HashSet::new();
    db.process_record_continuation("SDLYL_AI", &mut v2, 0)
        .await
        .unwrap();

    let simm = db.get_pv("SDLYL_AI.SIMM").unwrap();
    assert!(
        matches!(simm, EpicsValue::Short(1)),
        "SIMM stays latched YES on the continuation (not re-resolved from SIML), got {simm:?}"
    );
    let val = db.get_pv("SDLYL_AI").unwrap();
    assert!(
        matches!(val, EpicsValue::Double(v) if (v - 42.0).abs() < 1e-10),
        "deferred SIOL sim read completes with the latched SIMM, got {val:?}"
    );
    assert!(
        !is_processing(&db, "SDLYL_AI").await,
        "PACT cleared on the continuation"
    );
}

/// C resolves the simulation mode in `recGblGetSimm` guarded by
/// `if (!prec->pact)`, so it re-resolves SIMM from SIML on EVERY `pact=FALSE`
/// entry — not only the literal first cycle. A `bo` with `HIGH > 0` arms a
/// one-shot reset that re-processes the record WITHOUT holding PACT (it returns
/// `Complete`, not async), so that re-process is `pact=FALSE` and C re-resolves
/// SIMM there. The port keys the SIML read on the actual PACT state
/// (`!pact_held`, `is_processing()` at entry), not on "re-entered via a token"
/// (`is_continuation`) — which would conflate this `pact=FALSE` re-trigger with
/// a PACT-holding SDLY/ODLY continuation and wrongly keep the stale SIMM. Here
/// the sim switch flips NO->YES before the HIGH reset fires; the re-trigger must
/// re-resolve SIMM=YES, matching C. (Twin of
/// `sdly_continuation_keeps_simm_latched_from_fresh_cycle`, which pins the other
/// boundary: a PACT-held continuation must NOT re-resolve.)
#[epics_macros_rs::epics_test]
async fn pact_false_retrigger_reresolves_simm_from_siml() {
    use epics_base_rs::server::records::bo::BoRecord;

    let db = PvDatabase::new();
    // Sim switch starts NO; SIOL is the simulated target.
    db.add_record("BOH_SW", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("BOH_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut bo = BoRecord::new(1); // VAL=1 + HIGH>0 arms the one-shot reset
    bo.siml = "BOH_SW".to_string();
    bo.siol = "BOH_TGT".to_string();
    bo.high = 100.0;
    // sdly left at the default -1.0; the HIGH timer is the pact=FALSE re-trigger.
    db.add_record("BOH", Box::new(bo)).await.unwrap();

    // Fresh cycle: SIML reads 0 -> SIMM=NO -> not simulated -> body runs and
    // arms the HIGH one-shot (returns Complete, so PACT is NOT held).
    let mut v1 = HashSet::new();
    db.process_record_with_links("BOH", &mut v1, 0)
        .await
        .unwrap();
    let simm = db.get_pv("BOH.SIMM").unwrap();
    assert!(
        matches!(simm, EpicsValue::Short(0)),
        "SIMM resolved NO on the fresh cycle, got {simm:?}"
    );
    assert!(
        !is_processing(&db, "BOH").await,
        "the bo HIGH one-shot returns Complete and does NOT hold PACT"
    );

    // Operator flips the sim switch ON before the HIGH reset fires.
    db.put_pv_no_process("BOH_SW", EpicsValue::Double(1.0))
        .await
        .unwrap();

    // HIGH reset re-process: a pact=FALSE re-trigger. C re-resolves SIMM
    // (recGblGetSimm runs because !pact); the port must too — the gate is
    // `!pact_held`, not `!is_continuation`.
    let mut v2 = HashSet::new();
    db.process_record_continuation("BOH", &mut v2, 0)
        .await
        .unwrap();
    let simm = db.get_pv("BOH.SIMM").unwrap();
    assert!(
        matches!(simm, EpicsValue::Short(1)),
        "pact=FALSE HIGH re-trigger re-resolves SIMM from SIML (matches C recGblGetSimm on !pact), got {simm:?}"
    );
}

/// Output record: `SDLY >= 0` defers the SIOL write (`aoRecord.c::writeValue`
/// shares the same async branch as the input read). On the fresh cycle the
/// SIOL target keeps its value and PACT is held; the continuation writes VAL to
/// the target and clears PACT.
#[epics_macros_rs::epics_test]
async fn sdly_async_defers_output_sim_write_to_continuation() {
    let db = PvDatabase::new();
    db.add_record("SDLYO_SW", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db.add_record("SDLYO_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    // ao output: VAL=77 is written to SIOL when simulated.
    let mut ao = AoRecord::new(77.0);
    ao.siml = "SDLYO_SW".to_string();
    ao.siol = "SDLYO_TGT".to_string();
    ao.sdly = 100.0;
    db.add_record("SDLYO_AO", Box::new(ao)).await.unwrap();

    // Fresh (delaying) cycle: PACT held, SIOL write deferred — target untouched.
    let mut v1 = HashSet::new();
    db.process_record_with_links("SDLYO_AO", &mut v1, 0)
        .await
        .unwrap();

    assert!(
        is_processing(&db, "SDLYO_AO").await,
        "PACT held across the SDLY delay on the output record"
    );
    let tgt = db.get_pv("SDLYO_TGT").unwrap();
    assert!(
        matches!(tgt, EpicsValue::Double(v) if v == 0.0),
        "SIOL target untouched on the delaying cycle (sim write deferred), got {tgt:?}"
    );

    // Continuation: VAL written to the SIOL target, PACT cleared.
    let mut v2 = HashSet::new();
    db.process_record_continuation("SDLYO_AO", &mut v2, 0)
        .await
        .unwrap();

    let tgt = db.get_pv("SDLYO_TGT").unwrap();
    assert!(
        matches!(tgt, EpicsValue::Double(v) if (v - 77.0).abs() < 1e-10),
        "SIOL target written on the continuation, got {tgt:?}"
    );
    assert!(
        !is_processing(&db, "SDLYO_AO").await,
        "PACT cleared on the continuation"
    );
}
