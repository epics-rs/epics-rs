//! mbbiDirect B0..B1F (DBF_UCHAR) PUT parity — the `type-max` class of the
//! 32-case oracle bit-field gap (32 bits, type-max only; over-max /
//! non-numeric-text already agreed).
//!
//! `caput -c mbbiDirect.Bn 255` is ACCEPTED by the compiled C softIoc: the bit
//! is DBF_UCHAR, so C parses the put through the unsigned 0..=255 range. The
//! port previously served the bit as a signed `Char`, so the put-coercion
//! target was signed i8 and everything above i8-max (128..=255) was REFUSED.
//! Now the bit is served as the native `UChar` (still `DBR_CHAR` on the wire),
//! so the coercion target is unsigned and 255 is accepted.
//!
//! mbbiDirect is an INPUT record: B0..B1F are DERIVED from VAL (no `special()`
//! for the bits, `mbbiDirectRecord.c`), so a `pp(TRUE)` bit put processes the
//! record and `monitor()` re-derives every bit as `!! (val & 1)` from the
//! unchanged VAL. On a fresh record VAL=0, so the bit reads back 0 after the
//! accepted put — exactly what C does (oracle c_side: value 0, NO_ALARM,
//! put_accepted=true). The fix only flips put_accepted false→true.

// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the feature-ON suite.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(mbbiDirect, "TMAX") {}
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

/// (SEVR, STAT) as a client would read them after the put.
async fn alarm(db: &PvDatabase, name: &str) -> (AlarmSeverity, u16) {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    (inst.common.sevr, inst.common.stat)
}

/// `caput -c TMAX.B0 255` is ACCEPTED. The `pp(TRUE)` put processes the record,
/// which re-derives the bit from the unchanged VAL=0 → B0 reads back 0. Live C
/// (oracle): `put_accepted=true, B0=0, STAT=NO_ALARM, SEVR=NO_ALARM`.
#[tokio::test]
async fn type_max_bit_put_is_accepted() {
    let db = ioc().await;

    let r = db
        .put_record_field_from_ca("TMAX", "B0", EpicsValue::String("255".into()))
        .await;
    assert!(
        r.is_ok(),
        "255 is in the DBF_UCHAR range 0..=255 — C accepts it, so must the port"
    );

    // INPUT record: the bit is re-derived from VAL=0 on the pp(TRUE) process,
    // so it reads back 0 (a Bx put does NOT fold into VAL here).
    assert_eq!(
        field(&db, "TMAX", "B0").await,
        EpicsValue::UChar(0),
        "the bit is re-derived from VAL=0; served as native UChar (DBF_UCHAR)"
    );
    assert_eq!(
        field(&db, "TMAX", "VAL").await,
        EpicsValue::Long(0),
        "a Bx put must not fold into VAL on an INPUT record"
    );
    assert_eq!(
        alarm(&db, "TMAX").await,
        (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM),
        "the accepted bit put processes with no alarm — matches C"
    );
}
