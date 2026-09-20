//! The four explicit alarm bands are answered from the record-level metadata
//! cache, so a write to any of the eight cells behind them must invalidate it.
//!
//! C needs no such rule: `get_alarm_double` reads `prec->hihi` and
//! `prec->hhsv` as struct members on every call (`calcRecord.c:205-221`), so
//! the served value cannot lag the field. Here the eight `resolve_field` name
//! lookups moved into `cached_metadata`, which makes staleness the one way
//! this can be wrong — and the boundary is the ORDER: a snapshot taken BEFORE
//! the put is what populates the cache the put has to knock down.
//!
//! The cases are per-boundary rather than per-record: the band's value, and
//! the severity that gates it (C serves NaN while the severity is zero, so
//! arming it is a change from NaN rather than between two numbers).

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::types::EpicsValue;

/// `(lolo, low, high, hihi)` as a client reading `CALC.VAL` sees them.
fn served(db: &PvDatabase) -> (f64, f64, f64, f64) {
    let rec = db.get_record("CALC").expect("record exists");
    let snap = rec
        .read()
        .snapshot_for_field("VAL")
        .expect("VAL has a snapshot");
    snap.alarm_limits().expect("calc supplies get_alarm_double")
}

async fn armed_calc() -> PvDatabase {
    let db = PvDatabase::new();
    let mut calc = CalcRecord::default();
    calc.calc = "0".into();
    db.add_record("CALC", Box::new(calc)).await.unwrap();
    db.ioc_init().await;
    db.put_pv("CALC.HHSV", EpicsValue::Short(AlarmSeverity::Major as i16))
        .await
        .unwrap();
    db.put_pv("CALC.HIHI", EpicsValue::Double(90.0))
        .await
        .unwrap();
    db
}

/// The band. A snapshot first, so the cache holds 90; then the put.
#[epics_macros_rs::epics_test]
async fn a_hihi_put_after_a_snapshot_changes_the_served_band() {
    let db = armed_calc().await;
    assert_eq!(served(&db).3, 90.0, "the armed band is what is served");

    db.put_pv("CALC.HIHI", EpicsValue::Double(42.0))
        .await
        .unwrap();
    assert_eq!(
        served(&db).3,
        42.0,
        "a cached band must not outlive the put that replaced it"
    );
}

/// The gate. NaN while the severity is zero, the band once it is armed —
/// with the cache populated in between, which is the ordering that fails if
/// the severities are not cache sources.
#[epics_macros_rs::epics_test]
async fn arming_a_severity_after_a_snapshot_turns_nan_into_the_band() {
    let db = PvDatabase::new();
    let mut calc = CalcRecord::default();
    calc.calc = "0".into();
    db.add_record("CALC", Box::new(calc)).await.unwrap();
    db.ioc_init().await;

    db.put_pv("CALC.LOLO", EpicsValue::Double(10.0))
        .await
        .unwrap();
    assert!(
        served(&db).0.is_nan(),
        "C's `prec->llsv ? prec->lolo : epicsNAN` serves NaN while LLSV is 0"
    );

    db.put_pv("CALC.LLSV", EpicsValue::Short(AlarmSeverity::Major as i16))
        .await
        .unwrap();
    assert_eq!(
        served(&db).0,
        10.0,
        "arming LLSV must invalidate the cached NaN"
    );
}
