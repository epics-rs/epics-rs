//! The `DBF_FLOAT`/`DBF_DOUBLE` → `DBF_STRING` row of C's put table renders
//! with the record's `get_precision`, seeded 6.
//!
//! `putFloatString` / `putDoubleString` (`dbConvert.c:1558`/`:1600`) and the
//! scalar twins `cvt_f_st` / `cvt_d_st` (`dbFastLinkConv.c:1216`/`:1333`) all
//! open `long precision = 6;` and overwrite it from `prset->get_precision`
//! before calling `cvtFloatToString`/`cvtDoubleToString`. Every other numeric
//! row of that column is precision-free.
//!
//! softIoc (EPICS 7.0.10, linux-x86_64), `T:AI` an `ai` with `PREC=3` and
//! `T:SO` a `stringout`:
//!
//! ```text
//! dbtpf T:AI.DESC 1.0        -> DBF_STRING:         "1.000"
//! dbtpf T:AI.EGU  1.23456789 -> DBF_STRING:         "1.235"
//! dbtpf T:SO.VAL  1.0        -> DBF_STRING:         "1.000000"
//! ```
//!
//! Pre-fix the port rendered all three through Rust's `Display` — `"1"`,
//! `"1.23456789"`, `"1"` — so one field read back differently depending on
//! whether the value arrived through `dbtgf` (which already consulted PREC)
//! or through a put.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::histogram::HistogramRecord;
use epics_base_rs::server::records::stringout::StringoutRecord;
use epics_base_rs::types::EpicsValue;

async fn read(db: &PvDatabase, record: &str, field: &str) -> String {
    let r = db.get_record(record).unwrap();
    let r = r.read();
    match r.resolve_field_stored(field).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{record}.{field} is not a DBF_STRING: {other:?}"),
    }
}

/// `ai` seeds `*precision = prec->prec` before the shared tail
/// (`aiRecord.c:238-239`), and `recGblGetPrec` has no `DBF_STRING` case, so a
/// dbCommon string field keeps PREC.
#[epics_macros_rs::epics_test]
async fn a_float_put_into_a_string_field_renders_at_the_records_prec() {
    let db = PvDatabase::new();
    db.add_record("T:AI", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.put_record_field_from_ca_no_notify("T:AI", "PREC", EpicsValue::Short(3))
        .await
        .unwrap();

    db.put_record_field_from_ca_no_notify("T:AI", "DESC", EpicsValue::Float(1.0))
        .await
        .unwrap();
    assert_eq!(read(&db, "T:AI", "DESC").await, "1.000");

    db.put_record_field_from_ca_no_notify("T:AI", "EGU", EpicsValue::Double(1.23456789))
        .await
        .unwrap();
    assert_eq!(read(&db, "T:AI", "EGU").await, "1.235");
}

/// A record type whose rset NULLs `get_precision` never overwrites the seed,
/// so the put renders at 6 — `stringout` is the measured case.
#[epics_macros_rs::epics_test]
async fn a_record_with_no_get_precision_slot_renders_at_the_seeded_six() {
    let db = PvDatabase::new();
    db.add_record("T:SO", Box::new(StringoutRecord::new("")))
        .await
        .unwrap();

    db.put_record_field_from_ca_no_notify("T:SO", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(read(&db, "T:SO", "VAL").await, "1.000000");
}

/// `histogram` supplies the slot but its body is a bare switch with no seed
/// and no case a dbCommon field reaches (`histogramRecord.c:420-438`), so the
/// caller's 6 survives even though the record HAS a `PREC` field. The boundary
/// that separates "has PREC" from "get_precision answers PREC".
#[epics_macros_rs::epics_test]
async fn a_records_prec_is_not_used_where_its_get_precision_never_seeds_it() {
    let db = PvDatabase::new();
    db.add_record("T:HI", Box::new(HistogramRecord::new(4, 0.0, 4.0)))
        .await
        .unwrap();
    db.put_record_field_from_ca_no_notify("T:HI", "PREC", EpicsValue::Short(3))
        .await
        .unwrap();

    db.put_record_field_from_ca_no_notify("T:HI", "DESC", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(read(&db, "T:HI", "DESC").await, "1.000000");
}

/// The other side of the family boundary: `cvtLongToString` and its siblings
/// take no precision argument at all, so an integer put keeps the plain
/// decimal whatever PREC says.
#[epics_macros_rs::epics_test]
async fn an_integer_put_into_a_string_field_ignores_prec() {
    let db = PvDatabase::new();
    db.add_record("T:AI", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.put_record_field_from_ca_no_notify("T:AI", "PREC", EpicsValue::Short(3))
        .await
        .unwrap();

    db.put_record_field_from_ca_no_notify("T:AI", "DESC", EpicsValue::Long(1))
        .await
        .unwrap();
    assert_eq!(read(&db, "T:AI", "DESC").await, "1");
}
