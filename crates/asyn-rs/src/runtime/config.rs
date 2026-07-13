use std::time::Duration;

use crate::services::PortServices;

/// Configuration for a port runtime.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Channel capacity for the command channel.
    pub channel_capacity: usize,
    /// Auto-connect on startup.
    pub auto_connect: bool,
    /// Backoff configuration for auto-connect retries.
    pub connect_backoff: BackoffConfig,
    /// Supervision policy.
    pub supervision: SupervisionPolicy,
    /// How long port creation waits for an `auto_connect` port to come up
    /// before returning — C `waitConnect(pport->pconnectUser,
    /// pasynBase->autoConnectTimeout)` at the end of registration
    /// (asynManager.c:2135), with `DEFAULT_AUTOCONNECT_TIMEOUT` = 0.5 s (:49,
    /// :507). It bounds the wait; it does not decide the outcome — a port that
    /// has not connected by then keeps retrying on its own timer, exactly as C's
    /// does, and registration still succeeds.
    pub auto_connect_timeout: Duration,
    /// The trace configuration and exception list the port is created with —
    /// C's `pasynBase`. Defaults to the process-wide
    /// [`PortServices::global()`], so a port built with a default config is
    /// still traceable and still announces exceptions; a
    /// [`crate::manager::PortManager`] carrying its own `TraceManager`
    /// substitutes its own here.
    pub services: PortServices,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 1024,
            auto_connect: true,
            connect_backoff: BackoffConfig::default(),
            supervision: SupervisionPolicy::default(),
            auto_connect_timeout: Duration::from_millis(500),
            services: PortServices::global(),
        }
    }
}

/// Backoff configuration for retries.
#[derive(Debug, Clone)]
pub struct BackoffConfig {
    pub initial: Duration,
    pub max: Duration,
    pub multiplier: f64,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(100),
            max: Duration::from_secs(30),
            multiplier: 2.0,
        }
    }
}

/// Supervision policy for runtime actors.
#[derive(Debug, Clone)]
pub struct SupervisionPolicy {
    /// Maximum restart attempts before giving up.
    pub max_restarts: usize,
    /// Window in which max_restarts is counted.
    pub restart_window: Duration,
}

impl Default for SupervisionPolicy {
    fn default() -> Self {
        Self {
            max_restarts: 5,
            restart_window: Duration::from_secs(60),
        }
    }
}
