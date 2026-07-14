//! Invariant: **a record type has exactly one field declaration, and it is its
//! `.dbd`** — and that holds across a crate boundary too.
//!
//! `epics-base-rs` carries this test for the record types ITS vendored `.dbd`
//! set covers. The record types in this crate are not in that set: base's
//! generator never saw them, so `FieldDeclaration::field_list` cannot resolve
//! them from base's table and asks the record itself
//! ([`Record::declared_fields`]). That used to be the hole — the answer was a
//! hand-written `FieldDesc` table, the record type's ONLY declaration, and it
//! had never been checked against the `.dbd` it claimed to transcribe. It was
//! wrong: `epid`'s `IP`/`DTP`/`FBOP` were declared
//! `special(SPC_NOMOD)` where `epidRecord.dbd` declares them writable — a
//! `caput` to any of the three was REJECTED — `epid.FBON`/`throttle.OV` were
//! `DBF_SHORT` where the `.dbd` says `DBF_MENU` (served as `DBR_SHORT`, not
//! `DBR_ENUM`), and `timestamp.RVAL` was `DBF_LONG` where the `.dbd` says
//! `DBF_ULONG`.
//!
//! The `.dbd` is now vendored into this crate, `tools/dbd-codegen` generates a
//! table into it (`targets.rs`), and `declared_fields` returns THAT. This file
//! is the ratchet that keeps it so: the table a record serves must BE the
//! generated one — not an equal one, the same one — so a hand-written table
//! cannot come back, and the generator's own drift gate then holds it to the
//! `.dbd`.

use epics_base_rs::server::record::{FieldDeclaration, Record};
use std_rs::records::dbd_generated::{RECORD_TYPES, record_fields};

/// Every record type this crate ships, and the `.dbd` name it declares itself
/// by. A record type whose `.dbd` is vendored but which is missing here fails
/// `every_vendored_record_type_is_walked` below — vendoring a `.dbd` without
/// wiring the record to it is exactly the gap this file exists to catch.
fn records() -> Vec<Box<dyn Record>> {
    vec![
        Box::new(std_rs::records::epid::EpidRecord::default()),
        Box::new(std_rs::records::throttle::ThrottleRecord::default()),
        Box::new(std_rs::records::timestamp::TimestampRecord::default()),
    ]
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
