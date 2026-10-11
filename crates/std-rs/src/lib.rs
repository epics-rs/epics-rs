//! # C reference pins
//!
//! Every `file.c:NNN` citation in this crate resolves at the tree and revision
//! below, not at whatever that tree's working copy holds today. These trees are
//! checked out on local branches here and run ahead of their pins.
//!
//! | tree | pinned revision |
//! | --- | --- |
//! | `std` | `R3-6-4` |
//! | `epics-base` | `R7.0.10` |
//!
//! **Resolve by symbol at the pin; the line is a hint.** Find the named
//! function, struct, macro or field first, and treat the line number as a hint
//! that has to land inside that construct. Three cases follow:
//!
//! 1. Construct at the pin, line lands in it — the citation is exact. A
//!    reference checkout ahead of the pin will disagree; that disagreement is
//!    the checkout's, not the citation's.
//! 2. Construct at the pin, line lands outside it — line drift. Keep the
//!    symbol and move the line to the pin's.
//! 3. Construct absent at the pin — the citation means code added after it,
//!    and is NOT moved onto the pin, where it would point at lines that do not
//!    exist. It names the revision it means inline, beside the line span: the
//!    upstream PR and commit, and that both are later than the pin this table
//!    gives. `epics-libcom-rs` already carries that form.
//!
//! Every pin above passes `git merge-base --is-ancestor <pin> origin/<default>`
//! in its own tree, which is the test a pin has to meet. A `git describe`
//! string names an exact commit and is worth as much as a tag; what
//! disqualifies a revision is being reachable only from a fork branch or an
//! unmerged PR, because then it names nothing a reader outside this workspace
//! can fetch.
//!
//! Resolve each citation on its own. One sentence can cite two lines that are
//! right at different revisions, and a check run at either revision then
//! reports a single tidy error while vouching for the very citation the other
//! condemns.
//!
//! A row reading *no settled pin* means no revision has been agreed for that
//! tree: say which revision you read, and do not take its `HEAD` for the pin.
//! Citations into non-EPICS sources (libc, RTEMS, `rtems-libbsd`, VxWorks,
//! vendored third-party) are outside this table and carry no pin.

#![allow(
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::field_reassign_with_default,
    clippy::type_complexity
)]

pub mod device_support;
pub mod records;
pub mod seq_runner;
pub mod snl;

pub use device_support::time_of_day::{SecPastEpochDeviceSupport, TimeOfDayStringDeviceSupport};
pub use records::epid::EpidRecord;
pub use records::throttle::ThrottleRecord;
pub use records::timestamp::TimestampRecord;

/// Path to the bundled database template directory.
pub const STD_DB_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db");

/// Return the epid record type factory for injection into IocBuilder.
///
/// Also loads std's `.dbd` ([`register_std_dbd`]), as `stdSupport.dbd` does in
/// C: without its `device()` menus a std DTYP serves only the bound choice and
/// a DTYP put by name is refused. Every factory below does the same.
pub fn epid_record_factory() -> (&'static str, epics_base_rs::server::RecordFactory) {
    register_std_dbd();
    ("epid", Box::new(|| Box::new(EpidRecord::default())))
}

/// Hand std's generated `.dbd` to base, C's `dbLoadDatabase` of it: the
/// record types and menus the dbStatic reports walk, and the `device()` menus
/// of std's own record types. Idempotent: base dedups a repeat.
///
/// The `device()` lines std adds to base's types (`devTimeOfDay.dbd`'s ai and
/// stringin) arrive when their support is registered instead, because a
/// type's DTYP index is the order its lines arrive: registered here, they
/// would land before asyn's whenever the record types are registered first.
pub fn register_std_dbd() {
    use records::dbd_generated::{DBD, RECORD_TYPES, devices};
    epics_base_rs::server::record::register_contributed_dbd(&DBD);
    for &record_type in RECORD_TYPES {
        if let Some(rows) = devices(record_type) {
            epics_base_rs::server::record::register_device_menu(record_type, rows);
        }
    }
}

/// Return the throttle record type factory for injection into IocBuilder.
pub fn throttle_record_factory() -> (&'static str, epics_base_rs::server::RecordFactory) {
    register_std_dbd();
    ("throttle", Box::new(|| Box::new(ThrottleRecord::default())))
}

/// Return the timestamp record type factory for injection into IocBuilder.
pub fn timestamp_record_factory() -> (&'static str, epics_base_rs::server::RecordFactory) {
    register_std_dbd();
    (
        "timestamp",
        Box::new(|| Box::new(TimestampRecord::default())),
    )
}

/// Return all std record type factories for bulk registration.
pub fn std_record_factories() -> Vec<(&'static str, epics_base_rs::server::RecordFactory)> {
    vec![
        epid_record_factory(),
        throttle_record_factory(),
        timestamp_record_factory(),
    ]
}

/// Register all std record types via the global registry (legacy).
/// Prefer `std_record_factories()` with `IocBuilder::register_record_type()`.
pub fn register_std_record_types() {
    for (name, factory) in std_record_factories() {
        epics_base_rs::server::db_loader::register_record_type(name, factory);
    }
}

/// A std device support as its record type, its `devSup` row and its factory,
/// the three arguments of `register_device_support`.
pub type StdDeviceSupport = (
    &'static str,
    &'static epics_base_rs::server::record::DevSup,
    epics_base_rs::server::DeviceSupportFactory,
);

/// The "Sec Past Epoch" (`ai`) device support from `devTimeOfDay.c`
/// (`devAiTodSeconds`).
pub fn sec_past_epoch_device_factory() -> StdDeviceSupport {
    register_std_dbd();
    (
        "ai",
        &records::dbd_generated::DEVICES_AI[0],
        Box::new(|| Box::new(SecPastEpochDeviceSupport::new())),
    )
}

/// The "Time of Day" (`stringin`) device support from `devTimeOfDay.c`
/// (`devSiTodString`).
pub fn time_of_day_device_factory() -> StdDeviceSupport {
    register_std_dbd();
    (
        "stringin",
        &records::dbd_generated::DEVICES_STRINGIN[0],
        Box::new(|| Box::new(TimeOfDayStringDeviceSupport::new())),
    )
}

/// Return all std-module device supports for bulk registration.
///
/// These are the `devTimeOfDay.c` DTYPs ("Sec Past Epoch" ai, "Time of Day"
/// stringin). They need only the framework's `ProcessContext` (PHAS/TSE), no
/// INP, so a static [`DeviceSupportFactory`](epics_base_rs::server::DeviceSupportFactory)
/// suffices. Register each onto an `IocBuilder` / `IocApplication` /
/// `CaServerBuilder` via its `register_device_support(record_type, devsup,
/// factory)`, which also adds the row to the record type's DTYP menu.
pub fn std_device_supports() -> Vec<StdDeviceSupport> {
    vec![
        sec_past_epoch_device_factory(),
        time_of_day_device_factory(),
    ]
}
