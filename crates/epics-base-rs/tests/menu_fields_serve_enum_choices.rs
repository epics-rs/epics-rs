//! A `menu()` field must be SERVED as `DBR_ENUM` carrying its choice labels.
//!
//! Every expectation here is what the compiled C softIoc actually answers —
//! measured on `/home/stevek/work/epics-base/bin/linux-x86_64/softIoc`, not read
//! off the `.dbd`:
//!
//! ```text
//! $ cainfo T:AI.LINR   ->  Native data type: DBF_ENUM
//! $ caget  T:AI.LINR   ->  NO CONVERSION
//! $ caget  T:AI.SIMM   ->  NO
//! $ caget  T:AI.SIMS   ->  NO_ALARM
//! ```
//!
//! Before the field tables came from the spec, the port had these typed
//! `DBF_SHORT` in its hand-written table and served a bare `0`. A client asking
//! for the string got a number.

use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// What a CA client sees on create-channel: the native type is read from the
/// VALUE, not from the descriptor (`epics-ca-rs/src/server/tcp.rs`).
fn served(inst: &RecordInstance, field: &str) -> EpicsValue {
    inst.client_field_value(field)
        .unwrap_or_else(|| panic!("{field} is not served at all"))
}

/// The choice labels a client reads back — the port's `get_enum_strs`.
fn enum_strs(inst: &RecordInstance, field: &str) -> Vec<String> {
    let snap = inst
        .snapshot_for_field(field)
        .unwrap_or_else(|| panic!("{field} has no snapshot"));
    let enums = snap
        .enums
        .unwrap_or_else(|| panic!("{field}: snapshot carries no enum strings"));
    enums.strings.iter().map(|c| c.to_string()).collect()
}

/// The type a CA client sees on create-channel, and the label it reads back.
#[test]
fn ai_menu_fields_serve_dbr_enum_with_the_choices_c_serves() {
    let inst = RecordInstance::new("T:AI".to_string(), AiRecord::new(0.0));

    for (field, index, label) in [
        ("LINR", 0u16, "NO CONVERSION"),
        ("SIMM", 0, "NO"),
        ("SIMS", 0, "NO_ALARM"),
    ] {
        let v = served(&inst, field);
        assert_eq!(
            v.db_field_type(),
            DbFieldType::Enum,
            "ai.{field}: C serves DBF_ENUM, port serves {:?}",
            v.db_field_type()
        );
        assert_eq!(v, EpicsValue::Enum(index), "ai.{field} index");

        let choices: Vec<String> = enum_strs(&inst, field);
        assert_eq!(
            choices.get(index as usize).map(String::as_str),
            Some(label),
            "ai.{field}: C's caget answers {label:?}; port's choices are {choices:?}"
        );
    }
}

/// The menu is the FIELD's, not a global keyed on the field name. `ai.SIMM` is
/// `menu(menuSimm)` — three choices, NO/YES/RAW — while the integer and string
/// records' SIMM is the two-choice `menuYesNo`. A table keyed on the name alone
/// cannot tell them apart.
#[test]
fn ai_simm_serves_its_own_three_choice_menu() {
    let inst = RecordInstance::new("T:AI".to_string(), AiRecord::new(0.0));
    assert_eq!(
        enum_strs(&inst, "SIMM"),
        ["NO", "YES", "RAW"],
        "ai.SIMM is menu(menuSimm)"
    );
}

/// A put of the choice INDEX still lands: the record stores the index as a
/// `Short`, and serving the field as an `Enum` must not make its `Short` put
/// arm unreachable.
#[test]
fn a_put_to_a_menu_field_still_lands_as_the_stored_index() {
    let mut inst = RecordInstance::new("T:AI".to_string(), AiRecord::new(0.0));
    inst.record.put_field("SIMM", EpicsValue::Short(1)).unwrap();
    assert_eq!(served(&inst, "SIMM"), EpicsValue::Enum(1));
    assert_eq!(enum_strs(&inst, "SIMM")[1], "YES");
}
