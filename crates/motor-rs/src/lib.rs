#![allow(
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::field_reassign_with_default,
    clippy::if_same_then_else,
    clippy::type_complexity
)]

pub mod axis_runtime;
pub mod builder;
pub(crate) mod coordinate;
pub mod device_state;
pub mod device_support;
pub(crate) mod fields;
pub mod flags;
pub mod ioc;
pub mod poll_loop;
pub mod record;
pub mod sim_motor;

pub use axis_runtime::{AxisHandle, AxisRuntime};
pub use builder::MotorBuilder;
pub use record::MotorRecord;

/// Path to the motor ioc directory (for motor.template resolution).
pub const MOTOR_IOC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/ioc");

/// Return the motor record type factory (name, factory) for injection into IocBuilder.
pub fn motor_record_factory() -> (&'static str, epics_base_rs::server::RecordFactory) {
    ("motor", Box::new(|| Box::new(MotorRecord::default())))
}

/// Register the "motor" record type via the global registry (legacy).
/// Prefer `motor_record_factory()` with `IocBuilder::register_record_type()`.
pub fn register_motor_record_type() {
    epics_base_rs::server::db_loader::register_record_type(
        "motor",
        Box::new(|| Box::new(MotorRecord::default())),
    );
}
