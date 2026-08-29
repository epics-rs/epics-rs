//! A `DBF_MENU` field must read the same to the framework whichever numeric
//! `EpicsValue` the record type carries it in.
//!
//! `mcaRecord.dbd:391` declares SIMS exactly as every base record does:
//!
//! ```c
//! /* mcaRecord.dbd:386-395 */
//! field(SIMM,DBF_MENU) {
//!     prompt("Simulation Mode")
//!     ...
//!     menu(menuYesNo)
//! }
//! field(SIMS,DBF_MENU) {
//!     prompt("Simulation Mode Severity")
//!     ...
//!     menu(menuAlarmSevr)
//! }
//! ```
//!
//! and C reads it as a plain `epicsEnum16` off the record struct
//! (`mcaRecord.c:1129`, `recGblSetSevr(pmca,SIMM_ALARM,pmca->sims)`), so the
//! severity the database configured is the severity C raises.
//!
//! The port's record types disagree on the carrier — every `epics-base-rs`
//! record answers `EpicsValue::Short` (`records/ai.rs:310`) and `mca` answers
//! `EpicsValue::Enum` (`mca-rs/src/record/mod.rs:679`) — and the framework read
//! in `check_simulation_mode` pattern-matched `Short`, so `mca`'s SIMS resolved
//! to the `unwrap_or(0)` default. A simulated mca therefore raised SIMM_ALARM
//! at NO_ALARM whatever the database asked for, which raises nothing at all:
//! the operator got no indication that the record was running off its
//! simulation link. `EpicsValue::to_menu_index` is the carrier-agnostic read.
//!
//! Boundaries: carrier {Enum (mca), Short (longin)} x index {NO_ALARM = the
//! value the defect always produced, MINOR, INVALID}.
//!
//! C read against `mca` at `687d563` and `epics-base` at `8f5015b66`.

// RTEMS-EXEC-MODEL-ALLOW(4): checked, not waived — all 4 ran and passed
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

/// Every record reads its simulation value from a SIOL that resolves, so no
/// LINK_ALARM is ever in play and the only alarm on show is the SIMM one.
const DB: &str = r#"
record(waveform, "SIM:SPEC") {
    field(FTVL, "LONG")
    field(NELM, "8")
}
record(ai, "SIM:SCALAR") {
}
record(mca, "MCA:INVALID") {
    field(NMAX, "8")
    field(NUSE, "8")
    field(FTVL, "LONG")
    field(SIOL, "SIM:SPEC")
    field(SIMS, "INVALID")
}
record(mca, "MCA:MINOR") {
    field(NMAX, "8")
    field(NUSE, "8")
    field(FTVL, "LONG")
    field(SIOL, "SIM:SPEC")
    field(SIMS, "MINOR")
}
record(mca, "MCA:NONE") {
    field(NMAX, "8")
    field(NUSE, "8")
    field(FTVL, "LONG")
    field(SIOL, "SIM:SPEC")
}
record(longin, "LI:INVALID") {
    field(SIOL, "SIM:SCALAR")
    field(SIMS, "INVALID")
}
"#;

async fn load(db: &PvDatabase) {
    mca_rs::register_mca_record_type();
    for def in parse_db(DB, &HashMap::new()).unwrap() {
        let mut rec = create_record(&def.record_type).unwrap();
        let mut common = Vec::new();
        apply_fields(&mut rec, &def.fields, &mut common).unwrap();
        db.add_loaded_record(&def.name, rec, RecordLoad::from_common_fields(common))
            .await
            .unwrap();
    }
    db.put_pv(
        "SIM:SPEC",
        EpicsValue::LongArray(vec![1, 2, 3, 4, 5, 6, 7, 8]),
    )
    .await
    .unwrap();
}

/// Put SIMM = YES and process once.
async fn simulate(db: &PvDatabase, name: &str) {
    db.put_pv(&format!("{name}.SIMM"), EpicsValue::Short(1))
        .await
        .unwrap();
    let mut v = HashSet::new();
    db.process_record_with_links(name, &mut v, 0).await.unwrap();
}

fn alarm_of(db: &PvDatabase, name: &str) -> (u16, AlarmSeverity) {
    let rec = db.get_record(name).expect("record exists");
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr)
}

/// The `Enum` carrier at the top severity. Before the fix this read
/// `AlarmSeverity::NoAlarm` and `recGblSetSevr` raised nothing.
#[tokio::test]
async fn an_mca_reaches_the_simm_alarm_at_invalid() {
    let db = PvDatabase::new();
    load(&db).await;

    simulate(&db, "MCA:INVALID").await;

    assert_eq!(
        alarm_of(&db, "MCA:INVALID"),
        (alarm_status::SIMM_ALARM, AlarmSeverity::Invalid),
        "mca answers SIMS as EpicsValue::Enum; the framework must still read \
         the menu index the database set"
    );
}

/// A second index off the same carrier, so the assertion above cannot be met
/// by anything that merely treats a non-`Short` SIMS as "some alarm".
#[tokio::test]
async fn an_mca_reaches_the_simm_alarm_at_minor() {
    let db = PvDatabase::new();
    load(&db).await;

    simulate(&db, "MCA:MINOR").await;

    assert_eq!(
        alarm_of(&db, "MCA:MINOR"),
        (alarm_status::SIMM_ALARM, AlarmSeverity::Minor),
        "SIMS = MINOR is menuAlarmSevr index 1, not index 3"
    );
}

/// The zero boundary — the value the defect produced unconditionally. It must
/// still raise nothing, so the fix is not simply "always alarm".
#[tokio::test]
async fn an_mca_without_sims_raises_no_simm_alarm() {
    let db = PvDatabase::new();
    load(&db).await;

    simulate(&db, "MCA:NONE").await;

    assert_eq!(
        alarm_of(&db, "MCA:NONE"),
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm),
        "recGblSetSevr with NO_ALARM raises nothing (recGbl.c, strict-greater)"
    );
}

/// The `Short` carrier control: the path that always worked must be untouched.
#[tokio::test]
async fn a_longin_still_reaches_the_simm_alarm_at_invalid() {
    let db = PvDatabase::new();
    load(&db).await;

    simulate(&db, "LI:INVALID").await;

    assert_eq!(
        alarm_of(&db, "LI:INVALID"),
        (alarm_status::SIMM_ALARM, AlarmSeverity::Invalid),
        "base records answer SIMS as EpicsValue::Short"
    );
}
