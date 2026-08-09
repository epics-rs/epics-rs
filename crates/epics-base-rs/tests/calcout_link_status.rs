//! Boundary tests for the `calcout` link-connection-status diagnostics
//! (`INAV`..`INUV`/`OUTV`, `menu(calcoutINAV)`), mirroring C
//! `calcoutRecord.c::init_record` (calcoutRecord.c:160-189), which classifies
//! every `INPA`..`INPU` input link and the `OUT` link.
//!
//! The classification rule itself (`classify_link`) is shared with `sseq`
//! and covered there; these pin the calcout-specific wiring:
//!
//!   * a local DB input link → `LOC`, an empty input link → `CON`, posted at
//!     record init (`set_async_context`),
//!   * the OUT link — a *common* field, not a calcout field, invisible to the
//!     record at `set_async_context` (which receives only name+handle). C
//!     `init_record` (calcoutRecord.c:160-189) still classifies it at load,
//!     so the framework hands the resolved common fields to the record after
//!     the common-field/init_record passes via `Record::init_links`
//!     (`ioc_builder`/`iocsh` load path): a passive never-processed calcout
//!     loaded through a `.db` already shows `OUTV = LOC` for a local OUT link.
//!     On the lower-level `PvDatabase::add_record` path (no loader), `OUTV`
//!     holds its `CON` default until the first process re-points it through
//!     `check_alarms`.

use std::collections::HashSet;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::types::EpicsValue;

// menu(calcoutINAV) indices (calcoutRecord.dbd.pod:45-50).
const LOC: i16 = 2;
const CON: i16 = 3;

/// Poll a DBF_MENU-served-as-DBR_ENUM status field until its numeric value
/// equals `want`.
async fn poll_status(db: &PvDatabase, pv: &str, want: i16, label: &str) {
    for _ in 0..400 {
        if let Ok(v) = db.get_pv(pv)
            && v.to_f64().map(|f| f as i16) == Some(want)
        {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "{label}: {pv} did not reach {want} before timeout (last {:?})",
        db.get_pv(pv)
    );
}

async fn read_status(db: &PvDatabase, pv: &str) -> i16 {
    db.get_pv(pv)
        .ok()
        .and_then(|v| v.to_f64())
        .map(|f| f as i16)
        .unwrap_or_else(|| panic!("{pv} not readable as a number"))
}

#[epics_macros_rs::epics_test]
async fn calcout_input_link_status_loc_vs_con() {
    let db = PvDatabase::new();
    // A local target so a DB input link to it resolves to LOC.
    db.add_record("CALC_LS_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut co = CalcoutRecord::default();
    // INPA → local DB link (LOC); INPB left empty (CON).
    co.put_field("INPA", EpicsValue::String("CALC_LS_TGT.VAL".into()))
        .unwrap();
    db.add_record("CALC_LS", Box::new(co)).await.unwrap();

    // Init refresh (set_async_context) classifies the input links.
    poll_status(&db, "CALC_LS.INAV", LOC, "local INPA → LOC").await;
    assert_eq!(
        read_status(&db, "CALC_LS.INBV").await,
        CON,
        "empty INPB → CON"
    );
    // The last input keeps its CON default when unconfigured.
    assert_eq!(
        read_status(&db, "CALC_LS.INUV").await,
        CON,
        "empty INPU → CON"
    );
}

#[epics_macros_rs::epics_test]
async fn calcout_input_status_reclassifies_on_special() {
    let db = PvDatabase::new();
    db.add_record("CALC_SP_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("CALC_SP", Box::new(CalcoutRecord::default()))
        .await
        .unwrap();

    // Empty at init → CON.
    poll_status(&db, "CALC_SP.INCV", CON, "empty INPC → CON").await;

    // A client put to the INP link re-runs checkLinks via special().
    db.put_record_field_from_ca_no_notify(
        "CALC_SP",
        "INPC",
        EpicsValue::String("CALC_SP_TGT.VAL".into()),
    )
    .await
    .unwrap();
    poll_status(&db, "CALC_SP.INCV", LOC, "INPC repointed to local → LOC").await;
}

#[epics_macros_rs::epics_test]
async fn calcout_outv_classifies_via_process() {
    let db = PvDatabase::new();
    db.add_record("CALC_OUT_TGT", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("CALC_OUT", Box::new(CalcoutRecord::default()))
        .await
        .unwrap();

    // The OUT link is a common field, invisible to the record at init, so
    // OUTV holds its CON default until the first process.
    poll_status(&db, "CALC_OUT.OUTV", CON, "OUTV CON before process").await;

    // Configure a local OUT link, then process: check_alarms mirrors
    // common.out into the record and re-classifies OUTV → LOC.
    db.put_record_field_from_ca_no_notify(
        "CALC_OUT",
        "OUT",
        EpicsValue::String("CALC_OUT_TGT".into()),
    )
    .await
    .unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("CALC_OUT", &mut visited, 0)
        .await
        .unwrap();

    poll_status(
        &db,
        "CALC_OUT.OUTV",
        LOC,
        "local OUT link → LOC after process",
    )
    .await;
}

/// C `calcoutRecord.c::init_record` (calcoutRecord.c:160-189) classifies the
/// OUT link at load, before any scan. The OUT link is a *common* field,
/// invisible to the record at `set_async_context`, so the framework hands the
/// resolved common fields to the record via `Record::init_links` after the
/// common-field/init_record passes on the `.db` load path. A passive,
/// never-processed calcout loaded with a local OUT link must therefore already
/// report `OUTV = LOC`; one with an empty OUT link keeps its `CON` default.
#[epics_macros_rs::epics_test]
async fn calcout_outv_classifies_at_init_via_loader() {
    let db_content = r#"
record(ao, "INIT_OUT_TGT") {
    field(VAL, "0.0")
}
record(calcout, "CO_OUT_LOC") {
    field(OUT, "INIT_OUT_TGT")
}
record(calcout, "CO_OUT_EMPTY") {
    field(VAL, "0.0")
}
"#;
    let (db, _) = IocBuilder::new()
        .db_string(db_content, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    // No record is processed: `OUTV` is set purely by the load-time
    // `init_links` classification.  A local OUT link → LOC.
    poll_status(&db, "CO_OUT_LOC.OUTV", LOC, "local OUT at init → LOC").await;
    // An unconfigured OUT link keeps its CON default.
    assert_eq!(
        read_status(&db, "CO_OUT_EMPTY.OUTV").await,
        CON,
        "empty OUT at init → CON"
    );
}
