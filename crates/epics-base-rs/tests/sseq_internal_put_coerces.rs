//! W10-A7 — `sseq`'s `put_field_internal` must end in the framework's
//! `put_field_internal_default`, so that an internal write it does not special-case
//! still gets the DBF-type coercion C's link layer performs.
//!
//! C reads a link into a record field with `dbGetLink(plink, DBF_<target>, pdest,
//! NULL, NULL)`: the request type is the TARGET FIELD's type, and `nRequest = NULL`
//! asks for exactly one element — so `dbGet` converts the source field at offset 0
//! and an array source lands its first element. `sseq`'s SELL link is read that way
//! into `SELN`, a `DBF_USHORT` (`sseqRecord.c`; `sseqRecord.dbd:40`).
//!
//! The port's override ended with `self.put_field(name, value)`, bypassing the
//! coercion layer entirely: every field the override did not special-case (i.e.
//! everything but `DOn`) reached the typed `put_field` arm raw. `SELN`'s arm rejects
//! an array outright, so an array-valued SELL source failed the internal write where
//! C takes element 0. `acalcout`'s override (`acalcout.rs:1672`) already routes
//! through the default; this makes `sseq` do the same.

use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::EpicsValue;

/// An array source into the scalar `SELN`: C's `dbGetLink` asks for one element and
/// converts offset 0, so the record sees 3. The port raised `TypeMismatch` and left
/// `SELN` at its previous value.
#[test]
fn an_array_source_into_seln_lands_element_zero() {
    let mut rec = SseqRecord::new();
    rec.put_field_internal("SELN", EpicsValue::DoubleArray(vec![3.0, 9.0]))
        .expect("an array source must reduce to element 0, not fail");
    assert_eq!(rec.get_field("SELN"), Some(EpicsValue::UShort(3)));
}

/// The same rule for the other scalar link target on the record: `DLY1` is
/// `DBF_DOUBLE`.
#[test]
fn an_array_source_into_a_scalar_field_lands_element_zero() {
    let mut rec = SseqRecord::new();
    rec.put_field_internal("DLY1", EpicsValue::DoubleArray(vec![0.25, 7.0]))
        .expect("an array source must reduce to element 0, not fail");
    assert_eq!(rec.get_field("DLY1"), Some(EpicsValue::Double(0.25)));
}

/// The `DOn` special case still owns its own path — a string DOL source is kept
/// byte-exact (that is what the override exists for), and routing everything else
/// through the default must not disturb it.
#[test]
fn the_don_string_capture_still_bypasses_the_default() {
    let mut rec = SseqRecord::new();
    rec.put_field_internal("DO1", EpicsValue::String("abc".into()))
        .expect("DO1 takes a string");
    assert_eq!(
        rec.get_field("STR1"),
        Some(EpicsValue::String("abc".into()))
    );
}
