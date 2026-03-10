use std::time::Duration;

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
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 1024,
            auto_connect: true,
            connect_backoff: BackoffConfig::default(),
            supervision: SupervisionPolicy::default(),
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
