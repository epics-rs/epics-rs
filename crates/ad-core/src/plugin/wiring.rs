//! Global wiring registry for runtime NDArrayPort rewiring.
//!
//! Maps port names to their `NDArrayOutput`, enabling plugins to dynamically
//! change their upstream data source by writing to the NDArrayPort PV.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::sync::Arc;

use super::channel::{NDArrayOutput, NDArraySender};

/// Global registry: port name -> shared NDArrayOutput.
static WIRING_REGISTRY: OnceLock<Mutex<HashMap<String, Arc<parking_lot::Mutex<NDArrayOutput>>>>> =
    OnceLock::new();

fn get_registry() -> &'static Mutex<HashMap<String, Arc<parking_lot::Mutex<NDArrayOutput>>>> {
    WIRING_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a port's output in the global wiring registry.
pub fn register_output(port_name: &str, output: Arc<parking_lot::Mutex<NDArrayOutput>>) {
    let mut reg = get_registry().lock().unwrap();
    reg.insert(port_name.to_string(), output);
}

/// Look up a port's output by name.
pub fn lookup_output(port_name: &str) -> Option<Arc<parking_lot::Mutex<NDArrayOutput>>> {
    let reg = get_registry().lock().ok()?;
    reg.get(port_name).cloned()
}

/// Rewire a sender from one upstream to another.
///
/// Removes the sender from `old_upstream`'s output and adds it to `new_upstream`'s output.
/// Returns `Err` if the new upstream port is not found in the registry.
///
/// Self-wiring (sender's port_name == new_upstream) is rejected.
/// Empty `old_upstream` is allowed (initial wiring).
pub fn rewire(sender: &NDArraySender, old_upstream: &str, new_upstream: &str) -> Result<(), String> {
    let sender_port = sender.port_name();

    // Prevent self-wiring
    if sender_port == new_upstream {
        return Err(format!(
            "cannot wire port '{}' to itself",
            sender_port
        ));
    }

    let reg = get_registry().lock().unwrap();

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
    let new_output = reg
        .get(new_upstream)
        .ok_or_else(|| format!("upstream port '{}' not found in wiring registry", new_upstream))?;
    new_output.lock().add(sender.clone());

    Ok(())
}

/// Rewire by port name only (used by the data loop at runtime).
///
/// Extracts the sender from the old upstream's output and adds it to the new upstream's output.
/// This avoids holding an `NDArraySender` clone inside the data loop, which would prevent
/// channel shutdown.
pub fn rewire_by_name(sender_port: &str, old_upstream: &str, new_upstream: &str) -> Result<(), String> {
    // Prevent self-wiring
    if sender_port == new_upstream {
        return Err(format!("cannot wire port '{}' to itself", sender_port));
    }

    let reg = get_registry().lock().unwrap();

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

    let new_output = reg
        .get(new_upstream)
        .ok_or_else(|| format!("upstream port '{}' not found in wiring registry", new_upstream))?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::channel::ndarray_channel;

    // Note: tests share the global registry, so use unique port names.

    #[test]
    fn test_register_and_lookup() {
        let output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        register_output("WIRING_TEST_DRV1", output.clone());

        let found = lookup_output("WIRING_TEST_DRV1");
        assert!(found.is_some());
        assert!(lookup_output("NONEXISTENT_PORT_XYZ").is_none());
    }

    #[test]
    fn test_rewire_basic() {
        let drv_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        let stats_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        register_output("WIRING_DRV", drv_output.clone());
        register_output("WIRING_STATS", stats_output.clone());

        let (sender, _rx) = ndarray_channel("WIRING_PLUGIN_A", 10);

        // Initial wiring: "" -> DRV
        rewire(&sender, "", "WIRING_DRV").unwrap();
        assert_eq!(drv_output.lock().num_senders(), 1);

        // Rewire: DRV -> STATS
        rewire(&sender, "WIRING_DRV", "WIRING_STATS").unwrap();
        assert_eq!(drv_output.lock().num_senders(), 0);
        assert_eq!(stats_output.lock().num_senders(), 1);
    }

    #[test]
    fn test_rewire_self_rejected() {
        let output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        register_output("WIRING_SELF", output);

        let (sender, _rx) = ndarray_channel("WIRING_SELF", 10);
        let result = rewire(&sender, "", "WIRING_SELF");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cannot wire port"));
    }

    #[test]
    fn test_rewire_nonexistent_port() {
        let (sender, _rx) = ndarray_channel("WIRING_ORPHAN", 10);
        let result = rewire(&sender, "", "NO_SUCH_PORT_12345");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_rewire_to_empty_disconnects() {
        let drv_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        register_output("WIRING_DISC_DRV", drv_output.clone());

        let (sender, _rx) = ndarray_channel("WIRING_DISC_PLUGIN", 10);
        rewire(&sender, "", "WIRING_DISC_DRV").unwrap();
        assert_eq!(drv_output.lock().num_senders(), 1);

        // Rewire to empty = disconnect
        rewire(&sender, "WIRING_DISC_DRV", "").unwrap();
        assert_eq!(drv_output.lock().num_senders(), 0);
    }
}
