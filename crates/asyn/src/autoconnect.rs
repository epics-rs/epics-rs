//! Auto-connect background task.
//!
//! [`spawn_auto_connect`] is called by [`crate::manager::PortManager`] when
//! registering a port with auto-connect enabled. The manager owns the
//! `JoinHandle` and `shutdown` sender to control the task lifecycle.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::watch;

use crate::port::PortDriver;
use crate::asyn_trace;
use crate::trace::TraceMask;
use crate::user::AsynUser;

/// Configuration for the auto-connect retry loop.
pub struct AutoConnectConfig {
    /// Interval between reconnection attempts.
    pub retry_interval: Duration,
    /// Whether auto-connect is currently active.
    pub enabled: bool,
}

impl Default for AutoConnectConfig {
    fn default() -> Self {
        Self {
            retry_interval: Duration::from_secs(2),
            enabled: true,
        }
    }
}

/// Spawn an auto-connect background task for a port.
///
/// The task periodically attempts `connect()` when the port is enabled,
/// disconnected, and has auto_connect set. It exits when the `shutdown`
/// watch channel signals `true`.
///
/// Returns a `JoinHandle` that the manager should keep to manage the task.
pub fn spawn_auto_connect(
    port: Arc<Mutex<dyn PortDriver>>,
    config: AutoConnectConfig,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let (trace, port_name) = {
        let p = port.lock();
        (p.base().trace.clone(), p.base().port_name.clone())
    };

    tokio::spawn(async move {
        if !config.enabled {
            return;
        }

        let user = AsynUser::default();

        loop {
            // Check shutdown signal
            if *shutdown.borrow() {
                break;
            }

            // Check port state
            let should_connect = {
                let p = port.lock();
                let base = p.base();
                base.enabled && !base.connected && base.auto_connect
            };

            if should_connect {
                asyn_trace!(Some(trace), &port_name, TraceMask::FLOW, "auto-connect attempt");
                let mut p = port.lock();
                // Re-check under write lock
                if p.base().enabled && !p.base().connected && p.base().auto_connect {
                    let result = p.connect(&user);
                    match &result {
                        Ok(()) => asyn_trace!(Some(trace), &port_name, TraceMask::FLOW, "auto-connect succeeded"),
                        Err(e) => asyn_trace!(Some(trace), &port_name, TraceMask::FLOW, "auto-connect failed: {}", e),
                    }
                }
            }

            // Wait for retry interval or shutdown
            tokio::select! {
                _ = tokio::time::sleep(config.retry_interval) => {}
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::{PortDriverBase, PortFlags};

    struct MockDriver {
        base: PortDriverBase,
    }

    impl MockDriver {
        fn new() -> Self {
            let mut base = PortDriverBase::new("autotest", 1, PortFlags::default());
            base.connected = false;
            Self { base }
        }
    }

    impl PortDriver for MockDriver {
        fn base(&self) -> &PortDriverBase { &self.base }
        fn base_mut(&mut self) -> &mut PortDriverBase { &mut self.base }
    }

    #[tokio::test]
    async fn test_auto_connect_reconnects() {
        let port: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(MockDriver::new()));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let config = AutoConnectConfig {
            retry_interval: Duration::from_millis(10),
            enabled: true,
        };

        let handle = spawn_auto_connect(port.clone(), config, shutdown_rx);

        // Wait a bit for reconnection
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(port.lock().base().connected);

        // Shutdown
        shutdown_tx.send(true).unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_auto_connect_shutdown() {
        let port: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(MockDriver::new()));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let config = AutoConnectConfig {
            retry_interval: Duration::from_secs(60), // long interval
            enabled: true,
        };

        let handle = spawn_auto_connect(port.clone(), config, shutdown_rx);

        // Immediate shutdown
        shutdown_tx.send(true).unwrap();
        // Task should finish promptly
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("task should exit on shutdown")
            .unwrap();
    }

    #[tokio::test]
    async fn test_auto_connect_disabled_exits() {
        let port: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(MockDriver::new()));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        let config = AutoConnectConfig {
            retry_interval: Duration::from_millis(10),
            enabled: false,
        };

        let handle = spawn_auto_connect(port.clone(), config, shutdown_rx);
        // Should exit immediately since config.enabled = false
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("task should exit when disabled")
            .unwrap();

        // Port should still be disconnected
        assert!(!port.lock().base().connected);
    }
}
