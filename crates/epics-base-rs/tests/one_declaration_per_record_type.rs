//! Invariant: **a record type has exactly one field declaration.**
//!
//! C has exactly one: the `.dbd`, read at runtime. The port compiles the
//! vendored `.dbd`s into `dbd_generated`, and until now several record types
//! also carried a hand-written `FieldDesc` table for the same fields. Both were
//! live. The wire happened to come out right only because `field_desc_of`
//! consulted the generated one first — a runtime ordering, not a guarantee —
//! and every consumer that reached for `Record::field_list()` directly (`dbpr`,
//! the `dbpf` typo hint, `motor`'s field gate, `QSRV`'s VAL introspection) read
//! the hand-written one instead. That is how `waveform.FTVL` was declared
//! `DBF_SHORT` with no `menu()` while `waveformRecord.dbd:83-89` declares it
//! `DBF_MENU`/`menu(menuFtype)`, and how `sseq.ABORTING` was declared
//! `special(SPC_NOMOD)` while `sseqRecord.dbd:820-824` declares it
//! `special(SPC_MOD)` — writable.
//!
//! The second source is now unrepresentable rather than merely unread:
//! `field_list()` lives on `FieldDeclaration`, which is blanket-implemented for
//! every `Record` and so cannot be overridden, and it serves the generated
//! table for every record type the `.dbd` set covers, never falling through to
//! `Record::hand_field_list`. A record type cannot *supply* a declaration; it
//! can only be *asked* for one.
//!
//! The boundary this file walks is the fall-through: covered types must resolve
//! to the generated table (and carry no hand table at all), uncovered types must
//! resolve to their hand table.

use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::record::dbd_generated::{RECORD_TYPES, record_fields};
use epics_base_rs::server::record::{FieldDeclaration, Record};

/// Covered side of the boundary: every record type with a vendored `.dbd` is
/// served that `.dbd`'s table, and hands over no second table of its own.
#[test]
fn a_dbd_covered_record_type_is_declared_only_by_its_dbd() {
    for &record_type in RECORD_TYPES {
        let record = create_record(record_type)
            .unwrap_or_else(|_| panic!("{record_type}: vendored .dbd but no record impl"));
        let generated = record_fields(record_type)
            .unwrap_or_else(|| panic!("{record_type} is in RECORD_TYPES but has no field table"));

        // The declaration served IS the generated one — same table, not merely
        // an equal one.
        assert!(
            std::ptr::eq(record.field_list(), generated),
            "{record_type}: field_list() must serve the table generated from its .dbd"
        );

        // And there is no second table to contradict it.
        assert!(
            Record::hand_field_list(record.as_ref()).is_empty(),
            "{record_type} has a vendored .dbd, so it must not also hand-write a field table"
        );
    }
    assert!(
        RECORD_TYPES.len() >= 40,
        "only {} record types walked — the .dbd set emptied",
        RECORD_TYPES.len()
    );
}

/// Uncovered side of the boundary: a record type the `.dbd` set does NOT cover
/// falls through to its hand-written table, which is then its one declaration.
///
/// The downstream Tier-3 record types (`motor`, `table`, `scaler`, `epid`,
/// `throttle`, `timestamp`) live here, in their own crates. This synthetic one
/// stands in for them so the fall-through has a test inside `epics-base-rs`.
#[test]
fn a_record_type_with_no_dbd_falls_through_to_its_hand_table() {
    use epics_base_rs::error::CaResult;
    use epics_base_rs::server::record::FieldDesc;
    use epics_base_rs::types::{DbFieldType, EpicsValue};

    struct NoDbdRecord;

    static HAND: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

    impl Record for NoDbdRecord {
        fn record_type(&self) -> &'static str {
            "noSuchRecordTypeInAnyDbd"
        }
        fn get_field(&self, _name: &str) -> Option<EpicsValue> {
            None
        }
        fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
            Ok(())
        }
        fn hand_field_list(&self) -> &'static [FieldDesc] {
            HAND
        }
    }

    assert!(
        record_fields("noSuchRecordTypeInAnyDbd").is_none(),
        "the premise of this test is that no .dbd covers this record type"
    );
    assert!(std::ptr::eq(NoDbdRecord.field_list(), HAND));
}

/// A record type that overrides nothing has no declaration at all — it does not
/// silently inherit `dbCommon` or an empty-but-plausible table. This is what
/// makes `#[derive(EpicsRecord)]` safe to strip of its field emission: a derived
/// record with no `.dbd` gets `&[]`, which is loud, rather than a table invented
/// from its Rust struct members, which was quiet and wrong.
#[test]
fn a_record_type_that_declares_nothing_has_no_fields() {
    use epics_base_rs::error::CaResult;
    use epics_base_rs::types::EpicsValue;

    struct UndeclaredRecord;

    impl Record for UndeclaredRecord {
        fn record_type(&self) -> &'static str {
            "alsoNotInAnyDbd"
        }
        fn get_field(&self, _name: &str) -> Option<EpicsValue> {
            None
        }
        fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
            Ok(())
        }
    }

    assert!(UndeclaredRecord.field_list().is_empty());
}
