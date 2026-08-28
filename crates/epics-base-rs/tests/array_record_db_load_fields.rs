//! R15-76: `field(SIML/SIMM/SIOL/SIMS/SDLY/MPST/APST, ...)` on an
//! `aai`/`aao`/`waveform` must stick at `.db` load.
//!
//! C declares all seven as ordinary fields on all three array records
//! (`aaiRecord.dbd.pod:374-433`, `aaoRecord.dbd.pod:407-466`,
//! `waveformRecord.dbd.pod:475-534`). The port's `.db` loader gates
//! `field(...)` on `Record::field_list` membership — a name missing from the
//! list is routed to the common fields, rejected there with `FieldNotFound`,
//! logged to stderr and skipped. None of the seven was in the array records'
//! field lists, so the ONLY way real IOCs configure simulation and On-Change
//! posting was silently dropped at load (a runtime `caput` worked, which is
//! why the existing sim tests were green over a dead loader path).
//!
//! Boundary-per-case: one case per kind (aai / aao / waveform), each asserting
//! every field landed in the record's own storage, plus SDLY reaching the
//! framework's async-simulation delay (`processing.rs` reads it through
//! `get_field("SDLY")`, defaulting an absent field to -1.0 = synchronous).

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// menu(waveformPOST) index for "On Change" (`waveformRecord.dbd.pod:20-23`).
const POST_ONCHANGE: i16 = 1;
/// menu(menuYesNo) index for YES.
const SIMM_YES: i16 = 1;
/// menu(menuAlarmSevr) index for MINOR.
const SEVR_MINOR: i16 = 1;

const DB: &str = r#"
record(ai, "SIM:SW") {
    field(VAL, "0")
}
record(waveform, "ARR:SRC") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
}
record(aai, "ARR:AAI") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
    field(SIML, "SIM:SW")
    field(SIMM, "YES")
    field(SIOL, "ARR:SRC")
    field(SIMS, "MINOR")
    field(SDLY, "0.25")
    field(MPST, "On Change")
    field(APST, "On Change")
}
record(aao, "ARR:AAO") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
    field(SIML, "SIM:SW")
    field(SIMM, "YES")
    field(SIOL, "ARR:SRC")
    field(SIMS, "MINOR")
    field(SDLY, "0.25")
    field(MPST, "On Change")
    field(APST, "On Change")
}
record(waveform, "ARR:WF") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
    field(SIML, "SIM:SW")
    field(SIMM, "YES")
    field(SIOL, "ARR:SRC")
    field(SIMS, "MINOR")
    field(SDLY, "0.25")
    field(MPST, "On Change")
    field(APST, "On Change")
}
"#;

async fn field(db: &PvDatabase, pv: &str) -> EpicsValue {
    db.get_pv(pv)
        .unwrap_or_else(|e| panic!("{pv} not readable: {e}"))
}

async fn assert_sim_block_loaded(db: &PvDatabase, rec: &str) {
    // A link field READS as C `dbGetString` renders it, so the defaulted
    // modifiers come back: measured on `softIoc` R7.0.10-146, `caget
    // ARR:AAI.SIML` on this very database answers `SIM:SW NPP NMS` on all
    // three record types and for both link classes.
    assert_eq!(
        field(db, &format!("{rec}.SIML")).await,
        EpicsValue::String("SIM:SW NPP NMS".into()),
        "{rec}: field(SIML) dropped at load"
    );
    assert_eq!(
        field(db, &format!("{rec}.SIOL")).await,
        EpicsValue::String("ARR:SRC NPP NMS".into()),
        "{rec}: field(SIOL) dropped at load"
    );
    assert_eq!(
        field(db, &format!("{rec}.SIMM")).await.to_f64(),
        Some(SIMM_YES as f64),
        "{rec}: field(SIMM,\"YES\") dropped at load"
    );
    assert_eq!(
        field(db, &format!("{rec}.SIMS")).await.to_f64(),
        Some(SEVR_MINOR as f64),
        "{rec}: field(SIMS,\"MINOR\") dropped at load"
    );
    assert_eq!(
        field(db, &format!("{rec}.SDLY")).await,
        EpicsValue::Double(0.25),
        "{rec}: field(SDLY) dropped at load (no storage before R15-76)"
    );
    assert_eq!(
        field(db, &format!("{rec}.MPST")).await.to_f64(),
        Some(POST_ONCHANGE as f64),
        "{rec}: field(MPST,\"On Change\") dropped at load"
    );
    assert_eq!(
        field(db, &format!("{rec}.APST")).await.to_f64(),
        Some(POST_ONCHANGE as f64),
        "{rec}: field(APST,\"On Change\") dropped at load"
    );
}

#[epics_macros_rs::epics_test]
async fn aai_db_load_carries_sim_and_post_fields() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    assert_sim_block_loaded(&db, "ARR:AAI").await;
}

#[epics_macros_rs::epics_test]
async fn aao_db_load_carries_sim_and_post_fields() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    assert_sim_block_loaded(&db, "ARR:AAO").await;
}

#[epics_macros_rs::epics_test]
async fn waveform_db_load_carries_sim_and_post_fields() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    assert_sim_block_loaded(&db, "ARR:WF").await;
}

/// The SDLY the `.db` loaded must reach the framework's async-simulation
/// branch: with `SIMM=YES` and `SDLY >= 0`, C's `readValue` arms the delayed
/// callback and holds PACT (`aaiRecord.c:359-368`), so the SIOL value lands
/// only AFTER the delay — not within the first process call.
#[epics_macros_rs::epics_test]
async fn loaded_sdly_defers_the_simulated_aai_read() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    // Sentinel array in the SIOL source.
    db.put_pv("ARR:SRC", EpicsValue::DoubleArray(vec![10.0, 20.0, 30.0]))
        .await
        .unwrap();
    // C `recGblGetSimm` re-reads SIMM from SIML on every non-PACT entry, so the
    // switch record — not the loaded `field(SIMM,"YES")` — decides the mode at
    // process time. Drive it YES.
    db.put_pv("SIM:SW", EpicsValue::Double(1.0)).await.unwrap();

    let mut visited = std::collections::HashSet::new();
    db.process_record_with_links("ARR:AAI", &mut visited, 0)
        .await
        .unwrap();

    // SDLY=0.25 -> the read is deferred; VAL is still the empty (NORD=0) buffer.
    assert_eq!(
        field(&db, "ARR:AAI").await,
        EpicsValue::DoubleArray(vec![]),
        "SDLY >= 0 must defer the simulated SIOL read (PACT held)"
    );

    // After the delay the deferred continuation lands the SIOL array.
    for _ in 0..200 {
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(10)).await;
        if field(&db, "ARR:AAI").await == EpicsValue::DoubleArray(vec![10.0, 20.0, 30.0]) {
            return;
        }
    }
    panic!(
        "deferred SIOL read never landed (last {:?})",
        field(&db, "ARR:AAI").await
    );
}
