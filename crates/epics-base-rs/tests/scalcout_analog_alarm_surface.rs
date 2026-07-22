//! R18-5: scalcout has the full analog-alarm surface its C `.dbd` declares.
//!
//! `sCalcoutRecord.dbd:479-531` declares HIHI/LOLO/HIGH/LOW, HHSV/LLSV/HSV/LSV
//! and HYST; `:858` declares LALM (`special(SPC_NOMOD)`). `checkAlarms`
//! (`sCalcoutRecord.c:699-751`) is the same per-level hysteresis ladder
//! calc/calcout/ai/ao run, and `process` calls it BEFORE the OOPT switch
//! (`:371`) precisely so a limit excursion can drive IVOA.
//!
//! The port had NONE of it: `rg 'HIHI|HHSV|HYST|LALM' scalcout.rs` was empty and
//! the record was absent from the shared `AnalogAlarmConfig` slot, so
//! `caput scalc.HIHI 5` was a `FieldNotFound` and a scalcout could never go
//! MINOR/MAJOR on its own result.
//!
//! Boundaries: below limit / at limit; MAJOR (HIHI) vs MINOR (HIGH); the
//! hysteresis band on the way back down (which is what LALM is for); and the
//! IVOA gate the C ordering exists to feed.

// RTEMS-EXEC-MODEL-ALLOW(4): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(scalcout, "S:LIM") {
    field(CALC, "A")
    field(HIHI, "10")
    field(HHSV, "MAJOR")
    field(HIGH, "5")
    field(HSV,  "MINOR")
    field(LOW,  "-5")
    field(LSV,  "MINOR")
    field(LOLO, "-10")
    field(LLSV, "MAJOR")
    field(HYST, "2")
}
record(ao, "T:IVOA") { field(VAL, "0") }
record(scalcout, "S:IVOA") {
    field(CALC, "20")
    field(HIHI, "10")
    field(HHSV, "INVALID")
    field(IVOA, "Don't drive outputs")
    field(OOPT, "Every Time")
    field(OUT,  "T:IVOA PP")
}
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

/// Drive VAL through A and read back the committed (severity, status).
async fn severity_at(db: &PvDatabase, a: f64) -> (AlarmSeverity, u16) {
    db.put_pv("S:LIM.A", EpicsValue::Double(a)).await.unwrap();
    process(db, "S:LIM").await;
    let rec = db.get_record("S:LIM").unwrap();
    let g = rec.read();
    (g.common.sevr, g.common.stat)
}

/// The ten fields exist and take a put — the surface itself. `caput
/// scalc.HIHI 5` used to be `FieldNotFound`.
#[tokio::test]
async fn the_alarm_fields_exist_and_are_writable() {
    let db = build().await;

    assert_eq!(db.get_pv("S:LIM.HIHI").unwrap().to_f64(), Some(10.0));
    assert_eq!(db.get_pv("S:LIM.LOLO").unwrap().to_f64(), Some(-10.0));
    assert_eq!(db.get_pv("S:LIM.HIGH").unwrap().to_f64(), Some(5.0));
    assert_eq!(db.get_pv("S:LIM.LOW").unwrap().to_f64(), Some(-5.0));
    assert_eq!(db.get_pv("S:LIM.HYST").unwrap().to_f64(), Some(2.0));
    assert_eq!(db.get_pv("S:LIM.LALM").unwrap().to_f64(), Some(0.0));

    db.put_pv("S:LIM.HIHI", EpicsValue::Double(42.0))
        .await
        .expect("HIHI is a writable DBF_DOUBLE (sCalcoutRecord.dbd:479)");
    assert_eq!(db.get_pv("S:LIM.HIHI").unwrap().to_f64(), Some(42.0));
}

/// The C ladder, level by level (`sCalcoutRecord.c:727-748`): HIHI/LOLO are
/// checked before HIGH/LOW, and a zero severity disables its level.
#[tokio::test]
async fn the_ladder_raises_each_level() {
    let db = build().await;
    use epics_base_rs::server::recgbl::alarm_status;

    assert_eq!(
        severity_at(&db, 0.0).await,
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "inside the limits: no alarm"
    );
    assert_eq!(
        severity_at(&db, 7.0).await,
        (AlarmSeverity::Minor, alarm_status::HIGH_ALARM),
        "val >= HIGH (5), HSV=MINOR"
    );
    assert_eq!(
        severity_at(&db, 12.0).await,
        (AlarmSeverity::Major, alarm_status::HIHI_ALARM),
        "val >= HIHI (10), HHSV=MAJOR — checked before HIGH"
    );
    assert_eq!(
        severity_at(&db, -12.0).await,
        (AlarmSeverity::Major, alarm_status::LOLO_ALARM),
        "val <= LOLO (-10), LLSV=MAJOR"
    );
    assert_eq!(
        severity_at(&db, -7.0).await,
        (AlarmSeverity::Minor, alarm_status::LOW_ALARM),
        "val <= LOW (-5), LSV=MINOR"
    );
}

/// HYST is a per-LEVEL hysteresis keyed on LALM, not a plain deadband — C
/// `(lalm == hihi) && (val >= hihi - hyst)`. Coming down from 12 to 9, the HIHI
/// alarm HOLDS (9 >= 10 - 2) because LALM latched at 10; at 7.5 it releases to
/// the HIGH level.
#[tokio::test]
async fn hysteresis_holds_the_level_through_lalm() {
    let db = build().await;
    use epics_base_rs::server::recgbl::alarm_status;

    assert_eq!(
        severity_at(&db, 12.0).await,
        (AlarmSeverity::Major, alarm_status::HIHI_ALARM)
    );
    assert_eq!(
        db.get_pv("S:LIM.LALM").unwrap().to_f64(),
        Some(10.0),
        "C latches LALM at the LEVEL it alarmed at (hihi), not at VAL"
    );

    assert_eq!(
        severity_at(&db, 9.0).await,
        (AlarmSeverity::Major, alarm_status::HIHI_ALARM),
        "9 >= HIHI - HYST (8) and LALM == HIHI: the HIHI alarm holds"
    );
    assert_eq!(
        severity_at(&db, 7.5).await,
        (AlarmSeverity::Minor, alarm_status::HIGH_ALARM),
        "7.5 < HIHI - HYST: HIHI releases, and 7.5 >= HIGH is MINOR"
    );
}

/// The reason C calls `checkAlarms` BEFORE the OOPT switch (`:371`): the
/// severity it raises is what the output stage reads. `execOutput` writes OUT
/// only `if (pcalc->nsev < INVALID_ALARM)` (`sCalcoutRecord.c:780-786`) and
/// otherwise takes the IVOA branch — so an INVALID limit excursion must reach
/// the gate and leave the OUT target untouched. A record with no alarm surface
/// at all can never raise that severity, which is what made this unreachable.
#[tokio::test]
async fn an_invalid_limit_excursion_reaches_the_ivoa_gate() {
    let db = build().await;

    process(&db, "S:IVOA").await;

    let rec = db.get_record("S:IVOA").unwrap();
    let sevr = rec.read().common.sevr;
    assert_eq!(
        sevr,
        AlarmSeverity::Invalid,
        "CALC=20 is over HIHI=10 with HHSV=INVALID"
    );
    assert_eq!(
        db.get_pv("T:IVOA").unwrap(),
        EpicsValue::Double(0.0),
        "IVOA=Don't drive outputs vetoes the OUT write on the INVALID cycle"
    );
}
