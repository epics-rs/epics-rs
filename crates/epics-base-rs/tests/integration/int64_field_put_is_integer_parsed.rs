//! Cause B: a record field the C `.dbd` declares `DBF_INT64` (int64in/int64out)
//! or `DBF_LONG` (aSub `VAL`) is an INTEGER put, not a double.
//!
//! `caput` sends these fields as `DBR_STRING`; C converts them with
//! `dbConvert.c`'s `putStringInt64` / `putStringLong`, i.e.
//! `epicsParseInt64` / `epicsParseLong`, which REFUSE a value outside the
//! integer's range. The port modeled the fields as `f64`, so the put target
//! resolved to `DBF_DOUBLE` and `c_parse::put_string` parsed via the float row,
//! accepting `9.22e18` where C rejects. Modeling them as `i64` / `i32` keys the
//! parse on the integer row and closes the divergence; they are still served
//! over CA as `DBR_DOUBLE` (`EpicsValue::Int64`'s wire mapping) / `DBR_LONG`.
//!
//! The boundary is RANGE, not fractionality: `epicsParse*` uses `strtol`, which
//! tolerates trailing text, so `"5volts"` and `"1.5"` parse to `5` and `1` — the
//! same trailing-text tolerance the dbCommon parse gate keeps. Only an
//! out-of-integer-range magnitude is refused.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::asub_record::ASubRecord;
use epics_base_rs::server::records::int64in::Int64inRecord;
use epics_base_rs::server::records::int64out::Int64outRecord;
use epics_base_rs::types::EpicsValue;

/// `caput REC.FIELD <text>` over the put-notify path the oracle drives.
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

async fn db_with(record: Box<dyn epics_base_rs::server::record::Record>) -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("REC", record).await.unwrap();
    db
}

// --- int64out: DBF_INT64 operator/drive fields --------------------------------

/// `9223372036854775808` is `2^63`, one past `i64::MAX`. A `DBF_DOUBLE` parse
/// accepts it (`9.22e18`); `epicsParseInt64` refuses it — so the put must be
/// REFUSED and the field must keep its prior value.
#[epics_macros_rs::epics_test]
async fn int64out_hopr_over_i64_max_is_refused() {
    let db = db_with(Box::new(Int64outRecord::new(0))).await;
    assert!(
        caput(&db, "HOPR", "9223372036854775808").await.is_err(),
        "HOPR is DBF_INT64: a value past i64::MAX must be refused, not stored as a double"
    );
    assert_eq!(
        stored(&db, "HOPR").await,
        EpicsValue::Int64(0),
        "the refused conversion stored nothing"
    );
}

/// The same magnitude a double would silently accept is fine once it fits: the
/// top of the i64 band lands, stored as `Int64`.
#[epics_macros_rs::epics_test]
async fn int64out_drive_and_display_fields_accept_the_i64_band() {
    let db = db_with(Box::new(Int64outRecord::new(0))).await;
    for field in [
        "HOPR", "LOPR", "DRVH", "DRVL", "HYST", "IVOV", "ADEL", "MDEL",
    ] {
        caput(&db, field, "1000000000000")
            .await
            .unwrap_or_else(|e| panic!("{field} must accept an in-range i64: {e}"));
        assert_eq!(
            stored(&db, field).await,
            EpicsValue::Int64(1_000_000_000_000),
            "{field} stores the parsed i64"
        );
    }
    // i64::MAX is the last accepted value.
    caput(&db, "HOPR", "9223372036854775807").await.unwrap();
    assert_eq!(stored(&db, "HOPR").await, EpicsValue::Int64(i64::MAX));
}

/// Trailing-text tolerance is preserved (C `strtol`): `5volts` -> `5`, and a
/// fractional string truncates rather than being rejected — RANGE is the gate,
/// not fractionality.
#[epics_macros_rs::epics_test]
async fn int64out_trailing_text_and_fraction_truncate_not_reject() {
    let db = db_with(Box::new(Int64outRecord::new(0))).await;
    caput(&db, "HOPR", "5volts").await.unwrap();
    assert_eq!(stored(&db, "HOPR").await, EpicsValue::Int64(5));
    caput(&db, "MDEL", "1.5").await.unwrap();
    assert_eq!(stored(&db, "MDEL").await, EpicsValue::Int64(1));
}

// --- int64in: DBF_INT64 fields via the derive macro ---------------------------

#[epics_macros_rs::epics_test]
async fn int64in_over_i64_max_is_refused_in_range_accepted() {
    let db = db_with(Box::new(Int64inRecord::new(0))).await;
    for field in ["HOPR", "LOPR", "HYST", "ADEL", "MDEL"] {
        assert!(
            caput(&db, field, "9223372036854775808").await.is_err(),
            "int64in.{field} is DBF_INT64: a value past i64::MAX must be refused"
        );
        assert_eq!(stored(&db, field).await, EpicsValue::Int64(0));

        caput(&db, field, "42")
            .await
            .unwrap_or_else(|e| panic!("int64in.{field} must accept 42: {e}"));
        assert_eq!(stored(&db, field).await, EpicsValue::Int64(42));
    }
}

// --- aSub VAL: DBF_LONG (epicsInt32) ------------------------------------------

/// aSub `VAL` is `DBF_LONG`, i.e. `epicsInt32`. `2147483648` is `2^31`, one past
/// `i32::MAX`; a double parse accepts it, `epicsParseLong` refuses it.
#[epics_macros_rs::epics_test]
async fn asub_val_over_i32_max_is_refused() {
    let db = db_with(Box::new(ASubRecord::default())).await;
    assert!(
        caput(&db, "VAL", "2147483648").await.is_err(),
        "aSub VAL is DBF_LONG: a value past i32::MAX must be refused"
    );
    assert_eq!(stored(&db, "VAL").await, EpicsValue::Long(0));
    assert!(
        caput(&db, "VAL", "-2147483649").await.is_err(),
        "aSub VAL is DBF_LONG: a value below i32::MIN must be refused"
    );
    assert_eq!(stored(&db, "VAL").await, EpicsValue::Long(0));
}

#[epics_macros_rs::epics_test]
async fn asub_val_in_range_accepted_as_i32() {
    let db = db_with(Box::new(ASubRecord::default())).await;
    caput(&db, "VAL", "42").await.unwrap();
    assert_eq!(stored(&db, "VAL").await, EpicsValue::Long(42));
    // The i32 band edges land.
    caput(&db, "VAL", "2147483647").await.unwrap();
    assert_eq!(stored(&db, "VAL").await, EpicsValue::Long(i32::MAX));
    caput(&db, "VAL", "-2147483648").await.unwrap();
    assert_eq!(stored(&db, "VAL").await, EpicsValue::Long(i32::MIN));
}

// --- serving is unchanged: DBF_INT64/DBF_LONG fields keep their CA wire type ---

/// The stored variant now integer, the CA wire type is unchanged: `Int64`
/// promotes to `DBR_DOUBLE` (the record serves int64 as double, as C does),
/// `Long` stays `DBR_LONG`.
#[epics_macros_rs::epics_test]
async fn integer_fields_keep_their_ca_wire_type() {
    assert_eq!(
        EpicsValue::Int64(1).dbr_type(),
        epics_base_rs::types::DbFieldType::Double,
        "int64 fields serve as DBR_DOUBLE"
    );
    assert_eq!(
        EpicsValue::Long(1).dbr_type(),
        epics_base_rs::types::DbFieldType::Long,
        "aSub VAL serves as DBR_LONG"
    );
}
