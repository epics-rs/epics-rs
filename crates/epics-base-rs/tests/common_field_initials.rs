//! Every `dbCommon` field starts at the value its `.dbd` declares.
//!
//! `dbCommon.dbd` gives each field an `initial("…")` or gives it none, in which
//! case C zero-initialises the record (`dbStaticLib` calloc + `initial` pass).
//! The port models dbCommon as a hand-written `CommonFields::default()`, so a
//! default picked by hand rather than read off the declaration is invisible
//! until a client reads the field — which is how a fresh record served
//! `ASG = "DEFAULT"` where every C IOC serves the empty string.
//!
//! This is the whole table, not a sample: the generated `DB_COMMON_FIELDS`
//! carries each field's declared `initial`, so the invariant is checkable for
//! all of them at once.

use epics_base_rs::server::access_security::{AccessLevel, parse_acf};
use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::server::record::dbd_generated::DB_COMMON_FIELDS;

fn instance(record_type: &str) -> RecordInstance {
    let rec = create_record(record_type).expect("record type is registered");
    RecordInstance::new_boxed(format!("T:{record_type}"), rec)
}

/// The field's value as a client reads it (`caget -t -n`), textually.
fn value_text(inst: &RecordInstance, field: &str) -> Option<String> {
    let v = inst.client_field_value(field)?;
    Some(match v {
        epics_base_rs::types::EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        epics_base_rs::types::EpicsValue::Char(n) => n.to_string(),
        epics_base_rs::types::EpicsValue::UChar(n) => n.to_string(),
        epics_base_rs::types::EpicsValue::Short(n) => n.to_string(),
        epics_base_rs::types::EpicsValue::Enum(n) => n.to_string(),
        epics_base_rs::types::EpicsValue::Long(n) => n.to_string(),
        epics_base_rs::types::EpicsValue::UInt64(n) => n.to_string(),
        epics_base_rs::types::EpicsValue::Int64(n) => n.to_string(),
        epics_base_rs::types::EpicsValue::Double(n) => n.to_string(),
        other => format!("{other:?}"),
    })
}

/// The declared `initial()` is TEXT; a menu field's is a LABEL. Compare through
/// the same converter a `.db` load would: the field's own choice list.
fn matches_initial(inst: &RecordInstance, field: &str, initial: &str, got: &str) -> bool {
    if got == initial {
        return true;
    }
    if let (Ok(a), Ok(b)) = (got.parse::<f64>(), initial.parse::<f64>()) {
        return a == b;
    }
    // A menu initial names its choice; the port stores the index.
    inst.snapshot_for_field(field)
        .and_then(|s| s.enums)
        .is_some_and(|e| {
            e.strings
                .iter()
                .position(|c| c.as_str_lossy() == initial)
                .is_some_and(|i| got == i.to_string())
        })
}

/// A field the `.dbd` gives no `initial()` starts ZERO — C callocs the record.
/// `NAME` is the one field C fills from outside the declaration (`dbStaticLib`
/// writes the record's name into it at load).
#[test]
fn r21_a_common_field_with_no_declared_initial_starts_zero() {
    let inst = instance("ai");
    let mut bad = Vec::new();
    for f in DB_COMMON_FIELDS {
        if f.initial.is_some() || f.name == "NAME" {
            continue;
        }
        let Some(got) = value_text(&inst, f.name) else {
            continue;
        };
        let zero = got.is_empty() || got.parse::<f64>() == Ok(0.0);
        if !zero {
            bad.push(format!("{}: no declared initial, port {got:?}", f.name));
        }
    }
    assert!(bad.is_empty(), "dbCommon defaults off the .dbd:\n{bad:#?}");
}

#[test]
fn r21_every_common_field_starts_at_its_declared_initial() {
    let inst = instance("ai");
    let mut bad = Vec::new();
    for f in DB_COMMON_FIELDS {
        let Some(initial) = f.initial else { continue };
        let Some(got) = value_text(&inst, f.name) else {
            bad.push(format!(
                "{}: no value (declared initial {initial:?})",
                f.name
            ));
            continue;
        };
        if !matches_initial(&inst, f.name, initial, &got) {
            bad.push(format!("{}: declared {initial:?}, port {got:?}", f.name));
        }
    }
    assert!(bad.is_empty(), "dbCommon defaults off the .dbd:\n{bad:#?}");
}

/// The other half of the ASG rule: the FIELD is empty, but the record is still a
/// member of the DEFAULT group — C `asAddMemberPvt` (asLibRoutines.c:884-919)
/// resolves an empty or unknown group name to the always-present DEFAULT. The
/// port must not read the raw field at an access-security call site, which is
/// why [`CommonFields::access_group`] exists.
#[test]
fn r21_an_empty_asg_is_evaluated_against_the_default_group() {
    let cfg = parse_acf(
        r#"
ASG(DEFAULT) { RULE(1, READ) }
ASG(RW)      { RULE(1, WRITE) }
"#,
    )
    .expect("acf parses");

    let mut inst = instance("ai");
    assert_eq!(inst.common.asg, "", "a record names no group by default");
    assert_eq!(
        cfg.check_access(inst.common.access_group(), "host", "user"),
        AccessLevel::Read,
        "the empty ASG must evaluate against DEFAULT, not against nothing"
    );

    inst.common.asg = "RW".to_string();
    assert_eq!(
        cfg.check_access(inst.common.access_group(), "host", "user"),
        AccessLevel::ReadWrite,
        "a named group is evaluated as itself"
    );
}
