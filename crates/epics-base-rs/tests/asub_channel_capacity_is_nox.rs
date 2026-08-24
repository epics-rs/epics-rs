//! An `aSub` channel is `NOx`/`NOVx` wide however few elements it holds.
//!
//! C `aSubRecord.c::cvt_dbaddr` (`:476-495`) reads the capacity per channel:
//!
//! ```c
//! /* A..U */
//! paddr->no_elements = (&prec->noa )[offset];
//! /* VALA..VALU */
//! paddr->no_elements = (&prec->nova)[offset];
//! ```
//!
//! while `get_array_info` serves `(&prec->nea)[..]` / `(&prec->neva)[..]` —
//! the elements a link or a subroutine actually delivered. `initFields`
//! allocates each cell at its full `NOx` and never resizes it, so the capacity
//! is a property of the declaration and the served count is a property of the
//! last delivery.
//!
//! The port advertised nothing, so each channel was sized from what it
//! currently held. `ca_element_count` is settled once at create-channel time,
//! so a client that connected while a short value was in the cell could never
//! see a longer one.
//!
//! Boundaries: a partly filled input channel, a partly filled output channel,
//! two channels with different capacities on one record, and a one-element
//! channel.

use epics_base_rs::server::record::{FieldDeclaration, Record};
use epics_base_rs::server::records::asub_record::ASubRecord;
use epics_base_rs::types::EpicsValue;

fn served_len(r: &ASubRecord, field: &str) -> usize {
    match r.get_field(field) {
        Some(EpicsValue::DoubleArray(v)) => v.len(),
        Some(EpicsValue::Double(_)) => 1,
        other => panic!("{field} reads as {other:?}"),
    }
}

/// An input channel `A` eight elements wide, holding three.
#[test]
fn an_input_channel_advertises_noa_not_nea() {
    let mut r = ASubRecord::default();
    r.put_field("NOA", EpicsValue::Long(8)).unwrap();
    r.put_field("A", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
        .unwrap();

    assert_eq!(r.get_field("NEA").unwrap(), EpicsValue::Long(3));
    assert_eq!(served_len(&r, "A"), 3);
    assert_eq!(r.field_native_count("A"), Some(8));
}

/// The output half of the same hook, with its own capacity field.
#[test]
fn an_output_channel_advertises_nova_not_neva() {
    let mut r = ASubRecord::default();
    r.put_field("NOVA", EpicsValue::Long(6)).unwrap();
    r.put_field("VALA", EpicsValue::DoubleArray(vec![7.0, 8.0]))
        .unwrap();

    assert_eq!(r.get_field("NEVA").unwrap(), EpicsValue::Long(2));
    assert_eq!(served_len(&r, "VALA"), 2);
    assert_eq!(r.field_native_count("VALA"), Some(6));
}

/// The capacity is per channel, not per record — C indexes `(&prec->noa)` by
/// the field's own offset.
#[test]
fn each_channel_carries_its_own_capacity() {
    let mut r = ASubRecord::default();
    r.put_field("NOA", EpicsValue::Long(8)).unwrap();
    r.put_field("NOB", EpicsValue::Long(4)).unwrap();
    r.put_field("NOVC", EpicsValue::Long(12)).unwrap();

    assert_eq!(r.field_native_count("A"), Some(8));
    assert_eq!(r.field_native_count("B"), Some(4));
    assert_eq!(r.field_native_count("VALC"), Some(12));
}

/// `NOx` floors at 1 (`aSubRecord.c:189-190`), so a scalar channel advertises
/// one element rather than nothing.
#[test]
fn a_scalar_channel_advertises_one_element() {
    let mut r = ASubRecord::default();
    r.put_field("NOA", EpicsValue::Long(0)).unwrap();

    assert_eq!(r.field_native_count("A"), Some(1));
}

/// The `A..U` and `VALA..VALU` cells are the record's `special(SPC_DBADDR)`
/// fields; the counts and links beside them are not.
#[test]
fn only_the_channel_cells_carry_a_capacity() {
    let r = ASubRecord::default();

    for field in ["NOA", "NEA", "NOVA", "NEVA", "FTA", "INPA", "OUTA", "SNAM"] {
        assert_eq!(r.field_native_count(field), None, "{field} is not a cell");
    }
}
