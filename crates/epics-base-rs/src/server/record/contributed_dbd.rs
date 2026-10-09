//! `.dbd` tables a downstream crate generated for its own record types.
//!
//! C has one `pdbbase` for every loaded `.dbd`, so `dbDumpDevice epid` in a
//! softIoc that loaded `stdSupport.dbd` walks epid like any base type. Base's
//! generated tables cannot name a downstream crate's types, so the crate hands
//! its generated [`ContributedDbd`] here when it is wired into an IOC, as
//! `dbLoadDatabase` would add it to `pdbbase`.

use std::sync::RwLock;

use super::FieldDesc;

/// A downstream crate's generated `dbd_generated::DBD`: the lookups its
/// `dbd_generated` module exports, for the record types it declares.
pub struct ContributedDbd {
    /// Every record type the crate's `.dbd` declares.
    pub record_types: &'static [&'static str],
    /// C's `papFldDes` order per record type, internals included.
    pub record_declaration_order: fn(&str) -> Option<&'static [&'static str]>,
    /// The record-own field declarations per record type.
    pub record_fields: fn(&str) -> Option<&'static [FieldDesc]>,
    /// C's `pdbRecordType->no_prompt` per record type.
    pub record_no_prompt: fn(&str) -> Option<u16>,
    /// Every `menu()` as `(name, identifiers, choices)`.
    pub menus: &'static [(
        &'static str,
        &'static [&'static str],
        &'static [&'static str],
    )],
}

static CONTRIBUTED: RwLock<Vec<&'static ContributedDbd>> = RwLock::new(Vec::new());

/// Add `dbd` to the process's record-type universe, after base's and after
/// every crate registered earlier (C's `recordTypeList` load order).
/// Registering the same table again is a no-op.
pub fn register_contributed_dbd(dbd: &'static ContributedDbd) {
    let mut list = CONTRIBUTED
        .write()
        .expect("contributed-dbd registry lock poisoned");
    if !list.iter().any(|d| std::ptr::eq(*d, dbd)) {
        list.push(dbd);
    }
}

fn contributed() -> Vec<&'static ContributedDbd> {
    CONTRIBUTED
        .read()
        .expect("contributed-dbd registry lock poisoned")
        .clone()
}

/// C's `recordTypeList`: base's record types and each contributed crate's, in
/// `strcmp` order. `dbRecordtypeHead` appends in load order, and the expanded
/// `.dbd` an IOC loads lists its record types name-sorted, so a contributed
/// type sits among base's (epid between dfanout and event), not after them.
pub(crate) fn record_types() -> Vec<&'static str> {
    let mut types: Vec<&'static str> = super::dbd_generated::RECORD_TYPES.to_vec();
    for dbd in contributed() {
        types.extend_from_slice(dbd.record_types);
    }
    types.sort_unstable();
    types.dedup();
    types
}

/// The first answer to `lookup` from base's tables, then each contributed
/// crate's: base and a downstream crate never declare the same record type.
fn first<T>(
    record_type: &str,
    base: fn(&str) -> Option<T>,
    contributed_lookup: impl Fn(&ContributedDbd) -> Option<T>,
) -> Option<T> {
    base(record_type).or_else(|| {
        contributed()
            .into_iter()
            .find_map(|d| contributed_lookup(d))
    })
}

/// `record_declaration_order` over base's and every contributed table.
pub(crate) fn record_declaration_order(record_type: &str) -> Option<&'static [&'static str]> {
    first(
        record_type,
        super::dbd_generated::record_declaration_order,
        |d| (d.record_declaration_order)(record_type),
    )
}

/// `record_fields` over base's and every contributed table.
pub(crate) fn record_fields(record_type: &str) -> Option<&'static [FieldDesc]> {
    first(record_type, super::dbd_generated::record_fields, |d| {
        (d.record_fields)(record_type)
    })
}

/// `record_no_prompt` over base's and every contributed table.
pub(crate) fn record_no_prompt(record_type: &str) -> Option<u16> {
    first(record_type, super::dbd_generated::record_no_prompt, |d| {
        (d.record_no_prompt)(record_type)
    })
}

/// C's `menuList`: every menu of base and each contributed crate, the first
/// declaration of a name winning (`dbMenuHead`), in the descending `strcmp`
/// order `dbMenuBody` inserts in.
pub(crate) fn menus() -> Vec<(
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
)> {
    let mut menus = super::dbd_generated::MENUS.to_vec();
    for dbd in contributed() {
        for &menu in dbd.menus {
            if !menus.iter().any(|m| m.0 == menu.0) {
                menus.push(menu);
            }
        }
    }
    menus.sort_by(|a, b| b.0.cmp(a.0));
    menus
}
