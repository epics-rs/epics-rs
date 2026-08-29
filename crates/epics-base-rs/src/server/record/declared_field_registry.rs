//! The by-NAME half of [`crate::server::record::FieldDeclaration::field_list`].
//!
//! [`field_desc_of`](super::record_instance::field_desc_of) answers "what did
//! the `.dbd` declare for this field" from a record in hand: the generated
//! table when `dbd_generated` covers the type, the record's own
//! [`Record::declared_fields`](super::Record::declared_fields) when it does
//! not, then `dbCommon`. Some callers hold only the record type's NAME —
//! [`crate::types::dbf_link_class`] is the one that matters, because
//! `check_link_assignment` reaches it from the db loader with nothing but a
//! type string. For those, the generated table is reachable and the record's
//! own table is not: a downstream type's declarations live in that crate's own
//! `dbd_generated` (`motor-rs`'s `MOTOR_FIELDS`, `optics-rs`'s
//! `TABLE_FIELDS`), which `epics-base-rs` cannot name.
//!
//! This registry closes that half. Every funnel that takes a
//! [`RecordFactory`](crate::server::RecordFactory) snapshots the type's
//! declarations here, so a by-name lookup sees exactly what a by-instance one
//! would. The alternative — reconstructing the declaration from the field's
//! spelling — is what this replaces: it was wrong for 86 declared link fields
//! across the workspace, in both directions.

use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;
use std::sync::{OnceLock, RwLock};

use super::FieldDesc;

static DECLARED: OnceLock<RwLock<HashMap<String, &'static [FieldDesc]>>> = OnceLock::new();

fn registry() -> &'static RwLock<HashMap<String, &'static [FieldDesc]>> {
    DECLARED.get_or_init(|| RwLock::new(HashMap::new()))
}

/// The record types that reached [`register_declared_fields`] with an EMPTY
/// table.
///
/// `Record::declared_fields` has a trait default of `&[]`, so "declares
/// nothing" is what a record type inherits by FORGETTING rather than by
/// deciding, and an empty table has nothing to store — the registration used
/// to end in a bare early return and leave no trace. Everything keyed on the
/// declaration then answers as if the type had no such field:
/// [`crate::types::dbf_link_class`] says its `INP` is not a link, so
/// `PvDatabase::link_field_texts` drops it and the record stops joining its
/// targets' lock sets and stops being opened at iocInit. Nothing errors; the
/// links are simply not there.
///
/// Keeping the name is what makes that assertable —
/// [`record_types_declaring_nothing`] is the guard's input. It is not an
/// error by itself: the synthetic record types the tests define legitimately
/// declare nothing. It is an error for a type an application can serve, and
/// that is the boundary the guard draws.
#[cfg(test)]
static DECLARED_NOTHING: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();

#[cfg(test)]
fn declared_nothing() -> &'static RwLock<HashSet<String>> {
    DECLARED_NOTHING.get_or_init(|| RwLock::new(HashSet::new()))
}

/// Every record type this process registered with an empty declaration, sorted.
///
/// See [`DECLARED_NOTHING`]. A record type an application can serve must never
/// appear here; a test's synthetic type may.
#[cfg(test)]
pub fn record_types_declaring_nothing() -> Vec<String> {
    let mut names: Vec<String> = declared_nothing()
        .read()
        .expect("declared-field registry lock poisoned")
        .iter()
        .cloned()
        .collect();
    names.sort();
    names
}

/// Keep the name of a type that registered nothing. A no-op outside this
/// crate's own tests, which are the only reader — reaching it from another
/// crate needs a `pub use` in `record/mod.rs`.
#[cfg(test)]
fn note_declared_nothing(record_type: &str) {
    declared_nothing()
        .write()
        .expect("declared-field registry lock poisoned")
        .insert(record_type.to_string());
}

#[cfg(not(test))]
fn note_declared_nothing(_record_type: &str) {}

/// Record `record_type`'s `.dbd` field declarations under its name.
///
/// Called by every path that registers a record factory, with the table the
/// factory's record reports from `declared_fields`. A type `dbd_generated`
/// already covers gains nothing from being registered and loses nothing by it:
/// [`declared_field`] asks the generated table first.
pub fn register_declared_fields(record_type: &str, fields: &'static [FieldDesc]) {
    if fields.is_empty() {
        // Nothing to store, but the attempt is the fact worth keeping — see
        // [`DECLARED_NOTHING`].
        note_declared_nothing(record_type);
        return;
    }
    registry()
        .write()
        .expect("declared-field registry lock poisoned")
        .insert(record_type.to_string(), fields);
}

/// `record_type`'s OWN declarations — the generated table when
/// `dbd_generated` covers the type, otherwise whatever its factory
/// registered. `dbCommon` is NOT included; see [`declared_fields`] for the
/// whole record type.
///
/// `None` for a type nothing has declared, which is the same test
/// `record_type_field_exists` makes before it reports a field missing: an
/// unregistered type has no declarations to be missing from.
pub fn declared_field_table(record_type: &str) -> Option<&'static [FieldDesc]> {
    match super::dbd_generated::record_fields(record_type) {
        Some(generated) => Some(generated),
        None => registry()
            .read()
            .expect("declared-field registry lock poisoned")
            .get(record_type)
            .copied(),
    }
}

/// Every declaration `record_type` has, in C's `papFldDes` order: `dbCommon`
/// first, then the record's own.
///
/// The order is load-bearing wherever a scan reports the FIRST field that
/// meets a test — C's `dbFirstField`/`dbNextField` walk `papFldDes`, and every
/// record `.dbd` opens with `include "dbCommon.dbd"`, so `NAME` is index 0 and
/// the record's own `VAL` follows `dbCommon`'s last field. The db loader's
/// field suggestion breaks ties with `>` against the running best
/// (`dbLexRoutines.c:1371`), which makes the earliest field win a tie.
pub fn declared_fields(record_type: &str) -> impl Iterator<Item = &'static FieldDesc> {
    super::dbd_generated::DB_COMMON_FIELDS
        .iter()
        .chain(declared_field_table(record_type).unwrap_or(&[]).iter())
}

/// The `.dbd` declaration of `record_type`'s `field`, by name.
///
/// Same resolution order as
/// `field_desc_of` — the record's own
/// table shadows `dbCommon`, and the generated table is the record's own table
/// for every type `dbd_generated` covers. `None` when nothing declares the
/// field: an unregistered record type, or a virtual field (`RTYP`, `TIME`)
/// that C answers from dbStaticLib rather than from a `dbFldDes`.
pub fn declared_field(record_type: &str, field: &str) -> Option<&'static FieldDesc> {
    let named = |t: &'static [FieldDesc]| t.iter().find(|f| f.name.eq_ignore_ascii_case(field));
    declared_field_table(record_type)
        .and_then(named)
        .or_else(|| named(super::dbd_generated::DB_COMMON_FIELDS))
}

#[cfg(test)]
mod tests {
    //! **Every record type an application can serve declares its own fields.**
    //!
    //! The guard exists because the failure is silent. `Record::declared_fields`
    //! defaults to `&[]`, so a new record type declares nothing by forgetting,
    //! and nothing errors — the type loads, processes and serves. What it loses
    //! is every answer keyed on the declaration, and the one that bites is
    //! links: `dbf_link_class` says the type's `INP` is not a link field, so
    //! `PvDatabase::link_field_texts` never yields it, and the record silently
    //! stops joining its target's lock set and stops having the link opened at
    //! iocInit.

    use super::*;
    use crate::server::record::dbd_generated::RECORD_TYPES;

    /// Every record type this crate can serve carries its own non-empty table.
    #[test]
    fn every_serveable_record_type_declares_its_own_fields() {
        let missing: Vec<&str> = RECORD_TYPES
            .iter()
            .copied()
            .filter(|rt| declared_field_table(rt).is_none_or(<[FieldDesc]>::is_empty))
            .collect();
        assert!(
            missing.is_empty(),
            "record types with no declaration of their own: {missing:?}"
        );

        let forgot: Vec<&&str> = RECORD_TYPES
            .iter()
            .filter(|rt| record_types_declaring_nothing().iter().any(|n| n == *rt))
            .collect();
        assert!(
            forgot.is_empty(),
            "record types that registered an EMPTY declaration: {forgot:?}"
        );
    }

    /// The mechanism the guard reads, and the cost of being in it.
    ///
    /// A type that declares nothing still answers for `dbCommon` — that is why
    /// the loss is invisible from most angles. `INP` is not a `dbCommon` field,
    /// so it is where the silence shows.
    #[test]
    fn a_type_that_declares_nothing_is_recorded_and_loses_its_own_fields() {
        register_declared_fields("zzForgotToDeclare", &[]);

        assert!(
            record_types_declaring_nothing()
                .iter()
                .any(|n| n == "zzForgotToDeclare"),
            "an empty registration must leave a trace"
        );
        assert!(declared_field_table("zzForgotToDeclare").is_none());
        // dbCommon still answers, which is the half that makes it look fine.
        assert!(declared_field("zzForgotToDeclare", "FLNK").is_some());
        // Its own fields do not, and `INP` is a link — so every link-keyed
        // consumer drops it.
        assert!(declared_field("zzForgotToDeclare", "INP").is_none());
        assert_eq!(
            crate::types::dbf_link_class("zzForgotToDeclare", "INP"),
            None
        );
    }
}
