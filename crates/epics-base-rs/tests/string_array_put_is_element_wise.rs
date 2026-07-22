//! R18-105: a `DBR_STRING` array put is an ELEMENT-BY-ELEMENT numeric
//! conversion, not a scalar one.
//!
//! `caput -a` sends `DBR_STRING` unless told otherwise, so this is the default
//! array-write path for every shell/script client. C converts it with the
//! `putString*` family (`dbConvert.c:941-1148`), which runs the same per-element
//! parse over all `nRequest` elements:
//!
//! ```c
//! static long putStringLong(dbAddr *paddr, const void *pfrom,
//!     long nRequest, long no_elements, long offset)
//! {
//!     const char *psrc = pfrom;
//!     epicsInt32 *pdst = (epicsInt32 *) paddr->pfield + offset;
//!     while (nRequest--) {
//!         char *end;
//!         long status = epicsParseInt32(psrc, pdst++, dbConvertBase, &end);
//!         if (status) return status;
//!         psrc += MAX_STRING_SIZE;                       /* next element */
//!         ...
//!     }
//! }
//! ```
//!
//! The port's `as_f64_array` had no `StringArray` arm, so a string array fell
//! through to the SCALAR tail of `convert_to` and collapsed to one element —
//! `caput -a WV 3 7 8 9` wrote `[0.0]` with `NORD = 1` and returned success,
//! destroying the record's contents.
//!
//! Expectations are head-to-head against the built softIoc
//! (`/home/stevek/work/epics-base/bin/linux-x86_64`), same `.db`, C `caput`:
//!
//! ```text
//! caput -a WV 3 7 8 9      -> WV = 7 8 9 0 0    NORD = 3
//! caput -a WV 2 7.5 -3.9   -> WV = 7 -3 0 0 0   NORD = 2   (truncates toward 0)
//! caput -a WV 2 0x10 12    -> WV = 16 12 0 0 0  NORD = 2   (dbConvertBase = 0)
//! caput -a WD 3 1.5 2.5 3.5 -> WD = 1.5 2.5 3.5 NORD = 3
//! ```

// RTEMS-EXEC-MODEL-ALLOW(4): checked - these run and pass in the feature-ON suite.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString};

const DB: &str = r#"
record(waveform, "WV") { field(FTVL, "LONG")   field(NELM, "5") }
record(waveform, "WD") { field(FTVL, "DOUBLE") field(NELM, "5") }
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

fn strings(items: &[&str]) -> EpicsValue {
    EpicsValue::StringArray(items.iter().map(|s| PvString::from(*s)).collect())
}

async fn nord(db: &PvDatabase, rec: &str) -> f64 {
    db.get_pv(&format!("{rec}.NORD")).unwrap().to_f64().unwrap()
}

/// The finding's exact case: `caput -a WV 3 7 8 9` on a `FTVL=LONG` waveform.
#[tokio::test]
async fn ca_string_array_put_writes_every_element() {
    let db = build().await;

    db.put_record_field_from_ca("WV", "VAL", strings(&["7", "8", "9"]))
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("WV").unwrap(),
        EpicsValue::LongArray(vec![7, 8, 9]),
        "a DBR_STRING array put must convert element by element, not collapse"
    );
    assert_eq!(nord(&db, "WV").await, 3.0, "NORD is the element count");
}

/// A `DOUBLE` waveform takes the same path with the float parse.
#[tokio::test]
async fn ca_string_array_put_to_a_double_waveform() {
    let db = build().await;

    db.put_record_field_from_ca("WD", "VAL", strings(&["1.5", "2.5", "3.5"]))
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("WD").unwrap(),
        EpicsValue::DoubleArray(vec![1.5, 2.5, 3.5])
    );
    assert_eq!(nord(&db, "WD").await, 3.0);
}

/// Per-element semantics are the SCALAR string semantics, applied N times:
/// truncation toward zero into an integer target, and `dbConvertBase = 0`
/// hex. softIoc: `7.5 -3.9` → `7 -3`; `0x10 12` → `16 12`.
#[tokio::test]
async fn per_element_conversion_matches_the_scalar_string_rules() {
    let db = build().await;

    db.put_record_field_from_ca("WV", "VAL", strings(&["7.5", "-3.9"]))
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("WV").unwrap(),
        EpicsValue::LongArray(vec![7, -3]),
        "epicsParseInt32 keeps the leading integer — it does not round"
    );

    db.put_record_field_from_ca("WV", "VAL", strings(&["0x10", "12"]))
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("WV").unwrap(),
        EpicsValue::LongArray(vec![16, 12]),
        "dbConvertBase is 0, so a 0x-prefixed element is hex"
    );
}

/// The fix lives in the one coercion owner (`EpicsValue::convert_to`), so every
/// numeric array target gets it — not just the waveform put route.
#[tokio::test]
async fn convert_to_is_element_wise_for_every_numeric_target() {
    let src = strings(&["1", "2", "3"]);

    assert_eq!(
        src.convert_to(DbFieldType::Short),
        EpicsValue::ShortArray(vec![1, 2, 3])
    );
    assert_eq!(
        src.convert_to(DbFieldType::Long),
        EpicsValue::LongArray(vec![1, 2, 3])
    );
    assert_eq!(
        src.convert_to(DbFieldType::ULong),
        EpicsValue::ULongArray(vec![1, 2, 3])
    );
    assert_eq!(
        src.convert_to(DbFieldType::Double),
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0])
    );
    assert_eq!(
        src.convert_to(DbFieldType::Float),
        EpicsValue::FloatArray(vec![1.0, 2.0, 3.0])
    );
    // C `putStringChar` parses each element with epicsParseInt8 — a string
    // ARRAY into a CHAR field is numeric, unlike the scalar long-string put.
    assert_eq!(
        src.convert_to(DbFieldType::Char),
        EpicsValue::CharArray(vec![1, 2, 3])
    );
}
