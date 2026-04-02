//! Runtime module: promoted actors with event emission, shutdown, and supervision.
//!
//! Also re-exports async runtime primitives (`sync`, `task`, `select!`)
//! so that driver authors can use `asyn_rs::runtime::` instead of `tokio::` directly.

pub mod axis;
pub mod config;
pub mod event;
pub mod port;
pub mod supervisor;

/// Async sync primitives (channels, Notify, Mutex, etc.)
pub mod sync {
    pub use std::sync::Arc;
    pub use tokio::sync::{Mutex, Notify, RwLock, broadcast, mpsc, oneshot};
}

/// Async task utilities (spawn, sleep, interval, etc.)
pub mod task {
    use std::future::Future;
    use std::time::Duration;
    pub use tokio::runtime::Handle as RuntimeHandle;
    pub use tokio::task::JoinHandle;
    pub use tokio::time::interval;

    pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        tokio::spawn(future)
    }

    pub async fn sleep(duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    pub fn runtime_handle() -> RuntimeHandle {
        RuntimeHandle::current()
    }
}

/// Re-export `tokio::select!` macro.
pub use tokio::select;

pub use axis::{
    AxisActions, AxisDelayRequest, AxisMotorCommand, AxisPollDirective, AxisRuntime,
    AxisRuntimeHandle, create_axis_runtime,
};
pub use config::{BackoffConfig, RuntimeConfig, SupervisionPolicy};
pub use event::RuntimeEvent;
pub use port::{PortRuntimeHandle, create_port_runtime};
pub use supervisor::{SupervisionOutcome, supervise};
