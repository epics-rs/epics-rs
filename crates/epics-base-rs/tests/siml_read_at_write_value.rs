//! An output reads SIML inside `writeValue` (`aoRecord.c:559`,
//! `busyRecord.c:399`), which `process()` calls after `checkAlarms` and the
//! IVOA test. So a cycle IVOA keeps from the output never reads SIML, and an
//! alarm `checkAlarms` raises wins a tie with the SIML read's MS alarm.
//!
//! softIoc / oracleBusyIoc (EPICS 7.0.10), `SRC1` a longin at 1, `SMIN` a
//! longin at 0 held in LOW/MINOR, one `dbpf <rec>.PROC 1` each:
//!
//! ```text
//! AO:IV   SIMM=NO STAT=HIHI SEVR=INVALID      AO:TIE  SIMM=NO STAT=HIGH SEVR=MINOR
//! BO:IV   SIMM=NO STAT=UDF  SEVR=INVALID      BO:TIE  SIMM=NO STAT=UDF  SEVR=MINOR
//! LO:IV   SIMM=NO STAT=UDF  SEVR=INVALID      LO:TIE  SIMM=NO STAT=UDF  SEVR=MINOR
//! MB:IV   SIMM=NO STAT=UDF  SEVR=INVALID      MB:TIE  SIMM=NO STAT=UDF  SEVR=MINOR
//! MBD:IV  SIMM=NO STAT=UDF  SEVR=INVALID      MBD:TIE SIMM=NO STAT=UDF  SEVR=MINOR
//! SO:IV   SIMM=NO STAT=UDF  SEVR=INVALID      SO:TIE  SIMM=NO STAT=UDF  SEVR=MINOR
//! LSO:IV  SIMM=NO STAT=UDF  SEVR=INVALID      I64:IV  SIMM=NO STAT=UDF  SEVR=INVALID
//! I64:TIE SIMM=NO STAT=UDF  SEVR=MINOR        AAO:DF  SIMM=NO STAT=UDF  SEVR=INVALID
//! BUSY:U  SIMM=NO STAT=UDF  SEVR=INVALID      BUSY:IV SIMM=NO STAT=UDF  SEVR=INVALID
//! ```

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::busy::BusyRecord;

const DB: &str = r#"
record(longin, "SRC1") { field(VAL, "1") }
record(longin, "SMIN") { field(VAL, "0") field(LOW, "1") field(LSV, "MINOR") }
record(stringin, "SABC") { field(VAL, "abc") }
record(ao, "AO:IV") { field(HIHI, "-1") field(HHSV, "INVALID") field(IVOA, "Don't drive outputs") field(SIML, "SRC1") }
record(ao, "AO:TIE") { field(HIGH, "-1") field(HSV, "MINOR") field(SIML, "SMIN NPP MS") }
record(bo, "BO:IV") { field(IVOA, "Don't drive outputs") field(SIML, "SRC1") }
record(bo, "BO:TIE") { field(UDFS, "MINOR") field(SIML, "SMIN NPP MS") }
record(longout, "LO:IV") { field(IVOA, "Don't drive outputs") field(SIML, "SRC1") }
record(longout, "LO:TIE") { field(UDFS, "MINOR") field(SIML, "SMIN NPP MS") }
record(mbbo, "MB:IV") { field(IVOA, "Don't drive outputs") field(SIML, "SRC1") }
record(mbbo, "MB:TIE") { field(UDFS, "MINOR") field(SIML, "SMIN NPP MS") }
record(mbboDirect, "MBD:IV") { field(IVOA, "Don't drive outputs") field(SIML, "SRC1") }
record(mbboDirect, "MBD:TIE") { field(UDFS, "MINOR") field(SIML, "SMIN NPP MS") }
record(stringout, "SO:IV") { field(IVOA, "Don't drive outputs") field(SIML, "SRC1") }
record(stringout, "SO:TIE") { field(UDFS, "MINOR") field(SIML, "SMIN NPP MS") }
record(lso, "LSO:IV") { field(SIZV, "8") field(IVOA, "Don't drive outputs") field(SIML, "SRC1") }
record(int64out, "I64:IV") { field(IVOA, "Don't drive outputs") field(SIML, "SRC1") }
record(int64out, "I64:TIE") { field(UDFS, "MINOR") field(SIML, "SMIN NPP MS") }
record(aao, "AAO:DF") { field(OMSL, "closed_loop") field(DOL, "SABC NPP") field(FTVL, "DOUBLE") field(NELM, "2") field(SIML, "SRC1") }
record(ao, "AO:MS") { field(SIML, "SMIN NPP MS") }
record(busy, "BUSY:U") { field(SIML, "SABC") }
record(busy, "BUSY:IV") { field(IVOA, "Don't drive outputs") field(SIML, "SRC1") }
"#;

async fn build() -> std::sync::Arc<epics_base_rs::server::database::PvDatabase> {
    IocBuilder::new()
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &epics_base_rs::server::database::PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links(rec, &mut visited).await;
}

/// SIMM, STAT, SEVR after one process.
fn published(
    db: &epics_base_rs::server::database::PvDatabase,
    rec: &str,
) -> (u16, u16, AlarmSeverity) {
    let r = db.get_record(rec).unwrap();
    let inst = r.read();
    (
        inst.record.simm_value().map(|v| v as u16).unwrap(),
        inst.common.stat,
        inst.common.sevr,
    )
}

#[epics_macros_rs::epics_test]
async fn ivoa_dont_drive_never_reads_siml() {
    let db = build().await;
    let mut wrong = Vec::new();
    for (rec, stat) in [
        ("AO:IV", alarm_status::HIHI_ALARM),
        ("BO:IV", alarm_status::UDF_ALARM),
        ("LO:IV", alarm_status::UDF_ALARM),
        ("MB:IV", alarm_status::UDF_ALARM),
        ("MBD:IV", alarm_status::UDF_ALARM),
        ("SO:IV", alarm_status::UDF_ALARM),
        ("LSO:IV", alarm_status::UDF_ALARM),
        ("I64:IV", alarm_status::UDF_ALARM),
        ("BUSY:IV", alarm_status::UDF_ALARM),
    ] {
        process(&db, rec).await;
        let got = published(&db, rec);
        if got != (0, stat, AlarmSeverity::Invalid) {
            wrong.push((rec, got));
        }
    }
    assert!(wrong.is_empty(), "{wrong:?}");
}

#[epics_macros_rs::epics_test]
async fn check_alarms_wins_the_tie_with_the_siml_ms_alarm() {
    let db = build().await;
    // The builder runs no PINI pass.
    process(&db, "SMIN").await;
    {
        let r = db.get_record("SMIN").unwrap();
        let i = r.read();
        assert_eq!(
            (i.common.stat, i.common.sevr),
            (alarm_status::LOW_ALARM, AlarmSeverity::Minor),
            "SMIN"
        );
    }
    process(&db, "AO:MS").await;
    assert_eq!(
        published(&db, "AO:MS"),
        (0, alarm_status::LINK_ALARM, AlarmSeverity::Minor),
        "SIML MS raises LINK at the source's severity"
    );
    let mut wrong = Vec::new();
    for (rec, stat) in [
        ("AO:TIE", alarm_status::HIGH_ALARM),
        ("BO:TIE", alarm_status::UDF_ALARM),
        ("LO:TIE", alarm_status::UDF_ALARM),
        ("MB:TIE", alarm_status::UDF_ALARM),
        ("MBD:TIE", alarm_status::UDF_ALARM),
        ("SO:TIE", alarm_status::UDF_ALARM),
        ("I64:TIE", alarm_status::UDF_ALARM),
    ] {
        process(&db, rec).await;
        let got = published(&db, rec);
        if got != (0, stat, AlarmSeverity::Minor) {
            wrong.push((rec, got));
        }
    }
    assert!(wrong.is_empty(), "{wrong:?}");
}

/// aao returns from `process()` when its DOL read fails (`aaoRecord.c:208-209`),
/// before `writeValue`, so SIMM keeps NO and the init UDF alarm stands.
#[epics_macros_rs::epics_test]
async fn aao_failed_dol_never_reads_siml() {
    let db = build().await;
    process(&db, "AAO:DF").await;
    assert_eq!(
        published(&db, "AAO:DF"),
        (0, alarm_status::UDF_ALARM, AlarmSeverity::Invalid)
    );
}

/// busy's `checkAlarms` raises UDF before `writeValue`'s failed SIML read
/// raises LINK at the same severity.
#[epics_macros_rs::epics_test]
async fn busy_udf_precedes_its_failed_siml_read() {
    let db = build().await;
    process(&db, "BUSY:U").await;
    assert_eq!(
        published(&db, "BUSY:U"),
        (0, alarm_status::UDF_ALARM, AlarmSeverity::Invalid)
    );
}
