#![allow(
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::field_reassign_with_default,
    clippy::type_complexity
)]

pub mod device_support;
pub mod records;
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
