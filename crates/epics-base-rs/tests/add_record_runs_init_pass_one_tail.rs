//! The creation sink owes every record the REST of C's `init_record` pass 1.
//!
//! C reaches `init_record` through `iterateRecords` (`iocInit.c:562-586`), so
//! there is no record in the database that pass 1 did not touch, whatever
//! created it. Two pieces of that pass live outside the record itself:
//! `recGblInitSimm` with its paired `recGblInitConstantLink(&siol, …, &sval)`
//! (`recGbl.c:438-444`, called from e.g. `aiRecord.c:101` and
//! `mbboDirectRecord.c:117`), and `wdogInit` (`histogramRecord.c:168`).
//!
//! The port had them on the loader CALLERS instead of in
//! `PvDatabase::add_loaded_record`, the documented creation sink — so a record
//! made by `IocApplication::with_record` or by iocsh `dbCreateRecord`, both of
//! which call `add_record` directly, got neither: no SIML seed, no SDEL
//! watchdog. The same record loaded from a `.db` behaved correctly, which is
//! what makes this a sink defect and not a record defect.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::histogram::HistogramRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

fn field(db: &PvDatabase, rec: &str, f: &str) -> Option<f64> {
    db.get_record(rec)?.read().record.get_field(f)?.to_f64()
}

/// An `ai` carrying a constant SIML/SIOL, created the programmatic way.
async fn simulated_ai(db: &PvDatabase, name: &str, siml: &str) {
    let mut ai = AiRecord::new(0.0);
    ai.put_field("SIML", EpicsValue::String(siml.into()))
        .unwrap();
    ai.put_field("SIOL", EpicsValue::String("7".into()))
        .unwrap();
    db.add_record(name, Box::new(ai)).await.unwrap();
}

/// BOUNDARY: a constant SIML. `recGblInitSimm` loads it into SIMM, and the
/// paired constant-link load puts SIOL's value in SVAL — so the record is in
/// simulation from creation and reads 7 instead of its INP.
#[epics_macros_rs::epics_test]
async fn the_sink_seeds_simm_from_a_constant_siml() {
    let db = PvDatabase::new();
    simulated_ai(&db, "A", "1").await;

    assert_eq!(field(&db, "A", "SIMM"), Some(1.0), "SIML=1 -> SIMM YES");
    assert_eq!(field(&db, "A", "SVAL"), Some(7.0), "constant SIOL -> SVAL");
}

/// BOUNDARY: no SIML. Nothing loads, so the record stays out of simulation —
/// C's `if (dbLinkIsConstant(psiml))` simply does not fire.
#[epics_macros_rs::epics_test]
async fn a_record_with_no_siml_stays_out_of_simulation() {
    let db = PvDatabase::new();
    simulated_ai(&db, "A", "").await;

    assert_eq!(field(&db, "A", "SIMM"), Some(0.0), "SIMM NO");
}

/// BOUNDARY: the `.db` path, which had the seed from its loader caller. Moving
/// the call into the sink must not lose it.
#[epics_macros_rs::epics_test]
async fn the_db_path_still_seeds_simm() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(ai, "A") { field(SIML, "1") field(SIOL, "7") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    assert_eq!(field(&db, "A", "SIMM"), Some(1.0));
    assert_eq!(field(&db, "A", "SVAL"), Some(7.0));
}

/// BOUNDARY: `wdogInit` at creation. The record is born with `SDEL > 0`, so
/// nothing ever puts SDEL and the SPC_RESET re-arm never runs — the only thing
/// that can arm this watchdog is pass 1. MDEL is far above the count taken, so
/// the process-time `monitor()` post is suppressed and only the watchdog can
/// publish.
#[epics_macros_rs::epics_test]
async fn the_sink_arms_the_sdel_watchdog() {
    let db = PvDatabase::new();
    let mut hist = HistogramRecord::new(2, 0.0, 10.0);
    hist.mdel = 1000;
    hist.put_field("SDEL", EpicsValue::Double(0.05)).unwrap();
    db.add_record("H", Box::new(hist)).await.unwrap();

    let rec = db.get_record("H").unwrap();
    let mut val_rx = rec
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Long, EventMask::VALUE.bits())
        .expect("VAL subscription accepted");

    db.put_record_field_from_ca("H", "SGNL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    while val_rx.try_recv().is_ok() {}

    let event = epics_base_rs::runtime::task::timeout(Duration::from_secs(2), val_rx.recv())
        .await
        .expect("a record born with SDEL > 0 must have its watchdog armed at creation")
        .expect("subscription alive");
    match &event.snapshot.value {
        EpicsValue::ULongArray(v) => assert_eq!(v, &vec![1, 0]),
        other => panic!("VAL must be ULongArray (C DBF_ULONG), got {other:?}"),
    }
}
