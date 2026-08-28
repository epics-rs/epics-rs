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

use epics_base_rs::server::record::{FieldDeclaration, RecordInstance};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

mod module_records;

/// What a CA client sees on create-channel. The CA server reads the native type
/// off this value (`epics-ca-rs/src/server/tcp.rs`), and the value is the stored
/// one projected onto the field's DECLARED type — so the type it reads is the
/// declaration, and it cannot drift from the bytes the GET path then serves.
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

/// INVARIANT: a field that has menu choices is SERVED as `DBR_ENUM`.
///
/// A menu field is declared in two halves — the TYPE in a field table, the
/// CHOICES in `Record::menu_field_choices` or the field's own `menu()` — and
/// nothing but this check stops them disagreeing. A field with choices served
/// as a bare `DBR_SHORT` is a contradiction a client cannot resolve: it is
/// handed an index and no labels to read it with.
///
/// Asserted on the EFFECTIVE declaration — `declared_field_type`, the type that
/// actually reaches the wire — because that is where the invariant is
/// load-bearing. This is the check that caught the 24 downstream fields
/// (`scaler` `CONT`/`PCNT`/`G{n}`/`D{n}`, `epid` `SMSL`/`FMOD`/`FBOP`,
/// `timestamp` `TST`, `throttle`'s six, `motor`'s twelve) whose hand tables
/// said `Short` while their records answered with choice labels; those crates
/// have no generated table, so the hand table WAS what reached the wire.
///
/// It does not catch a contradiction in a hand table that the generated table
/// shadows — `waveform.SIMM`, `swait.OOPT`, `sseq.WAIT1`.. and 27 others still
/// say `Short` in tables `field_desc` no longer consults for the type. Those are
/// inert (the `.dbd` table wins, and `served_native_type_is_declared` pins every
/// one of them against the C IOC), but they are not *correct*, and they are the
/// property of the hand-table migration: four of those record types get their
/// table from `#[derive(EpicsRecord)]`, which types each field from its Rust
/// struct member, so there is no per-field type to fix without a macro
/// attribute. The type-level close — a `FieldDesc` constructor that sets type
/// and choices together, making the contradiction unrepresentable — is not
/// built.
#[test]
fn menu_choices_are_served_as_dbr_enum() {
    use epics_base_rs::server::record::dbd_generated::RECORD_TYPES;

    let mut contradictory = Vec::new();
    for record_type in RECORD_TYPES {
        // Through the module-record fixture: a `continue` here silently
        // dropped the seven types outside `stdRecords.dbd` from the sweep.
        let rec = module_records::create_any(record_type)
            .unwrap_or_else(|e| panic!("{record_type}: create_record failed: {e}"));
        let inst = RecordInstance::new_boxed(format!("T:{record_type}"), rec);
        for desc in inst.record.field_list() {
            let has_choices =
                desc.menu.is_some() || inst.record.menu_field_choices(desc.name).is_some();
            if !has_choices {
                continue;
            }
            let served = inst.declared_field_type(desc.name);
            if served != Some(DbFieldType::Enum) {
                contradictory.push(format!(
                    "{record_type}.{}: has menu choices but is served as {served:?}, \
                     so a client gets a bare index and no labels to read it with",
                    desc.name
                ));
            }
        }
    }
    assert!(
        contradictory.is_empty(),
        "{} field(s) are served at a type that contradicts their own menu:\n  {}",
        contradictory.len(),
        contradictory.join("\n  ")
    );
}
