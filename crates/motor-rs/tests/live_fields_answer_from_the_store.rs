//! A declared field the record STORES is answered from that store, never from
//! the `.dbd` `initial(...)`.
//!
//! `RecordInstance::resolve_field` tries `Record::get_field` first and falls
//! through to `declared_default` — the field's `initial(...)`, or a type-zero
//! — for a field the record models no storage for. That fallback is right for
//! a storeless field and silently wrong for a stored one: it answers, so the
//! channel works, and it answers the same value forever.
//!
//! `LSPG` and `PP` are the two motor fields C writes at runtime that had no
//! `motor_get_field` arm. The boundary is the store against the initial:
//! while the store still holds the `initial` the fallback's answer is
//! indistinguishable from the real one, which is why this went unseen. Each
//! case therefore moves the store OFF the initial first.
//!
//! Read through `client_field_value`, not `get_field` — that is the projection
//! a CA GET serves, so the assertion is on the byte a `caget` returns.

use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::types::EpicsValue;
use motor_rs::flags::SpmgMode;
use motor_rs::record::MotorRecord;

fn instance_of(rec: MotorRecord) -> RecordInstance {
    RecordInstance::new("M".to_string(), rec)
}

/// `motorRecord.dbd:495-501` — `field(LSPG,DBF_MENU) initial("3")`, and C
/// syncs it to SPMG at `motorRecord.cc:1859`. A record that has acted on a
/// Pause must report Pause, not the Go it was loaded with.
#[test]
fn lspg_answers_the_store_once_it_leaves_the_initial() {
    let mut rec = MotorRecord::default();
    assert_eq!(
        rec.internal.lspg,
        SpmgMode::Go,
        "the store starts on the .dbd initial"
    );
    rec.internal.lspg = SpmgMode::Pause;

    let inst = instance_of(rec);
    assert_eq!(
        inst.client_field_value("LSPG"),
        Some(EpicsValue::Enum(SpmgMode::Pause as u16)),
        "LSPG answered the .dbd initial instead of the store"
    );
}

/// The other side of the same boundary: with the store still ON the initial,
/// the answer is the initial — so this case passes with or without an arm and
/// is here to pin that the arm did not change the loaded value.
#[test]
fn lspg_on_the_initial_answers_the_initial() {
    let inst = instance_of(MotorRecord::default());
    assert_eq!(
        inst.client_field_value("LSPG"),
        Some(EpicsValue::Enum(SpmgMode::Go as u16))
    );
}

/// `motorRecord.dbd:664-669` — `field(PP,DBF_SHORT) initial("0")`, armed at
/// fifteen sites in `motorRecord.cc` and consumed at motion completion.
#[test]
fn pp_answers_the_store_once_it_leaves_the_initial() {
    let mut rec = MotorRecord::default();
    assert!(!rec.internal.pp, "the store starts on the .dbd initial");
    rec.internal.pp = true;

    let inst = instance_of(rec);
    assert_eq!(
        inst.client_field_value("PP"),
        Some(EpicsValue::Short(1)),
        "PP answered the .dbd initial instead of the store"
    );
}

#[test]
fn pp_on_the_initial_answers_the_initial() {
    let inst = instance_of(MotorRecord::default());
    assert_eq!(inst.client_field_value("PP"), Some(EpicsValue::Short(0)));
}

/// A field the record deliberately stores nothing for keeps answering through
/// the framework fallback — the arms added for LSPG/PP must not have turned
/// the storeless six into record-owned cells.
#[test]
fn a_storeless_field_still_answers_from_the_declaration() {
    let inst = instance_of(MotorRecord::default());
    // CARD: `initial(...)` absent, DBF_SHORT -> the declared type's zero,
    // which is also C's answer for the INST_IO OUT motor-rs uses.
    assert_eq!(inst.client_field_value("CARD"), Some(EpicsValue::Short(0)));
    // LOCK: `initial("NO")` against menuYesNo -> index 0.
    assert_eq!(inst.client_field_value("LOCK"), Some(EpicsValue::Enum(0)));
}
