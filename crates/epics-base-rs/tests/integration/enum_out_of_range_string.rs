//! What an enum-valued field renders as when its value is PAST its choices.
//!
//! There is no single answer, and assuming there was one is the defect. C picks
//! the converter by DBF class (`dbFastLinkConv.c`, the scalar table `dbGet`
//! takes for a one-element read — which is every CA get of a scalar field):
//!
//! * `DBF_MENU` → `cvt_menu_st:1590-1596` — *"Convert out-of-range values to
//!   numeric strings"*: `epicsSnprintf(to, MAX_STRING_SIZE, "%u", *from)`.
//! * `DBF_ENUM` → `cvt_e_st_get:1548-1563` — the record's `get_enum_str` rset,
//!   whose out-of-range answer is that record type's sentinel.
//! * `DBF_DEVICE` → `cvt_device_st:1616-1620` — a record type with no device
//!   support at all is *"Valid"* and renders empty.
//!
//! `SSCN` is the live case, not a hypothetical: its declared initial is 65535,
//! past every menu, so EVERY record on a C IOC serves it as a number. Measured:
//!
//! ```text
//! $ caget -t D:AI.SSCN     -> 65535        $ caget -t -n D:AI.SSCN -> -1
//! $ caget -t D:AI.SCAN     -> Passive      $ caget -t -n D:AI.SCAN -> 0
//! ```

use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::types::EpicsValue;

fn instance(record_type: &str) -> RecordInstance {
    let rec = create_record(record_type).expect("record type is registered");
    RecordInstance::new_boxed(format!("T:{record_type}"), rec)
}

fn dbr_string_of(inst: &RecordInstance, field: &str) -> String {
    let snap = inst.snapshot_for_field(field).expect("field exists");
    let bytes = epics_base_rs::types::encode_dbr(0, &snap).expect("DBR_STRING encodes");
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// The `.dbd`'s own out-of-range menu index: `SSCN` initial 65535.
#[test]
fn r21_menu_index_past_the_choices_renders_as_its_number() {
    let inst = instance("ai");
    assert_eq!(dbr_string_of(&inst, "SSCN"), "65535");
}

/// The in-range case is unchanged — the number is the LAST resort, not the rule.
#[test]
fn r21_menu_index_inside_the_choices_renders_its_label() {
    let inst = instance("ai");
    assert_eq!(dbr_string_of(&inst, "SCAN"), "Passive");
}

/// A `DBF_ENUM` VAL does NOT take the numeric branch: C goes to the record's
/// `get_enum_str`, which answers with the record type's sentinel.
#[test]
fn r21_enum_val_past_the_states_renders_the_sentinel_not_a_number() {
    let mut inst = instance("mbbi");
    inst.record.put_field("VAL", EpicsValue::Enum(20)).unwrap();
    assert_eq!(dbr_string_of(&inst, "VAL"), "Illegal Value");

    let mut inst = instance("bi");
    inst.record.put_field("VAL", EpicsValue::Enum(7)).unwrap();
    assert_eq!(dbr_string_of(&inst, "VAL"), "Illegal_Value");
}
