pub mod fields;
pub mod flags;
pub mod coordinate;
pub mod device_state;
pub mod record;
pub mod device_support;
pub mod sim_motor;
pub mod poll_loop;
pub mod builder;
pub mod axis_runtime;
pub mod ioc;

pub use fields::*;
pub use flags::*;
pub use record::MotorRecord;
pub use builder::MotorBuilder;
pub use axis_runtime::{AxisHandle, AxisRuntime};

/// Path to the motor ioc directory (for motor.template resolution).
pub const MOTOR_IOC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/ioc");

/// Register the "motor" record type factory with the epics-base db_loader.
/// Call this at application startup before loading .db files that use `record(motor, ...)`.
pub fn register_motor_record_type() {
    epics_base_rs::server::db_loader::register_record_type(
        "motor",
        Box::new(|| Box::new(MotorRecord::default())),
    );
}
