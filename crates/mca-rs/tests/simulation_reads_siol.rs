//! A simulated `mca` reads SIOL IN; it never writes VAL out to it.
//!
//! `mcaRecord.c:1097` `readValue`:
//!
//! ```c
//! if (pmca->simm == menuYesNoYES) {
//!     nRequest = pmca->nmax;
//!     status = dbGetLink(&(pmca->siol), pmca->ftvl, pmca->bptr, NULL, &nRequest);
//!     /* nord set only for db links: needed for old db_access */
//!     if (pmca->siol.type == DB_LINK) pmca->nord = nRequest;
//!     if (status == 0) { pmca->udf = FALSE; }
//! } else {
//!     status = -1;
//!     recGblSetSevr(pmca, SOFT_ALARM, INVALID_ALARM);
//!     return(status);
//! }
//! recGblSetSevr(pmca, SIMM_ALARM, pmca->sims);
//! ```
//!
//! `mca` was absent from the framework's `is_input` record-type list, so a
//! simulated cycle took the OUTPUT branch (`RedirectOutputToSiol`) and would
//! have written the record's own spectrum out to SIOL. The SIML/SIOL reads
//! additionally went through `Record::get_field`, which answers `None` for a
//! record that leaves its link fields to the framework, as `mca` does.

// RTEMS-EXEC-MODEL-ALLOW(5): checked, not waived — all 5 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p mca-rs
// --all-features`, 62/62). mca-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.

use std::collections::{HashMap, HashSet};

use epics_base_rs::server::database::{PvDatabase, RecordLoad};
use epics_base_rs::server::db_loader::{apply_fields, create_record, parse_db};
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

/// `NMAX` = 8 with a same-width simulation source.
const DB: &str = r#"
record(waveform, "SIM:SPEC") {
    field(FTVL, "LONG")
    field(NELM, "8")
}
record(mca, "MCA1") {
    field(NMAX, "8")
    field(NUSE, "8")
    field(FTVL, "LONG")
    field(SIOL, "SIM:SPEC")
}
"#;

/// `NMAX` = 4 against an 8-element source — C requests `nRequest = pmca->nmax`.
const DB_NARROW: &str = r#"
record(waveform, "SIM:WIDE") {
    field(FTVL, "LONG")
    field(NELM, "8")
}
record(mca, "MCA2") {
    field(NMAX, "4")
    field(NUSE, "4")
    field(FTVL, "LONG")
    field(SIOL, "SIM:WIDE")
}
"#;

const COUNTS: [i32; 8] = [11, 22, 33, 44, 55, 66, 77, 88];

async fn load(db: &PvDatabase, text: &str) {
    mca_rs::register_mca_record_type();
    for def in parse_db(text, &HashMap::new()).unwrap() {
        let mut rec = create_record(&def.record_type).unwrap();
        let mut common = Vec::new();
        apply_fields(&mut rec, &def.fields, &mut common).unwrap();
        db.add_loaded_record(&def.name, rec, RecordLoad::from_common_fields(common))
            .await
            .unwrap();
    }
}

async fn process(db: &PvDatabase, name: &str) {
    let mut v = HashSet::new();
    db.process_record_with_links(name, &mut v, 0).await.unwrap();
}

async fn alarm_of(db: &PvDatabase, name: &str) -> (u16, AlarmSeverity) {
    let rec = db.get_record(name).expect("record exists");
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr)
}

async fn udf_of(db: &PvDatabase, name: &str) -> u8 {
    let rec = db.get_record(name).expect("record exists");
    let inst = rec.read();
    inst.common.udf
}

/// The destructive half: SIOL is an INPUT for `mca`, so the simulated cycle
/// must leave the source record exactly as it found it. Under the OUTPUT
/// branch the record's own all-zero spectrum went out to `SIM:SPEC`.
#[tokio::test]
async fn a_simulated_mca_does_not_write_its_spectrum_onto_siol() {
    let db = PvDatabase::new();
    load(&db, DB).await;
    db.put_pv("SIM:SPEC", EpicsValue::LongArray(COUNTS.to_vec()))
        .await
        .unwrap();
    db.put_pv("MCA1.SIMM", EpicsValue::Short(1)).await.unwrap();

    process(&db, "MCA1").await;

    assert_eq!(
        db.get_pv("SIM:SPEC").unwrap(),
        EpicsValue::LongArray(COUNTS.to_vec()),
        "the simulation source was overwritten"
    );
}

/// The corrective half: the spectrum, `NORD` and `UDF` all come from SIOL.
#[tokio::test]
async fn a_simulated_mca_reads_the_spectrum_in_from_siol() {
    let db = PvDatabase::new();
    load(&db, DB).await;
    db.put_pv("SIM:SPEC", EpicsValue::LongArray(COUNTS.to_vec()))
        .await
        .unwrap();
    db.put_pv("MCA1.SIMM", EpicsValue::Short(1)).await.unwrap();

    process(&db, "MCA1").await;

    assert_eq!(
        db.get_pv("MCA1").unwrap(),
        EpicsValue::LongArray(COUNTS.to_vec())
    );
    // `if (pmca->siol.type == DB_LINK) pmca->nord = nRequest;` — SIOL names a
    // local record here, so the guard is taken.
    assert_eq!(db.get_pv("MCA1.NORD").unwrap(), EpicsValue::Long(8));
    assert_eq!(udf_of(&db, "MCA1").await, 0, "status 0 must clear UDF");
}

/// C asks for `nRequest = pmca->nmax` elements, so a wider source is truncated
/// to the record's own depth rather than refused or landed whole.
#[tokio::test]
async fn a_simulated_mca_reads_at_most_nmax_elements() {
    let db = PvDatabase::new();
    load(&db, DB_NARROW).await;
    db.put_pv("SIM:WIDE", EpicsValue::LongArray(COUNTS.to_vec()))
        .await
        .unwrap();
    db.put_pv("MCA2.SIMM", EpicsValue::Short(1)).await.unwrap();

    process(&db, "MCA2").await;

    assert_eq!(
        db.get_pv("MCA2").unwrap(),
        EpicsValue::LongArray(COUNTS[..4].to_vec())
    );
    assert_eq!(db.get_pv("MCA2.NORD").unwrap(), EpicsValue::Long(4));
}

/// The control boundary: `SIMM = NO` runs the real device read, so SIOL is
/// neither read nor written and the spectrum stays as device support left it.
#[tokio::test]
async fn an_unsimulated_mca_ignores_siol_entirely() {
    let db = PvDatabase::new();
    load(&db, DB).await;
    db.put_pv("SIM:SPEC", EpicsValue::LongArray(COUNTS.to_vec()))
        .await
        .unwrap();

    process(&db, "MCA1").await;

    assert_eq!(
        db.get_pv("SIM:SPEC").unwrap(),
        EpicsValue::LongArray(COUNTS.to_vec())
    );
    assert_eq!(db.get_pv("MCA1.NORD").unwrap(), EpicsValue::Long(0));
}

/// `mcaRecord.dbd:386-390` gives SIMM `menu(menuYesNo)` — two choices — so C's
/// `else` arm catches every value that is neither NO nor YES: `status = -1`
/// plus `recGblSetSevr(pmca, SOFT_ALARM, INVALID_ALARM)`, with no simulated
/// value landed and no SIMM_ALARM (the `return` precedes it).
#[tokio::test]
async fn an_illegal_simm_alarms_soft_invalid_and_lands_nothing() {
    let db = PvDatabase::new();
    load(&db, DB).await;
    db.put_pv("SIM:SPEC", EpicsValue::LongArray(COUNTS.to_vec()))
        .await
        .unwrap();
    db.put_pv("MCA1.SIMM", EpicsValue::Short(2)).await.unwrap();

    process(&db, "MCA1").await;

    assert_eq!(
        alarm_of(&db, "MCA1").await,
        (alarm_status::SOFT_ALARM, AlarmSeverity::Invalid)
    );
    assert_eq!(db.get_pv("MCA1.NORD").unwrap(), EpicsValue::Long(0));
    assert_eq!(
        db.get_pv("SIM:SPEC").unwrap(),
        EpicsValue::LongArray(COUNTS.to_vec())
    );
}
