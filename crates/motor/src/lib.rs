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
