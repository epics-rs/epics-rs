//! `pp(TRUE)` field sets for the `dbPutField` processing gate.
//!
//! C `dbPutField` (`dbAccess.c:1263-1268`) re-processes a record on a CA/PVA
//! put **only** when the put target is the `PROC` field, or when the put
//! field is declared `process_passive` (`pp(TRUE)`) in the record's DBD
//! **and** the record's `SCAN` is `Passive`:
//!
//! ```c
//! if (paddr->pfield == &precord->proc ||
//!     (pfldDes->process_passive && precord->scan == 0 &&
//!      dbrType < DBR_PUT_ACKT)) {
//!     ... dbProcess(precord); ...
//! }
//! ```
//!
//! `process_passive` is read from the record type's one field declaration,
//! [`FieldDeclaration::field_list`](super::FieldDeclaration::field_list), as C
//! reads it from `dbFldDes`: base's generated table, or the generated table a
//! downstream crate's record hands back from `declared_fields`. A type with
//! no declaration (a synthetic test record) has no `pp` field and processes
//! on `PROC` only.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

use super::FieldDesc;

/// The `pp(TRUE)` field names of `fields`, a record type's declaration.
///
/// Every put asks this, so each declaration's set is built once, keyed by the
/// table's address: declarations are `'static`, one per record type.
pub(crate) fn pp_fields_of(fields: &'static [FieldDesc]) -> &'static [&'static str] {
    static SETS: LazyLock<RwLock<HashMap<usize, &'static [&'static str]>>> =
        LazyLock::new(Default::default);
    if fields.is_empty() {
        return &[];
    }
    let key = fields.as_ptr() as usize;
    if let Some(set) = SETS.read().expect("pp set cache poisoned").get(&key) {
        return set;
    }
    let mut sets = SETS.write().expect("pp set cache poisoned");
    sets.entry(key).or_insert_with(|| {
        fields
            .iter()
            .filter(|f| f.pp)
            .map(|f| f.name)
            .collect::<Vec<_>>()
            .leak()
    })
}

#[cfg(test)]
mod tests {
    use super::pp_fields_of;
    use crate::server::record::dbd_generated::record_fields;

    fn pp_fields_for(record_type: &str) -> &'static [&'static str] {
        pp_fields_of(record_fields(record_type).unwrap())
    }

    /// A type with no declaration has no `pp` field: PROC-only.
    #[test]
    fn no_declaration_is_proc_only() {
        assert!(pp_fields_of(&[]).is_empty());
    }

    /// The set is the declaration's `pp(TRUE)` fields.
    #[test]
    fn modeled_type_with_pp_is_non_empty() {
        assert!(
            pp_fields_for("ai").contains(&"VAL"),
            "ai declares VAL pp(TRUE)"
        );
        assert!(
            pp_fields_for("asyn").contains(&"AOUT"),
            "asyn declares AOUT pp(TRUE); a put to AOUT must still process"
        );
    }

    /// A declared type with no `pp` field (`event`, `histogram`) is PROC-only.
    #[test]
    fn modeled_empty_type_returns_empty() {
        assert!(pp_fields_for("event").is_empty());
        assert!(pp_fields_for("histogram").is_empty());
    }
}
