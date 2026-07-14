//! `FTVL` IS the VAL buffer's element type — for every `menu(menuFtype)` choice.
//!
//! C keeps the menu index and re-derives the element type at each use
//! (`dbValueSize(prec->ftvl)`). The port had four hand-written copies of that
//! derivation in `waveform.rs`, none of which named STRING or ENUM and two of which
//! collapsed USHORT onto SHORT and ULONG onto LONG; every one fell through to
//! DOUBLE for the indices it did not name. So a `field(FTVL,"USHORT")` waveform
//! served `DBF_SHORT`, and the DECLARED default — index 0, STRING, since none of
//! the four `.dbd`s gives FTVL an `initial()` — came out DOUBLE.
//!
//! Measured on the compiled C `softIoc`, one bare record of each array type:
//!
//! ```text
//! $ caget -t P:WF.FTVL   -> STRING          $ cainfo P:WF   -> Native data type: DBF_STRING
//! $ caget -t P:AAI.FTVL  -> STRING          $ cainfo P:AAI  -> Native data type: DBF_STRING
//! $ caget -t P:AAO.FTVL  -> STRING          $ cainfo P:AAO  -> Native data type: DBF_STRING
//! $ caget -t P:SA.FTVL   -> STRING          $ cainfo P:SA   -> Native data type: DBF_STRING
//! ```
//!
//! The boundaries are the menu itself: every choice, in and out of range.

use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::record::{Ftype, MENU_FTYPE, RecordInstance};
use epics_base_rs::types::{DbFieldType, EpicsValue};

const ARRAY_KINDS: [&str; 4] = ["waveform", "aai", "aao", "subArray"];

fn instance(record_type: &str) -> RecordInstance {
    let rec = create_record(record_type).expect("record type is registered");
    RecordInstance::new_boxed(format!("T:{record_type}"), rec)
}

/// The declaration, not the port's taste: no `initial()` ⇒ index 0 ⇒ STRING.
#[test]
fn r21_an_unset_ftvl_is_the_declared_string() {
    for rt in ARRAY_KINDS {
        let inst = instance(rt);
        assert_eq!(
            inst.record.get_field("FTVL"),
            Some(EpicsValue::Short(0)),
            "{rt}: FTVL has no initial() — the calloc'd record is menu index 0"
        );
        assert_eq!(
            inst.client_field_value("VAL")
                .expect("VAL resolves")
                .dbr_type(),
            DbFieldType::String,
            "{rt}: a bare record's VAL is a STRING buffer (C: cainfo -> DBF_STRING)"
        );
    }
}

/// Every choice in the menu types the buffer, the served VAL, and the field table
/// the same way — no index falls through to DOUBLE, and USHORT/ULONG keep their
/// own storage instead of collapsing onto SHORT/LONG.
#[test]
fn r21_every_menu_ftype_choice_types_the_val_buffer() {
    for rt in ARRAY_KINDS {
        for (index, label) in MENU_FTYPE.iter().enumerate() {
            let want = Ftype::from_index(index as i16)
                .expect("a menu index is a choice")
                .element_type();
            let mut inst = instance(rt);
            inst.record
                .put_field("FTVL", EpicsValue::Short(index as i16))
                .unwrap_or_else(|e| panic!("{rt}: FTVL={label} must be accepted: {e:?}"));

            assert_eq!(
                inst.record.get_field("FTVL"),
                Some(EpicsValue::Short(index as i16)),
                "{rt}.FTVL={label}: the index a client reads back is the one it wrote"
            );
            // The NATIVE element type, not the CA-promoted one (`dbr_type` folds
            // UCHAR onto CHAR, USHORT onto LONG, ULONG onto DOUBLE — that
            // promotion is `ca_wire_type`'s job and is the same in C).
            assert_eq!(
                inst.client_field_value("VAL")
                    .expect("VAL resolves")
                    .db_field_type(),
                want,
                "{rt}.VAL under FTVL={label} must be served as its element type"
            );
            let desc = inst
                .record
                .field_list()
                .iter()
                .find(|f| f.name == "VAL")
                .expect("VAL is in the field table");
            assert_eq!(
                desc.dbf_type, want,
                "{rt}: the field table and the buffer must agree under FTVL={label}"
            );
        }
    }
}

/// An index past the menu is not a choice. C `dbPutStringNum`/`putStringMenu`
/// answer `S_db_badChoice` and store nothing, leaving the field at its previous
/// value — an FTVL whose `dbValueSize` is undefined must never reach the record.
#[test]
fn r21_an_ftvl_past_the_menu_is_rejected_and_changes_nothing() {
    for rt in ARRAY_KINDS {
        let mut inst = instance(rt);
        inst.record
            .put_field("FTVL", EpicsValue::Short(5))
            .expect("LONG is a choice");
        inst.record
            .put_field("VAL", EpicsValue::LongArray(vec![1, 2, 3]))
            .expect("a LONG buffer takes a long array");

        for bad in [MENU_FTYPE.len() as i16, 99, -1] {
            assert!(
                inst.record
                    .put_field("FTVL", EpicsValue::Short(bad))
                    .is_err(),
                "{rt}: FTVL={bad} names no menu choice and must fail the put"
            );
        }
        assert_eq!(
            inst.record.get_field("FTVL"),
            Some(EpicsValue::Short(5)),
            "{rt}: a rejected put leaves FTVL alone"
        );
        assert_eq!(
            inst.client_field_value("VAL")
                .expect("VAL resolves")
                .dbr_type(),
            DbFieldType::Long,
            "{rt}: a rejected put leaves the buffer alone"
        );
    }
}
