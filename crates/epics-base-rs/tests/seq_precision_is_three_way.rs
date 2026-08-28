//! `seqRecord.c:299-319` `get_precision` is three-way, and only one arm is a
//! literal.
//!
//! ```c
//! switch (fieldOffset & 3) {
//! case 0: *pprecision = seqDLYprecision; return 0;              /* DLYn */
//! case 2: if (dbGetPrecision(get_dol(prec, fieldOffset), &precision) == 0) {
//!             *pprecision = precision; return 0; }              /* DOn   */
//! }
//! *pprecision = prec->prec;                                     /* rest  */
//! recGblGetPrec(paddr, pprecision);
//! ```
//!
//! `seq` models no `prec` field of its own — PREC resolves through the
//! declared-override tail — so the fall-through arm served 0 until
//! `route_field_metadata` took the slot over. The `DOn` arm's `dbGetPrecision`
//! fails on a CONSTANT link (`S_db_noLSET`) and falls through with it.

use epics_base_rs::server::database::LinkBacking;
use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::types::EpicsValue;

fn seq_with_prec(prec: i16) -> RecordInstance {
    let rec = create_record("seq").expect("seq is registered");
    let mut inst = RecordInstance::new_boxed("S".to_string(), rec);
    inst.put_common_field("PREC", EpicsValue::Short(prec))
        .expect("seq declares PREC");
    inst
}

/// `caget -s` — the DBR_STRING form, which is where a precision shows.
fn dbr_string_of(inst: &RecordInstance, field: &str) -> String {
    let snap = inst
        .snapshot_for_field_with(field, LinkBacking::none())
        .expect("field exists");
    let bytes = epics_base_rs::types::encode_dbr(0, &snap).expect("DBR_STRING encodes");
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn precision_of(inst: &RecordInstance, field: &str) -> Option<i16> {
    inst.snapshot_for_field_with(field, LinkBacking::none())
        .expect("field exists")
        .precision()
}

/// Arm three, the lead's trigger: `field(PREC,"3") field(DO1,"1.23456789")`,
/// `caget -s S.DO1` — C prints `1.235`.
#[test]
fn a_do_field_over_a_constant_dol_takes_the_records_own_prec() {
    let mut inst = seq_with_prec(3);
    inst.record
        .put_field("DO1", EpicsValue::Double(1.234_567_89))
        .expect("DO1 is a value slot");
    assert_eq!(dbr_string_of(&inst, "DO1"), "1.235");
    assert_eq!(precision_of(&inst, "DO1"), Some(3));
}

/// Arm one: `DLYn` answers `seqDLYprecision = 2`, whatever PREC says.
#[test]
fn a_dly_field_answers_the_literal_not_prec() {
    let inst = seq_with_prec(3);
    assert_eq!(precision_of(&inst, "DLY1"), Some(2));
    assert_eq!(dbr_string_of(&inst, "DLY1"), "0.00");
    // Raising PREC must not move it.
    let inst = seq_with_prec(6);
    assert_eq!(precision_of(&inst, "DLY1"), Some(2));
}

/// Arm three again, on a field that is neither: every DBF_DOUBLE slot of the
/// record takes PREC, and `recGblGetPrec` only clamps a float, so the seed IS
/// the answer.
#[test]
fn the_fall_through_arm_covers_every_other_double_field() {
    let inst = seq_with_prec(4);
    for f in ["DO0", "DO7", "DOF"] {
        assert_eq!(precision_of(&inst, f), Some(4), "{f}");
    }
}

/// C's DBF gate (`dbAccess.c:387-394`) drops DBR_PRECISION for a field that is
/// neither float nor double — `seq.VAL` is DBF_LONG and `SELN` DBF_USHORT.
#[test]
fn an_integer_field_of_the_same_record_supplies_no_precision() {
    let inst = seq_with_prec(3);
    assert_eq!(precision_of(&inst, "VAL"), None);
    assert_eq!(precision_of(&inst, "SELN"), None);
    assert_eq!(dbr_string_of(&inst, "VAL"), "0");
}
