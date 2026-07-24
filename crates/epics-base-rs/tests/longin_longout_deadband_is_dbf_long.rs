//! Cause 1: longin/longout ADEL/MDEL are `DBF_LONG`, so an over-range put is
//! REFUSED, not saturated.
//!
//! These deadbands are `field(ADEL,DBF_LONG)` / `field(MDEL,DBF_LONG)`
//! (longinRecord.dbd.pod / longoutRecord.dbd.pod). `caput` sends them as
//! `DBR_STRING`; C converts with `dbConvert.c`'s `putStringLong` ->
//! `epicsParseInt32`, which REFUSES a value outside the `i32` range (the field
//! keeps its old value). The port modeled them as `f64`, so the put target
//! resolved to `DBF_DOUBLE`, `c_parse::put_string` parsed via the float row and
//! accepted the value, and the read path saturated it to `i32::MAX`/`MIN` —
//! `caput LI.ADEL 2147483648` stored 2147483647 where C rejects the put.
//!
//! Modeling them as `i32` keys the parse on `c_parse::put_string`'s `Long` row
//! and closes the divergence; they are still served over CA as `DBR_LONG`. The
//! boundary is RANGE, not fractionality — `strtol` trailing-text tolerance keeps
//! `"5volts"` -> `5`. Mirrors the int64in/int64out Cause B fix (commit 224d5ad5).

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::longin::LonginRecord;
use epics_base_rs::server::records::longout::LongoutRecord;
use epics_base_rs::types::EpicsValue;

/// `caput REC.FIELD <text>` over the CA put path the oracle drives.
async fn caput(db: &PvDatabase, field: &str, text: &str) -> Result<(), String> {
    db.put_record_field_from_ca("REC", field, EpicsValue::String(text.into()))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The raw stored field value (not the CA-served projection).
async fn stored(db: &PvDatabase, field: &str) -> EpicsValue {
    let rec = db.get_record("REC").unwrap();
    let inst = rec.read();
    inst.record.get_field(field).unwrap()
}

async fn db_with(record: Box<dyn Record>) -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("REC", record).await.unwrap();
    db
}

/// `2147483648` is `2^31`, one past `i32::MAX`; `-2147483649` is one past
/// `i32::MIN`. `epicsParseInt32` refuses both — so the put must be REFUSED and
/// the field must keep its prior value (0).
#[epics_macros_rs::epics_test]
async fn longin_adel_mdel_over_i32_are_refused() {
    let db = db_with(Box::new(LonginRecord::new(0))).await;
    for field in ["ADEL", "MDEL"] {
        assert!(
            caput(&db, field, "2147483648").await.is_err(),
            "{field} is DBF_LONG: over-i32::MAX must be refused, not saturated"
        );
        assert!(
            caput(&db, field, "-2147483649").await.is_err(),
            "{field} is DBF_LONG: under-i32::MIN must be refused, not saturated"
        );
        assert_eq!(
            stored(&db, field).await,
            EpicsValue::Long(0),
            "{field} kept its old value after the refused put"
        );
    }
}

#[epics_macros_rs::epics_test]
async fn longout_adel_mdel_over_i32_are_refused() {
    let db = db_with(Box::new(LongoutRecord::new(0))).await;
    for field in ["ADEL", "MDEL"] {
        assert!(
            caput(&db, field, "2147483648").await.is_err(),
            "{field} is DBF_LONG: over-i32::MAX must be refused, not saturated"
        );
        assert!(
            caput(&db, field, "-2147483649").await.is_err(),
            "{field} is DBF_LONG: under-i32::MIN must be refused, not saturated"
        );
        assert_eq!(
            stored(&db, field).await,
            EpicsValue::Long(0),
            "{field} kept its old value after the refused put"
        );
    }
}

/// The band edge (`i32::MAX` / `i32::MIN`) still lands, served as `Long` — the
/// gate is RANGE, not fractionality (`strtol` trailing-text tolerance keeps
/// `"5volts"` -> `5`).
#[epics_macros_rs::epics_test]
async fn longin_longout_adel_accept_the_i32_band_and_trailing_text() {
    for db in [
        db_with(Box::new(LonginRecord::new(0))).await,
        db_with(Box::new(LongoutRecord::new(0))).await,
    ] {
        caput(&db, "ADEL", "2147483647").await.unwrap();
        assert_eq!(stored(&db, "ADEL").await, EpicsValue::Long(i32::MAX));
        caput(&db, "MDEL", "-2147483648").await.unwrap();
        assert_eq!(stored(&db, "MDEL").await, EpicsValue::Long(i32::MIN));
        caput(&db, "ADEL", "5volts").await.unwrap();
        assert_eq!(stored(&db, "ADEL").await, EpicsValue::Long(5));
    }
}
