//! Device-support DTYP menus contributed at RUNTIME by a downstream crate.
//!
//! C's `dbDeviceMenu` for a record type is the list of every `device()` line
//! the loaded `.dbd` set declares for it, and a record's `DTYP` field is an
//! index into that list. A C fat `softIoc` that loads `asyn.dbd` therefore
//! serves `mbbo.DTYP` as `["Soft Channel", "Raw Soft Channel",
//! "Async Soft Channel", "asynInt32", "asynUInt32Digital"]` — base's own three
//! plus the two asyn adds.
//!
//! Base's build-time table ([`super::dbd_generated::device_menu`]) carries only
//! the `device()` lines vendored into `epics-base-rs/dbd`, which cannot include
//! asyn's: `epics-base-rs` must not depend on `asyn-rs`. So the asyn menus flow
//! the other way — `asyn-rs` generates them from its own vendored `.dbd` (same
//! `dbd-codegen` path) and registers them here at startup. [`RecordInstance`'s
//! `device_choices`](super::record_instance::RecordInstance::device_choices)
//! merges them after the base-declared choices, so the served `DTYP` list
//! matches C.
//!
//! This is the [`super::super::db_loader::register_record_type`] pattern applied
//! to device menus: a process-global registry a downstream crate contributes to
//! and the serve path reads. The menu is per record TYPE, not per record — in C
//! too, loading `asyn.dbd` extends the `dbDeviceMenu` of every `ai`, whether or
//! not that particular record binds an asyn DTYP.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// The contributed menus, keyed by record type. Each value is the list of
/// `&'static` choice slices registered for that type, in registration order —
/// one slice per contributing crate. Kept as a list (rather than a flattened
/// `Vec`) so re-registering the SAME slice is idempotent (see
/// [`register_device_menu`]): a second `IocApplication` built in one process, or
/// a helper called twice, must not double the menu.
static CONTRIBUTED: OnceLock<RwLock<HashMap<String, Vec<&'static [&'static str]>>>> =
    OnceLock::new();

fn registry() -> &'static RwLock<HashMap<String, Vec<&'static [&'static str]>>> {
    CONTRIBUTED.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Contribute a device-support DTYP menu for `record_type` — the choices a
/// downstream crate's device support adds to that record type's `DTYP` list,
/// in the crate's `.dbd` declaration order.
///
/// The choices are appended AFTER base's declared menu and after any menu
/// registered by an earlier call, matching C's load-order `dbDeviceMenu`
/// concatenation. Registering a slice already present for this record type
/// (by content) is a no-op, so wiring the same support twice in one process is
/// safe.
///
/// The caller is `asyn_rs::adapter::register_asyn_device_menus`, which walks
/// asyn's generated `DEVICE_MENU_RECORD_TYPES`.
pub fn register_device_menu(record_type: &str, choices: &'static [&'static str]) {
    let mut map = registry()
        .write()
        .expect("device-menu registry lock poisoned");
    let list = map.entry(record_type.to_string()).or_default();
    if !list.contains(&choices) {
        list.push(choices);
    }
}

/// The choices contributed for `record_type`, flattened in registration order,
/// or an empty `Vec` if none. The `device_choices` merge appends these after
/// the base-declared menu.
pub(crate) fn contributed_device_menu(record_type: &str) -> Vec<&'static str> {
    match CONTRIBUTED.get() {
        Some(lock) => lock
            .read()
            .expect("device-menu registry lock poisoned")
            .get(record_type)
            .map(|list| {
                list.iter()
                    .flat_map(|slice| slice.iter().copied())
                    .collect()
            })
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

/// The full DTYP menu for `record_type` — the static `device()` declarations
/// (base's build-time `dbd`) followed by every runtime-contributed device
/// support, in C `dbDeviceMenu` load order — or an empty `Vec` when the type
/// has no device menu at all.
///
/// The single source of "which DTYP names are valid for this record type",
/// consulted by BOTH the read/announce side
/// ([`RecordInstance::device_choices`](super::record_instance::RecordInstance::device_choices))
/// and the CA-put validation ([`super::coerce_put_value`]'s DTYP branch), so a
/// client can PUT exactly the DTYP names it can READ. `device_choices` layers
/// the None-vs-empty distinction and the current-DTYP entry on top; the put
/// path needs only the menu itself.
pub(crate) fn merged_device_menu(record_type: &str) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    if let Some(declared) = super::dbd_generated::device_menu(record_type) {
        names.extend_from_slice(declared);
    }
    names.extend(contributed_device_menu(record_type));
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Registering a menu makes its choices visible, appended in registration
    /// order; a second identical registration is a no-op (not doubled).
    #[test]
    fn register_is_idempotent_per_slice() {
        // A record type name no other test touches, so the process-global
        // registry cannot cross-contaminate this assertion.
        static M: &[&str] = &["devA", "devB"];
        assert!(contributed_device_menu("regtest_idem").is_empty());
        register_device_menu("regtest_idem", M);
        register_device_menu("regtest_idem", M);
        assert_eq!(
            contributed_device_menu("regtest_idem"),
            vec!["devA", "devB"]
        );
    }

    /// Distinct slices for one record type concatenate in registration order —
    /// two crates each adding device support to the same type.
    #[test]
    fn distinct_contributions_concatenate() {
        static A: &[&str] = &["a1"];
        static B: &[&str] = &["b1", "b2"];
        register_device_menu("regtest_concat", A);
        register_device_menu("regtest_concat", B);
        assert_eq!(
            contributed_device_menu("regtest_concat"),
            vec!["a1", "b1", "b2"]
        );
    }
}
