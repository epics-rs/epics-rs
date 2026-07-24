//! mbboDirect B0..B1F (DBF_UCHAR) PUT parity — the two causes of the 96-case
//! oracle bit-field gap (32 bits × {type-max, over-max, non-numeric-text}).
//!
//! Cause 1 — TYPE-MAX ACCEPT: `caput -c mbboDirect.Bn 255` is accepted by the
//! compiled C softIoc (stores bit=1). The bit is DBF_UCHAR, so C parses the put
//! through the unsigned 0..=255 range; the port previously served the bit as a
//! signed `Char` and refused everything above i8-max. Now the bit is served as
//! the native `UChar`, so the coercion target is unsigned and 255 is accepted.
//!
//! Cause 2 — REJECTED-PUT STAT/SEVR: `caput -c mbboDirect.Bn 256` /
//! `notanumber` is REFUSED by both C and the port, but C's `dbPut` runs
//! `dbPutSpecial(paddr, 1)` UNCONDITIONALLY (dbAccess.c:1401), and for B0..B1F
//! that special sets `prec->udf = FALSE` (mbboDirectRecord.c:290) even on the
//! rejected conversion. The notify-process that follows then recomputes
//! STAT/SEVR to NO_ALARM instead of the born-UDF INVALID. The primary VAL field
//! is EXCLUDED — its UDF clear is `isValueField` (post-status), so a rejected
//! VAL put keeps UDF/INVALID on both C and the port.
//!
//! Ground truth captured live from softIoc 7.0.10.1-DEV on this host
//! (`caput -c`, one fresh `record(mbboDirect,"X"){}` per case).

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(mbboDirect, "TMAX") {}
record(mbboDirect, "OMAX") {}
record(mbboDirect, "NONNUM") {}
record(mbboDirect, "VALREJ") {}
"#;

async fn ioc() -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

async fn field(db: &PvDatabase, name: &str, f: &str) -> EpicsValue {
    db.get_record(name)
        .unwrap()
        .read()
        .record
        .get_field(f)
        .unwrap_or_else(|| panic!("{name}.{f} missing"))
}

/// (SEVR, STAT, UDF) as a client would read them after the put.
async fn alarm(db: &PvDatabase, name: &str) -> (AlarmSeverity, u16, bool) {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    (inst.common.sevr, inst.common.stat, inst.common.udf != 0)
}

/// Cause 1: `caput -c TMAX.B0 255` is accepted; the bit stores 1 and folds into
/// VAL. Live C: `TMAX.B0=1 VAL=1 STAT=NO_ALARM UDF=0`.
#[epics_macros_rs::epics_test]
async fn type_max_bit_put_is_accepted_and_stores_one() {
    let db = ioc().await;

    let r = db
        .put_record_field_from_ca("TMAX", "B0", EpicsValue::String("255".into()))
        .await;
    assert!(
        r.is_ok(),
        "255 is in the DBF_UCHAR range 0..=255 — C accepts it, so must the port"
    );

    assert_eq!(
        field(&db, "TMAX", "B0").await,
        EpicsValue::UChar(1),
        "the bit is DBF_UCHAR and any NONZERO coerced byte sets it (C `if (*pBn)`)"
    );
    assert_eq!(
        field(&db, "TMAX", "VAL").await,
        EpicsValue::Long(1),
        "B0=1 folds into VAL bit 0"
    );
    assert_eq!(
        alarm(&db, "TMAX").await,
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM, false),
        "the accepted bit put clears UDF and processes → NO_ALARM"
    );
}

/// Cause 2 (over-max): `caput -c OMAX.B0 256` is REFUSED, but the after-put
/// special still clears UDF and the record processes → NO_ALARM. Live C:
/// `put FAILED, B0=0 STAT=NO_ALARM SEVR=NO_ALARM UDF=0`.
#[epics_macros_rs::epics_test]
async fn rejected_over_max_bit_put_clears_udf_reads_no_alarm() {
    let db = ioc().await;

    let r = db
        .put_record_field_from_ca("OMAX", "B0", EpicsValue::String("256".into()))
        .await;
    assert!(r.is_err(), "256 is out of the DBF_UCHAR range — refused");

    assert_eq!(
        field(&db, "OMAX", "B0").await,
        EpicsValue::UChar(0),
        "the rejected conversion wrote no bit"
    );
    assert_eq!(
        alarm(&db, "OMAX").await,
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM, false),
        "C's unconditional dbPutSpecial(paddr,1) clears UDF even on the rejected \
         put; the notify-process recomputes NO_ALARM (not born-UDF INVALID)"
    );
}

/// Cause 2 (non-numeric): `caput -c NONNUM.B0 notanumber` — same as over-max.
#[epics_macros_rs::epics_test]
async fn rejected_non_numeric_bit_put_clears_udf_reads_no_alarm() {
    let db = ioc().await;

    let r = db
        .put_record_field_from_ca("NONNUM", "B0", EpicsValue::String("notanumber".into()))
        .await;
    assert!(r.is_err(), "non-numeric text is refused");

    assert_eq!(
        alarm(&db, "NONNUM").await,
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM, false),
        "the after-put special clears UDF on reject; process → NO_ALARM"
    );
}

/// Negative control for the `field != primary_field()` exclusion: a rejected
/// VAL put must KEEP UDF/INVALID — VAL's UDF clear is `isValueField`, which runs
/// AFTER the status check, so a rejected conversion never reaches it. Live C:
/// `caput -c VALREJ.VAL notanumber` → `STAT=UDF SEVR=INVALID UDF=1`.
#[epics_macros_rs::epics_test]
async fn rejected_val_put_keeps_udf_invalid() {
    let db = ioc().await;

    let r = db
        .put_record_field_from_ca("VALREJ", "VAL", EpicsValue::String("notanumber".into()))
        .await;
    assert!(r.is_err(), "non-numeric VAL is refused");

    assert_eq!(
        alarm(&db, "VALREJ").await,
        (AlarmSeverity::Invalid, alarm_status::UDF_ALARM, true),
        "a rejected VAL put does NOT clear UDF (isValueField clear is post-status) \
         — the born-UDF INVALID persists, matching C"
    );
}
