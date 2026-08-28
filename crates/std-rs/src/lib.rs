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
pub fn epid_record_factory() -> (&'static str, epics_base_rs::server::RecordFactory) {
    ("epid", Box::new(|| Box::new(EpidRecord::default())))
}

/// Return the throttle record type factory for injection into IocBuilder.
pub fn throttle_record_factory() -> (&'static str, epics_base_rs::server::RecordFactory) {
    ("throttle", Box::new(|| Box::new(ThrottleRecord::default())))
}

/// Return the timestamp record type factory for injection into IocBuilder.
pub fn timestamp_record_factory() -> (&'static str, epics_base_rs::server::RecordFactory) {
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

/// Return the "Sec Past Epoch" (`ai`) device support factory from
/// `devTimeOfDay.c` (`devAiTodSeconds`) for injection into a builder.
pub fn sec_past_epoch_device_factory() -> (&'static str, epics_base_rs::server::DeviceSupportFactory)
{
    (
        "Sec Past Epoch",
        Box::new(|| Box::new(SecPastEpochDeviceSupport::new())),
    )
}

/// Return the "Time of Day" (`stringin`) device support factory from
/// `devTimeOfDay.c` (`devSiTodString`) for injection into a builder.
pub fn time_of_day_device_factory() -> (&'static str, epics_base_rs::server::DeviceSupportFactory) {
    (
        "Time of Day",
        Box::new(|| Box::new(TimeOfDayStringDeviceSupport::new())),
    )
}

/// Return all std-module device support factories for bulk registration.
///
/// These are the `devTimeOfDay.c` DTYPs ("Sec Past Epoch" ai, "Time of Day"
/// stringin). They need only the framework's `ProcessContext` (PHAS/TSE), no
/// INP, so a static [`DeviceSupportFactory`](epics_base_rs::server::DeviceSupportFactory)
/// suffices. Register each onto an `IocBuilder` / `IocApplication` /
/// `CaServerBuilder` via its `register_device_support(dtyp, factory)` — the
/// boxed factory satisfies the method's `Fn` bound — mirroring how
/// [`std_record_factories()`] is injected via `register_record_type()`.
pub fn std_device_supports() -> Vec<(&'static str, epics_base_rs::server::DeviceSupportFactory)> {
    vec![
        sec_past_epoch_device_factory(),
        time_of_day_device_factory(),
    ]
}
