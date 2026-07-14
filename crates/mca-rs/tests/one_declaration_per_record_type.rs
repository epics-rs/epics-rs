//! Invariant: **a record type has exactly one field declaration, and it is its
//! `.dbd`** — and that holds across a crate boundary too.
//!
//! `epics-base-rs` carries this test for the record types ITS vendored `.dbd`
//! set covers. `mca` is not in that set: base's generator never saw it, so
//! `FieldDeclaration::field_list` cannot resolve it from base's table and asks
//! the record itself ([`Record::declared_fields`]). Without a generated table
//! that answer would be a hand-written `FieldDesc` list — the record type's
//! ONLY declaration, and the second-declaration defect family this whole line
//! of work deletes.
//!
//! The `.dbd` is vendored into this crate (byte-identical to upstream),
//! `tools/dbd-codegen` generates a table into it (`targets.rs`), and
//! `declared_fields` returns THAT. This file is the ratchet that keeps it so:
//! the table a record serves must BE the generated one — not an equal one, the
//! same one — so a hand-written table cannot come back, and the generator's own
//! drift gate then holds it to the `.dbd`.

use epics_base_rs::server::record::{FieldDeclaration, Record};
use mca_rs::record::dbd_generated::{RECORD_TYPES, record_fields};

/// Every record type this crate ships, and the `.dbd` name it declares itself
/// by. A record type whose `.dbd` is vendored but which is missing here fails
/// `every_vendored_record_type_is_walked` below.
fn records() -> Vec<Box<dyn Record>> {
    vec![Box::new(mca_rs::record::McaRecord::default())]
}

#[test]
fn a_record_type_is_declared_only_by_its_vendored_dbd() {
    for record in records() {
        let record_type = record.record_type();
        let generated = record_fields(record_type)
            .unwrap_or_else(|| panic!("{record_type}: no table generated from its .dbd"));

        // The declaration served IS the generated one — same table, not merely
        // an equal one. A hand-written table that happened to agree today would
        // still be a second declaration free to drift tomorrow.
        assert!(
            std::ptr::eq(record.field_list(), generated),
            "{record_type}: field_list() must serve the table generated from its .dbd"
        );
        assert!(
            std::ptr::eq(Record::declared_fields(record.as_ref()), generated),
            "{record_type}: declared_fields() must return the generated table itself"
        );

        // And base does not ALSO declare it: two generated tables for one record
        // type would put the resolver's fall-through order in charge of which
        // declaration wins, which is the ordering-by-luck this invariant removes.
        assert!(
            epics_base_rs::server::record::dbd_generated::record_fields(record_type).is_none(),
            "{record_type} is declared by BOTH this crate's .dbd and epics-base-rs's"
        );
    }
}

/// The `.dbd` set and the record set are the same set. Neither a vendored
/// record type with no `impl Record`, nor a record type this crate ships that
/// quietly stopped being covered by the vendored `.dbd`.
#[test]
fn every_vendored_record_type_is_walked() {
    let mut walked: Vec<&str> = records().iter().map(|r| r.record_type()).collect();
    walked.sort_unstable();
    let mut vendored: Vec<&str> = RECORD_TYPES.to_vec();
    vendored.sort_unstable();
    assert_eq!(walked, vendored);
}
