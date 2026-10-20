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

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};

use super::DbLinkType;

/// What `dbDumpDevice` reads of a C `dset`: `number` entry points, which of
/// them besides `init` are non-NULL (slot indices, `report` = 0), whether its
/// `init` calls `devExtend`; and `init` itself, the one entry point the
/// build calls on the dset, whose presence is slot 1's. The fields are
/// private so [`Self::new`] is the only way to make one:
///
/// ```compile_fail
/// use epics_base_rs::server::record::Dset;
/// const _: Dset = Dset { number: 5, funcs: &[1, 4], extends: true, init: None };
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Dset {
    number: u8,
    funcs: &'static [u8],
    extends: bool,
    /// C `dset.init(int after)`: `initDevSup` calls it with `false` before
    /// `initHookAfterInitDevSup`, `finishDevSup` with `true` before
    /// `initHookAfterFinishDevSup` (`iocInit.c:441-480`), for every row of
    /// every record type whether or not a record uses it. C ignores its
    /// status, so it has none.
    init: Option<fn(after: bool)>,
}

impl Dset {
    /// A dset whose `funcs` may not name slot 1: `init` is [`Self::with_init`]'s,
    /// or, for `extends`, the `devExtend` C's `init` is.
    ///
    /// ```compile_fail
    /// use epics_base_rs::server::record::Dset;
    /// const _: Dset = Dset::new(5, &[1, 4], false);
    /// ```
    pub const fn new(number: u8, funcs: &'static [u8], extends: bool) -> Self {
        let mut i = 0;
        while i < funcs.len() {
            assert!(funcs[i] != 1, "slot 1 is `init`: give it with `with_init`");
            i += 1;
        }
        Self {
            number,
            funcs,
            extends,
            init: if extends { Some(dev_extend) } else { None },
        }
    }

    /// This dset with `init` in its init slot.
    pub const fn with_init(self, init: fn(after: bool)) -> Self {
        Self {
            init: Some(init),
            ..self
        }
    }

    pub const fn number(&self) -> u8 {
        self.number
    }

    pub const fn funcs(&self) -> &'static [u8] {
        self.funcs
    }

    pub const fn extends(&self) -> bool {
        self.extends
    }

    pub const fn init(&self) -> Option<fn(after: bool)> {
        self.init
    }
}

/// An `init` that is `if (pass == 0) devExtend(&dsxt)` and nothing else: C
/// can call `devExtend` only from `init`, and the port's dsxt is
/// [`Dset::extends`], so nothing is left to run.
fn dev_extend(_after: bool) {}

/// One `device(rectype, link_type, name, "choice")` line: C's `devSup`, a
/// row of the record type's `dbDeviceMenu`.
#[derive(Debug, Clone, Copy)]
pub struct DevSup {
    /// The dset symbol.
    pub name: &'static str,
    /// The `DTYP` string.
    pub choice: &'static str,
    pub link_type: DbLinkType,
    pub dset: Dset,
}

impl DevSup {
    /// `dbInitDevSup` gives every `CONSTANT` device the soft `dsxt`; any
    /// other has one only if its `init` calls `devExtend`.
    pub fn has_dsxt(&self) -> bool {
        self.link_type == DbLinkType::Constant || self.dset.extends()
    }

    /// The dset's `init_record` (slot 2) is non-NULL.
    pub fn has_init_record(&self) -> bool {
        self.dset.funcs().contains(&2)
    }

    /// The dset's `get_ioint_info` (slot 3) is non-NULL.
    pub fn has_get_ioint_info(&self) -> bool {
        self.dset.funcs().contains(&3)
    }
}

/// One contributed device menu: the `device()` lines a downstream crate's
/// `.dbd` declares for a record type — a generated `DEVICES_*` table.
type DeviceMenu = &'static [DevSup];

/// The contributed menus by record type, one entry per contributing crate in
/// registration order. Kept as a list (rather than a flattened `Vec`) so
/// re-registering the SAME menu is idempotent (see [`register_device_menu`]):
/// a second `IocApplication` built in one process, or a helper called twice,
/// must not double the menu.
///
/// An immutable snapshot that a registration replaces, leaking the old one
/// (registration is startup work), because [`dev_sup_of`] is asked on the
/// INP/OUT put under the record's write hold: one load, no lock, no
/// allocation.
type Contributed = Vec<(&'static str, Vec<DeviceMenu>)>;
static CONTRIBUTED: AtomicPtr<Contributed> = AtomicPtr::new(std::ptr::null_mut());

/// Serialises the registrations' copy-and-publish of [`CONTRIBUTED`]; no
/// reader takes it.
static PUBLISH: Mutex<()> = Mutex::new(());

fn contributed() -> &'static [(&'static str, Vec<DeviceMenu>)] {
    let menus = CONTRIBUTED.load(Ordering::Acquire);
    // SAFETY: a non-null pointer came from `Box::into_raw` and is never freed.
    unsafe { menus.as_ref() }.map_or(&[], Vec::as_slice)
}

/// The menus contributed for `record_type`, in registration order.
fn menus_of(record_type: &str) -> &'static [DeviceMenu] {
    contributed()
        .iter()
        .find(|(t, _)| *t == record_type)
        .map_or(&[], |(_, menus)| menus.as_slice())
}

/// Publish `contributed()` with each row of `rows` whose choice
/// `record_type`'s menu does not list yet appended, as C's `dbDevice`
/// (dbLexRoutines.c:843-846) skips a `device()` line for a choice already
/// loaded. The one writer of the menu; answers whether it appended any.
fn publish_rows(record_type: &str, rows: &'static [DevSup]) -> bool {
    let _publish = PUBLISH.lock().expect("device-menu registry lock poisoned");
    let declared = super::dbd_generated::devices(record_type).unwrap_or_default();
    let mut next = contributed().to_vec();
    let at = match next.iter().position(|(t, _)| *t == record_type) {
        Some(at) => at,
        None => {
            next.push((Box::leak(record_type.into()), Vec::new()));
            next.len() - 1
        }
    };
    let menus = &mut next[at].1;
    let before = menus.len();
    for row in rows {
        let listed = declared
            .iter()
            .chain(menus.iter().flat_map(|menu| menu.iter()))
            .any(|d| d.choice == row.choice);
        if !listed {
            menus.push(std::slice::from_ref(row));
        }
    }
    let added = menus.len() != before;
    if added {
        CONTRIBUTED.store(Box::into_raw(Box::new(next)), Ordering::Release);
    }
    added
}

/// Contribute a device-support DTYP menu for `record_type` — the choices a
/// downstream crate's device support adds to that record type's `DTYP` list,
/// in the crate's `.dbd` declaration order. Pass the generated
/// `devices(record_type)` table.
///
/// The choices are appended AFTER base's declared menu and after any menu
/// registered by an earlier call, matching C's load-order `dbDeviceMenu`
/// concatenation. A choice the menu already lists is skipped, so wiring the
/// same support twice in one process is safe.
pub fn register_device_menu(record_type: &str, menu: &'static [DevSup]) {
    publish_rows(record_type, menu);
}

/// A row only a loaded `.dbd`'s `device()` line added, and what its dset
/// name found. C's `dbDevice` stores just the dset NAME and `initDevSup`
/// finds the dset by it in the global registry (`iocInit.c:453`); here every
/// dset with support already has a row of its own, since each registration
/// brings one before the startup script runs, so the name finds that row or
/// nothing.
#[derive(Clone, Copy)]
struct LoadedRow {
    row: &'static DevSup,
    bound: Bound,
}

#[derive(Clone, Copy)]
enum Bound {
    /// The dset serves this record type under this choice.
    Choice(&'static str),
    /// The dset is registered for another record type. C binds it anyway and
    /// a record using the choice crashes in it; here the choice serves
    /// nothing.
    OtherType,
    /// No dset has the name: `device support %s not found`.
    Missing,
}

/// The loaded rows by record type in C's `recordTypeList` order (`strcmp`,
/// as [`super::record_types`]), each type's in `devList` order. Same publish
/// rule as [`CONTRIBUTED`]: [`served_choice`] is asked under the record's
/// hold.
type Loaded = BTreeMap<&'static str, Vec<LoadedRow>>;
static LOADED: AtomicPtr<Loaded> = AtomicPtr::new(std::ptr::null_mut());

fn loaded() -> Option<&'static Loaded> {
    // SAFETY: a non-null pointer came from `Box::into_raw` and is never freed.
    unsafe { LOADED.load(Ordering::Acquire).as_ref() }
}

/// A `device()` line from `dbLoadDatabase`: appended to `record_type`'s
/// menu as C's `dbDevice` appends it to `devList`, a choice already listed
/// skipped.
pub(crate) fn load_dbd_device(record_type: &str, link_type: DbLinkType, dset: &str, choice: &str) {
    let named = registered_dset(dset);
    let bound = match named {
        Some((rt, row)) if rt == record_type => Bound::Choice(served_choice(rt, row.choice)),
        Some(_) => Bound::OtherType,
        None => Bound::Missing,
    };
    let row: &'static DevSup = Box::leak(Box::new(DevSup {
        name: Box::leak(dset.into()),
        choice: Box::leak(choice.into()),
        link_type,
        dset: named.map_or(Dset::new(0, &[], false), |(_, row)| row.dset),
    }));
    if !publish_rows(record_type, std::slice::from_ref(row)) {
        return;
    }
    let _publish = PUBLISH.lock().expect("device-menu registry lock poisoned");
    let mut next = loaded().cloned().unwrap_or_default();
    next.entry(Box::leak(record_type.into()))
        .or_default()
        .push(LoadedRow { row, bound });
    LOADED.store(Box::into_raw(Box::new(next)), Ordering::Release);
}

/// C `registryDeviceSupportFind(name)`: the record type and row of the dset
/// called `name`, of any record type. A loaded row is no registration of its
/// own.
fn registered_dset(name: &str) -> Option<(&'static str, &'static DevSup)> {
    let loaded_rows: Vec<&DevSup> = loaded()
        .into_iter()
        .flat_map(|loaded| loaded.values().flatten())
        .map(|r| r.row)
        .collect();
    let declared = super::dbd_generated::DEVICE_MENU_RECORD_TYPES
        .iter()
        .flat_map(|&rt| {
            let rows = super::dbd_generated::devices(rt).unwrap_or_default();
            rows.iter().map(move |row| (rt, row))
        });
    let contributed = contributed().iter().flat_map(|(rt, menus)| {
        menus
            .iter()
            .flat_map(|menu| menu.iter())
            .map(move |row| (*rt, row))
    });
    declared
        .chain(contributed)
        .filter(|(_, row)| !loaded_rows.iter().any(|l| std::ptr::eq(*l, *row)))
        .find(|(_, row)| row.name == name)
}

/// The choice whose support serves `dtyp` on `record_type`: `dtyp` itself,
/// unless a loaded `device()` line gave an existing dset a new choice. An
/// empty DTYP is menu index 0, the first row's choice, as C's
/// `dbDTYPtoDevSup` takes `ellNth(&devList, dtyp+1)` (`dbAccess.c:1052-1054`).
pub(crate) fn served_choice<'a>(record_type: &str, dtyp: &'a str) -> &'a str {
    let dtyp = match dtyp {
        "" => dev_sup_of(record_type, "").map_or("", |row| row.choice),
        dtyp => dtyp,
    };
    loaded()
        .and_then(|loaded| loaded.get(record_type))
        .and_then(|rows| rows.iter().find(|r| r.row.choice == dtyp))
        .and_then(|r| match r.bound {
            Bound::Choice(choice) => Some(choice),
            Bound::OtherType | Bound::Missing => None,
        })
        .unwrap_or(dtyp)
}

/// Every row of every record type, in C's `recordTypeList` order (`strcmp`)
/// and each type's `devList` order, with whether its name found a dset.
fn dev_list() -> Vec<(&'static DevSup, bool)> {
    let mut types: Vec<&str> = super::dbd_generated::DEVICE_MENU_RECORD_TYPES
        .iter()
        .copied()
        .chain(contributed().iter().map(|(t, _)| *t))
        .collect();
    types.sort_unstable();
    types.dedup();
    let missing = |row: &DevSup| {
        loaded()
            .into_iter()
            .flat_map(|loaded| loaded.values().flatten())
            .any(|r| std::ptr::eq(r.row, row) && matches!(r.bound, Bound::Missing))
    };
    types
        .into_iter()
        .flat_map(|rt| {
            let declared = super::dbd_generated::devices(rt).unwrap_or_default();
            declared
                .iter()
                .chain(menus_of(rt).iter().flat_map(|menu| menu.iter()))
        })
        .map(|row| (row, !missing(row)))
        .collect()
}

/// C `initDevSup` (`iocInit.c:441-462`): every row's dset `init(0)`, and
/// `device support %s not found` for a loaded row whose name finds none.
pub(crate) fn init_dev_sup() {
    for (row, found) in dev_list() {
        if !found {
            crate::runtime::log::errlog_printf(&format!("device support {} not found\n", row.name));
        } else if let Some(init) = row.dset.init() {
            init(false);
        }
    }
}

/// C `finishDevSup` (`iocInit.c:464-481`): every row's dset `init(1)`.
pub(crate) fn finish_dev_sup() {
    for (row, _) in dev_list() {
        if let Some(init) = row.dset.init() {
            init(true);
        }
    }
}

/// The record types whose `.dbd` came with every `device()` line it has,
/// none included: C's `devList`, known even when empty. Same publish rule as
/// [`CONTRIBUTED`], for the same reader under the INP/OUT put hold.
static DEVLIST_TYPES: AtomicPtr<Vec<&'static str>> = AtomicPtr::new(std::ptr::null_mut());

fn devlist_types() -> &'static [&'static str] {
    let types = DEVLIST_TYPES.load(Ordering::Acquire);
    // SAFETY: a non-null pointer came from `Box::into_raw` and is never freed.
    unsafe { types.as_ref() }.map_or(&[], Vec::as_slice)
}

/// [`register_device_menu`] for a record type a contributed `.dbd` declares,
/// with all of that `.dbd`'s `device()` lines for it — `rows` may be empty,
/// which says the type has no device support at all.
pub fn register_dbd_devices(record_type: &'static str, rows: &'static [DevSup]) {
    publish_rows(record_type, rows);
    let _publish = PUBLISH.lock().expect("device-menu registry lock poisoned");
    if devlist_types().contains(&record_type) {
        return;
    }
    let mut next = devlist_types().to_vec();
    next.push(record_type);
    DEVLIST_TYPES.store(Box::into_raw(Box::new(next)), Ordering::Release);
}

/// C `ellCount(&rtyp->devList) == 0` where the port knows the whole list:
/// base's declared types and the [`register_dbd_devices`] ones.
pub(crate) fn has_no_device_support(record_type: &str) -> bool {
    super::dbd_generated::device_menu(record_type).is_none()
        && !has_contributed_device_menu(record_type)
        && (super::dbd_generated::RECORD_TYPES.contains(&record_type)
            || devlist_types().contains(&record_type))
}

/// The row a registered device support brings: appended to `record_type`'s
/// menu unless the menu already lists its choice, so every support that can
/// be bound is a `DTYP` choice by construction.
pub fn register_device_row(record_type: &str, row: &'static DevSup) {
    publish_rows(record_type, std::slice::from_ref(row));
}

/// The rows registered together with a device object, by address: the row
/// [`dev_sup_of`] finds for a choice is the one C binds, and a registration
/// whose choice the menu already listed added no row of its own.
static OBJECT_ROWS: AtomicPtr<Vec<&'static DevSup>> = AtomicPtr::new(std::ptr::null_mut());

fn object_rows() -> &'static [&'static DevSup] {
    let rows = OBJECT_ROWS.load(Ordering::Acquire);
    // SAFETY: a non-null pointer came from `Box::into_raw` and is never freed.
    unsafe { rows.as_ref() }.map_or(&[], Vec::as_slice)
}

/// [`register_device_row`] for a support that also registers the device
/// object serving `row`.
pub fn register_device_object_row(record_type: &str, row: &'static DevSup) {
    publish_rows(record_type, std::slice::from_ref(row));
    let _publish = PUBLISH.lock().expect("device-menu registry lock poisoned");
    if object_rows().iter().any(|r| std::ptr::eq(*r, row)) {
        return;
    }
    let mut next = object_rows().to_vec();
    next.push(row);
    OBJECT_ROWS.store(Box::into_raw(Box::new(next)), Ordering::Release);
}

/// Whether the row `dtyp` selects for `record_type` is served by a
/// registered device object, whatever its choice is called.
pub(crate) fn has_device_object(record_type: &str, dtyp: &str) -> bool {
    dev_sup_of(record_type, dtyp)
        .is_some_and(|row| object_rows().iter().any(|r| std::ptr::eq(*r, row)))
}

/// The contributed rows for `record_type`, flattened in registration order.
pub(crate) fn contributed_device_rows(record_type: &str) -> Vec<&'static DevSup> {
    menus_of(record_type)
        .iter()
        .flat_map(|menu| menu.iter())
        .collect()
}

/// The choices contributed for `record_type`, flattened in registration order,
/// or an empty `Vec` if none. The `device_choices` merge appends these after
/// the base-declared menu.
pub(crate) fn contributed_device_menu(record_type: &str) -> Vec<&'static str> {
    contributed_device_rows(record_type)
        .into_iter()
        .map(|row| row.choice)
        .collect()
}

/// Whether any choice was contributed for `record_type`.
pub(crate) fn has_contributed_device_menu(record_type: &str) -> bool {
    menus_of(record_type).iter().any(|menu| !menu.is_empty())
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

/// [`merged_device_menu`] without collecting it: what a put under the
/// record's hold may walk.
pub(crate) fn device_menu_choices(record_type: &str) -> impl Iterator<Item = &'static str> {
    super::dbd_generated::device_menu(record_type)
        .unwrap_or_default()
        .iter()
        .copied()
        .chain(
            menus_of(record_type)
                .iter()
                .flat_map(|menu| menu.iter().map(|row| row.choice)),
        )
}

/// C `prec->dset`'s row: the devSup `dtyp` names in `record_type`'s device
/// menu, declared rows first. An empty DTYP is menu index 0, as in C.
pub(crate) fn dev_sup_of(record_type: &str, dtyp: &str) -> Option<&'static DevSup> {
    let declared = super::dbd_generated::devices(record_type).unwrap_or_default();
    let contributed = menus_of(record_type).iter().flat_map(|menu| menu.iter());
    let mut rows = declared.iter().chain(contributed);
    if dtyp.is_empty() {
        rows.next()
    } else {
        rows.find(|row| row.choice == dtyp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn row(choice: &'static str) -> DevSup {
        DevSup {
            name: choice,
            choice,
            link_type: DbLinkType::Constant,
            dset: Dset::new(5, &[], false),
        }
    }

    /// The lock-free answer flips with the first non-empty menu for a type and
    /// for no other: an empty menu contributes no `device()` line, and a
    /// registered support's row is one.
    #[test]
    fn has_contributed_device_menu_follows_non_empty_registration() {
        static EMPTY: &[DevSup] = &[];
        static M: &[DevSup] = &[row("devC")];
        assert!(!has_contributed_device_menu("regtest_has"));
        register_device_menu("regtest_has", EMPTY);
        assert!(!has_contributed_device_menu("regtest_has"));
        register_device_menu("regtest_has", M);
        assert!(has_contributed_device_menu("regtest_has"));
        assert!(!has_contributed_device_menu("regtest_has_other"));
        static ROW: DevSup = row("devR");
        register_device_row("regtest_has_row", &ROW);
        assert!(has_contributed_device_menu("regtest_has_row"));
    }

    /// Registering a menu makes its choices visible, appended in registration
    /// order; a second identical registration is a no-op (not doubled).
    #[test]
    fn register_is_idempotent_per_slice() {
        // A record type name no other test touches, so the process-global
        // registry cannot cross-contaminate this assertion.
        static M: &[DevSup] = &[row("devA"), row("devB")];
        assert!(contributed_device_menu("regtest_idem").is_empty());
        register_device_menu("regtest_idem", M);
        register_device_menu("regtest_idem", M);
        assert_eq!(
            contributed_device_menu("regtest_idem"),
            vec!["devA", "devB"]
        );
    }

    /// A registered support adds its row once, and none for a choice the
    /// record type's menu already lists.
    #[test]
    fn register_device_row_adds_only_a_missing_choice() {
        static NEW: DevSup = row("regtest_row_new");
        static SOFT: DevSup = row("Soft Channel");
        register_device_row("ai", &SOFT);
        register_device_row("regtest_row", &NEW);
        register_device_row("regtest_row", &NEW);
        assert!(!contributed_device_menu("ai").contains(&"Soft Channel"));
        assert_eq!(
            contributed_device_menu("regtest_row"),
            vec!["regtest_row_new"]
        );
    }

    /// One choice registered through both entry points is one menu entry,
    /// whichever comes first.
    #[test]
    fn a_choice_registered_by_row_and_menu_is_listed_once() {
        static ROW: DevSup = row("regtest_both");
        static MENU: &[DevSup] = &[row("regtest_both"), row("regtest_both_2")];
        register_device_row("regtest_row_first", &ROW);
        register_device_menu("regtest_row_first", MENU);
        assert_eq!(
            contributed_device_menu("regtest_row_first"),
            vec!["regtest_both", "regtest_both_2"]
        );
        register_device_menu("regtest_menu_first", MENU);
        register_device_row("regtest_menu_first", &ROW);
        assert_eq!(
            contributed_device_menu("regtest_menu_first"),
            vec!["regtest_both", "regtest_both_2"]
        );
    }

    /// Distinct slices for one record type concatenate in registration order —
    /// two crates each adding device support to the same type.
    #[test]
    fn distinct_contributions_concatenate() {
        static A: &[DevSup] = &[row("a1")];
        static B: &[DevSup] = &[row("b1"), row("b2")];
        register_device_menu("regtest_concat", A);
        register_device_menu("regtest_concat", B);
        assert_eq!(
            contributed_device_menu("regtest_concat"),
            vec!["a1", "b1", "b2"]
        );
    }
}
