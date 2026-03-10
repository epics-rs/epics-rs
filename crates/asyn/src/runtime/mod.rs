//! Runtime module: promoted actors with event emission, shutdown, and supervision.

pub mod axis;
pub mod config;
pub mod event;
pub mod port;
pub mod supervisor;

pub use axis::{
    AxisActions, AxisDelayRequest, AxisMotorCommand, AxisPollDirective, AxisRuntime,
    AxisRuntimeHandle, create_axis_runtime,
};
pub use config::{BackoffConfig, RuntimeConfig, SupervisionPolicy};
pub use event::RuntimeEvent;
pub use port::{PortRuntimeHandle, create_port_runtime};
pub use supervisor::{SupervisionOutcome, supervise};
