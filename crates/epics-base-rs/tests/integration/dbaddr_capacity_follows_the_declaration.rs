//! Which fields carry a channel capacity is the `.dbd`'s answer, not a record's.
//!
//! In C only a `special(SPC_DBADDR)` field reaches `cvt_dbaddr` at all, so
//! `paddr->no_elements` is set for exactly those fields and for no others.
//! `FieldDeclaration::field_native_count` reads that population from
//! `field_list()` — the one declaration every consumer already goes through —
//! and only then asks the record for the number via `Record::dbaddr_capacity`.
//!
//! This is a ratchet, not a regression test: it fails if a record type ever
//! goes back to hand-listing its own array fields and that list drifts from
//! the `.dbd` the generator produced.

use epics_base_rs::server::record::{FieldDeclaration, Record, Special};
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::EpicsValue;

#[test]
fn a_capacity_is_advertised_for_exactly_the_declared_dbaddr_fields() {
    let mut r = AcalcoutRecord::new();
    r.put_field("NELM", EpicsValue::ULong(8)).unwrap();

    let declared: Vec<&str> = r
        .field_list()
        .iter()
        .filter(|d| d.special == Special::DbAddr)
        .map(|d| d.name)
        .collect();
    assert_eq!(declared.len(), 14, "aCalcoutRecord.dbd declares 14 of them");

    let advertised: Vec<&str> = r
        .field_list()
        .iter()
        .map(|d| d.name)
        .filter(|name| r.field_native_count(name).is_some())
        .collect();

    assert_eq!(advertised, declared);
}
