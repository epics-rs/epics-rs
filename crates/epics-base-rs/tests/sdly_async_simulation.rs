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
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

async fn is_processing(db: &PvDatabase, name: &str) -> bool {
    let rec = db.get_record(name).await.unwrap();
    let inst = rec.read().await;
    inst.is_processing()
}

/// Input record: `SDLY >= 0` defers the SIOL read (and the SIMM-alarm tail) to
/// the continuation while holding PACT. On the fresh cycle VAL is untouched and
/// no alarm is raised; the continuation reads SIOL into VAL, raises SIMM_ALARM,
/// and clears PACT.
#[tokio::test]
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
    let val = db.get_pv("SDLY_AI").await.unwrap();
    assert!(
        matches!(val, EpicsValue::Double(v) if v == 0.0),
        "VAL untouched on the delaying cycle (sim read deferred), got {val:?}"
    );
    let sevr = db.get_pv("SDLY_AI.SEVR").await.unwrap();
    assert!(
        matches!(sevr, EpicsValue::Short(0)),
        "SIMM_ALARM not raised on the delaying cycle (alarm tail deferred), got {sevr:?}"
    );

    // Continuation: sync SIOL read -> VAL=42, SIMM_ALARM raised, PACT cleared.
    let mut v2 = HashSet::new();
    db.process_record_continuation("SDLY_AI", &mut v2, 0)
        .await
        .unwrap();

    let val = db.get_pv("SDLY_AI").await.unwrap();
    assert!(
        matches!(val, EpicsValue::Double(v) if (v - 42.0).abs() < 1e-10),
        "VAL read from SIOL on the continuation, got {val:?}"
    );
    assert!(
        !is_processing(&db, "SDLY_AI").await,
        "PACT cleared on the continuation (C readValue sets pact=FALSE)"
    );
    let sevr = db.get_pv("SDLY_AI.SEVR").await.unwrap();
    assert!(
        matches!(sevr, EpicsValue::Short(1)),
        "SIMM_ALARM (MINOR) raised on the continuation, got {sevr:?}"
    );
}

/// Regression guard: the default `SDLY = -1.0` reads synchronously on the single
/// cycle (`pact || sdly < 0` takes the sync branch) — the async defer is gated
/// strictly on `sdly >= 0`, so VAL is set immediately and PACT is never held.
#[tokio::test]
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

    let val = db.get_pv("SDLYN_AI").await.unwrap();
    assert!(
        matches!(val, EpicsValue::Double(v) if (v - 42.0).abs() < 1e-10),
        "VAL read synchronously on the single cycle (sdly < 0), got {val:?}"
    );
    assert!(
        !is_processing(&db, "SDLYN_AI").await,
        "no PACT hold for sdly < 0 (synchronous sim read)"
    );
}

/// Output record: `SDLY >= 0` defers the SIOL write (`aoRecord.c::writeValue`
/// shares the same async branch as the input read). On the fresh cycle the
/// SIOL target keeps its value and PACT is held; the continuation writes VAL to
/// the target and clears PACT.
#[tokio::test]
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
    let tgt = db.get_pv("SDLYO_TGT").await.unwrap();
    assert!(
        matches!(tgt, EpicsValue::Double(v) if v == 0.0),
        "SIOL target untouched on the delaying cycle (sim write deferred), got {tgt:?}"
    );

    // Continuation: VAL written to the SIOL target, PACT cleared.
    let mut v2 = HashSet::new();
    db.process_record_continuation("SDLYO_AO", &mut v2, 0)
        .await
        .unwrap();

    let tgt = db.get_pv("SDLYO_TGT").await.unwrap();
    assert!(
        matches!(tgt, EpicsValue::Double(v) if (v - 77.0).abs() < 1e-10),
        "SIOL target written on the continuation, got {tgt:?}"
    );
    assert!(
        !is_processing(&db, "SDLYO_AO").await,
        "PACT cleared on the continuation"
    );
}
