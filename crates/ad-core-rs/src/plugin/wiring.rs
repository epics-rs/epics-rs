//! Instance-scoped wiring registry for runtime NDArrayPort rewiring.
//!
//! Maps port names to their `NDArrayOutput`, enabling plugins to dynamically
//! change their upstream data source by writing to the NDArrayPort PV.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::channel::{NDArrayOutput, NDArraySender};

/// Instance-scoped registry: port name -> shared NDArrayOutput.
///
/// Owned by `PluginManager` as `Arc<WiringRegistry>`, enabling test isolation
/// (each test can create its own registry without port name collisions).
pub struct WiringRegistry {
    inner: Mutex<HashMap<String, Arc<parking_lot::Mutex<NDArrayOutput>>>>,
}

impl WiringRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Register a port's output in the wiring registry.
    pub fn register_output(&self, port_name: &str, output: Arc<parking_lot::Mutex<NDArrayOutput>>) {
        let mut reg = self.inner.lock().unwrap();
        reg.insert(port_name.to_string(), output);
    }

    /// Look up a port's output by name.
    pub fn lookup_output(&self, port_name: &str) -> Option<Arc<parking_lot::Mutex<NDArrayOutput>>> {
        let reg = self.inner.lock().ok()?;
        reg.get(port_name).cloned()
    }

    /// Rewire a sender from one upstream to another.
    ///
    /// Removes the sender from `old_upstream`'s output and adds it to `new_upstream`'s output.
    /// Returns `Err` if the new upstream port is not found in the registry.
    ///
    /// Self-wiring (sender's port_name == new_upstream) is rejected.
    /// Empty `old_upstream` is allowed (initial wiring).
    pub fn rewire(
        &self,
        sender: &NDArraySender,
        old_upstream: &str,
        new_upstream: &str,
    ) -> Result<(), String> {
        let sender_port = sender.port_name();

        // Prevent self-wiring
        if sender_port == new_upstream {
            return Err(format!("cannot wire port '{}' to itself", sender_port));
        }

        let reg = self.inner.lock().unwrap();

        // Remove from old upstream (if it exists)
        if !old_upstream.is_empty() {
            if let Some(old_output) = reg.get(old_upstream) {
                old_output.lock().remove(sender_port);
            }
            // If old upstream not found, that's okay — it may have been removed
        }

        // Add to new upstream
        if new_upstream.is_empty() {
            return Ok(());
        }
        let new_output = reg.get(new_upstream).ok_or_else(|| {
            format!(
                "upstream port '{}' not found in wiring registry",
                new_upstream
            )
        })?;
        new_output.lock().add(sender.clone());

        Ok(())
    }

    /// Rewire by port name only (used by the data loop at runtime).
    ///
    /// Extracts the sender from the old upstream's output and adds it to the new upstream's output.
    /// This avoids holding an `NDArraySender` clone inside the data loop, which would prevent
    /// channel shutdown.
    pub fn rewire_by_name(
        &self,
        sender_port: &str,
        old_upstream: &str,
        new_upstream: &str,
    ) -> Result<(), String> {
        // Prevent self-wiring
        if sender_port == new_upstream {
            return Err(format!("cannot wire port '{}' to itself", sender_port));
        }

        let reg = self.inner.lock().unwrap();

        // Extract sender from old upstream
        let sender = if !old_upstream.is_empty() {
            if let Some(old_output) = reg.get(old_upstream) {
                old_output.lock().take(sender_port)
            } else {
                None
            }
        } else {
            None
        };

        if new_upstream.is_empty() {
            // Just disconnect — sender (if any) is dropped
            return Ok(());
        }

        let new_output = reg.get(new_upstream).ok_or_else(|| {
            format!(
                "upstream port '{}' not found in wiring registry",
                new_upstream
            )
        })?;

        match sender {
            Some(s) => {
                new_output.lock().add(s);
                Ok(())
            }
            None => Err(format!(
                "sender '{}' not found in upstream '{}' output",
                sender_port, old_upstream
            )),
        }
    }
}

impl Default for WiringRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::channel::ndarray_channel;

    #[test]
    fn test_register_and_lookup() {
        let registry = WiringRegistry::new();
        let output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        registry.register_output("DRV1", output.clone());

        let found = registry.lookup_output("DRV1");
        assert!(found.is_some());
        assert!(registry.lookup_output("NONEXISTENT").is_none());
    }

    #[test]
    fn test_rewire_basic() {
        let registry = WiringRegistry::new();
        let drv_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        let stats_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        registry.register_output("DRV", drv_output.clone());
        registry.register_output("STATS", stats_output.clone());

        let (sender, _rx) = ndarray_channel("PLUGIN_A", 10);

        // Initial wiring: "" -> DRV
        registry.rewire(&sender, "", "DRV").unwrap();
        assert_eq!(drv_output.lock().num_senders(), 1);

        // Rewire: DRV -> STATS
        registry.rewire(&sender, "DRV", "STATS").unwrap();
        assert_eq!(drv_output.lock().num_senders(), 0);
        assert_eq!(stats_output.lock().num_senders(), 1);
    }

    #[test]
    fn test_rewire_self_rejected() {
        let registry = WiringRegistry::new();
        let output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        registry.register_output("SELF", output);

        let (sender, _rx) = ndarray_channel("SELF", 10);
        let result = registry.rewire(&sender, "", "SELF");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cannot wire port"));
    }

    #[test]
    fn test_rewire_nonexistent_port() {
        let registry = WiringRegistry::new();
        let (sender, _rx) = ndarray_channel("ORPHAN", 10);
        let result = registry.rewire(&sender, "", "NO_SUCH_PORT");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_rewire_to_empty_disconnects() {
        let registry = WiringRegistry::new();
        let drv_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        registry.register_output("DISC_DRV", drv_output.clone());

        let (sender, _rx) = ndarray_channel("DISC_PLUGIN", 10);
        registry.rewire(&sender, "", "DISC_DRV").unwrap();
        assert_eq!(drv_output.lock().num_senders(), 1);

        // Rewire to empty = disconnect
        registry.rewire(&sender, "DISC_DRV", "").unwrap();
        assert_eq!(drv_output.lock().num_senders(), 0);
    }

    #[test]
    fn test_isolation_between_registries() {
        // Two registries with the same port names don't interfere
        let r1 = WiringRegistry::new();
        let r2 = WiringRegistry::new();

        let out1 = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        let out2 = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));

        r1.register_output("DRV", out1.clone());
        r2.register_output("DRV", out2.clone());

        let (sender1, _rx1) = ndarray_channel("PLUGIN", 10);
        let (sender2, _rx2) = ndarray_channel("PLUGIN", 10);

        r1.rewire(&sender1, "", "DRV").unwrap();
        r2.rewire(&sender2, "", "DRV").unwrap();

        assert_eq!(out1.lock().num_senders(), 1);
        assert_eq!(out2.lock().num_senders(), 1);
    }
}
