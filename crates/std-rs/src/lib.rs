#![allow(
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::field_reassign_with_default,
    clippy::type_complexity
)]

pub mod device_support;
pub mod records;
pub mod snl;

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
