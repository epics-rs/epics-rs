//! `transformRecord.c:754-766` `get_precision` is three-way and its first arm
//! is a LITERAL:
//!
//! ```c
//! *precision = ptran->prec;
//! if (fieldIndex == transformRecordVERS) { *precision = 2; }
//! else if (fieldIndex >= transformRecordVAL) { *precision = ptran->prec; }
//! else { recGblGetPrec(paddr, precision); }
//! ```
//!
//! No generic serve-PREC arm can answer VERS: the C value is a constant, so a
//! `transform` arm reading PREC would still print `6` for `caget -s T.VERS`
//! where C prints `5.80`.

use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::types::EpicsValue;

fn transform_with_prec(prec: i16) -> RecordInstance {
    let rec = create_record("transform").expect("transform is registered");
    let mut inst = RecordInstance::new_boxed("T".to_string(), rec);
    inst.record
        .put_field("PREC", EpicsValue::Short(prec))
        .expect("transform stores PREC");
    inst
}

fn dbr_string_of(inst: &RecordInstance, field: &str) -> String {
    let snap = inst.snapshot_for_field(field).expect("field exists");
    let bytes = epics_base_rs::types::encode_dbr(0, &snap).expect("DBR_STRING encodes");
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn precision_of(inst: &RecordInstance, field: &str) -> Option<i16> {
    inst.snapshot_for_field(field)
        .expect("field exists")
        .precision()
}

/// The lead's trigger: a bare `record(transform,"T"){}`, `caget -s T.VERS`.
#[test]
fn vers_answers_the_literal_two() {
    let inst = transform_with_prec(0);
    assert_eq!(dbr_string_of(&inst, "VERS"), "5.80");
    assert_eq!(precision_of(&inst, "VERS"), Some(2));
}

/// The literal does not move with PREC — that is what makes it a literal.
#[test]
fn raising_prec_leaves_vers_alone_but_moves_the_others() {
    let inst = transform_with_prec(4);
    assert_eq!(precision_of(&inst, "VERS"), Some(2), "still the literal");
    assert_eq!(dbr_string_of(&inst, "VERS"), "5.80");

    // `fieldIndex >= transformRecordVAL` — the PREC arm.
    for f in ["VAL", "A", "P"] {
        assert_eq!(precision_of(&inst, f), Some(4), "{f}");
    }
}

/// A dbCommon float field is C's third arm (`recGblGetPrec`), which only
/// CLAMPS an out-of-range seed — so an in-range PREC reaches it unchanged.
#[test]
fn a_dbcommon_double_field_keeps_the_prec_seed() {
    let inst = transform_with_prec(3);
    assert_eq!(precision_of(&inst, "HYST"), Some(3));
}
