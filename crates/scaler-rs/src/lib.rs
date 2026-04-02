#![allow(
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::field_reassign_with_default,
    clippy::type_complexity
)]

pub mod device_support;
pub mod records;

pub use records::scaler::{MAX_SCALER_CHANNELS, ScalerRecord};

/// Path to the bundled database template directory.
pub const SCALER_DB_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db");

/// Return the scaler record type factory for injection into IocBuilder.
pub fn scaler_record_factory() -> (&'static str, epics_base_rs::server::RecordFactory) {
    ("scaler", Box::new(|| Box::new(ScalerRecord::default())))
}

/// Register the scaler record type via the global registry (legacy).
/// Prefer `scaler_record_factory()` with `IocBuilder::register_record_type()`.
pub fn register_scaler_record_types() {
    let (name, factory) = scaler_record_factory();
    epics_base_rs::server::db_loader::register_record_type(name, factory);
}
