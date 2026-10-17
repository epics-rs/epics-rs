//! Instance-scoped wiring registry for runtime NDArrayPort rewiring.
//!
//! Maps port names to their `NDArrayOutput`, enabling plugins to dynamically
//! change their upstream data source by writing to the NDArrayPort PV.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::channel::{NDArrayOutput, NDArraySender, WeakNDArraySender};

type SharedOutput = Arc<parking_lot::Mutex<NDArrayOutput>>;

/// Where a subscription's sender is.
enum Placement {
    /// In the output published at its `(port, addr)`.
    Attached(SharedOutput),
    /// Nothing is published there yet. Weak, so a subscription alone cannot
    /// keep the plugin's queue open.
    Pending(WeakNDArraySender),
}

/// One `(NDArrayPort, NDArrayAddr)` a plugin source slot is connected to: C's
/// asynGenericPointer interrupt user, which is per `(port, addr)` and does
/// not need anything published there to exist.
struct Subscription {
    sender_port: String,
    port: String,
    addr: i32,
    placement: Placement,
}

#[derive(Default)]
struct State {
    outputs: HashMap<String, SharedOutput>,
    /// Ports registered with more than one address. Any other port is
    /// single-address, where asyn ignores the address a client connects
    /// with (asynManager.c:2006 `getAddr` returns -1, and
    /// asynPortDriver.cpp:3359 delivers -1 as address 0).
    multi_addr: HashSet<String>,
    subscriptions: Vec<Subscription>,
}

impl State {
    fn key(&self, port: &str, addr: i32) -> String {
        if addr == 0 || !self.multi_addr.contains(port) {
            port.to_string()
        } else {
            format!("{port}:{addr}")
        }
    }

    fn resolve(&self, port: &str, addr: i32) -> Option<SharedOutput> {
        self.outputs.get(&self.key(port, addr)).cloned()
    }

    /// Put subscription `i` in the output its `(port, addr)` resolves to now.
    /// Returns false if its plugin is gone.
    fn place(&mut self, i: usize) -> bool {
        let target = self.resolve(&self.subscriptions[i].port, self.subscriptions[i].addr);
        let sub = &mut self.subscriptions[i];
        let sender = match &sub.placement {
            Placement::Attached(at) => {
                if target.as_ref().is_some_and(|t| Arc::ptr_eq(t, at)) {
                    return true;
                }
                at.lock().take(&sub.sender_port)
            }
            Placement::Pending(weak) => weak.upgrade(),
        };
        let Some(sender) = sender else {
            return false;
        };
        sub.placement = match target {
            Some(output) => {
                output.lock().add(sender);
                Placement::Attached(output)
            }
            None => Placement::Pending(sender.downgrade()),
        };
        true
    }
}

/// Instance-scoped registry of the outputs published at each `(port, addr)`
/// and of the plugin sources subscribed to them.
///
/// Owned by `PluginManager` as `Arc<WiringRegistry>`, enabling test isolation
/// (each test can create its own registry without port name collisions).
pub struct WiringRegistry {
    state: Mutex<State>,
}

impl WiringRegistry {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
        }
    }

    /// Register a port's output under the bare port name (addr 0).
    pub fn register_output(&self, port_name: &str, output: SharedOutput) {
        self.register_output_at(port_name, 0, output);
    }

    /// Register the output a driver publishes at `addr`, and move every
    /// subscription to `port_name` into the output it now resolves to.
    pub fn register_output_at(&self, port_name: &str, addr: i32, output: SharedOutput) {
        let mut state = self.state.lock().unwrap();
        if addr != 0 {
            state.multi_addr.insert(port_name.to_string());
        }
        let key = state.key(port_name, addr);
        state.outputs.insert(key, output);
        let mut i = 0;
        while i < state.subscriptions.len() {
            if state.subscriptions[i].port != port_name || state.place(i) {
                i += 1;
            } else {
                state.subscriptions.swap_remove(i);
            }
        }
    }

    /// True if `port` publishes NDArrays at any address: C's
    /// asynGenericPointer interface, which is per port, not per address.
    pub fn publishes(&self, port: &str) -> bool {
        let state = self.state.lock().unwrap();
        state.outputs.contains_key(port) || state.multi_addr.contains(port)
    }

    /// The output published at `(port, addr)`, if any.
    pub fn output_at(&self, port: &str, addr: i32) -> Option<SharedOutput> {
        self.state.lock().unwrap().resolve(port, addr)
    }

    /// Connect `sender` to `(port, addr)`. It receives what is published
    /// there now or later. Refused for a port that publishes nothing (C's
    /// connectToArrayPort failure) and for the sender's own port.
    pub fn subscribe(&self, sender: &NDArraySender, port: &str, addr: i32) -> Result<(), String> {
        if sender.port_name() == port {
            return Err(format!("cannot wire port '{port}' to itself"));
        }
        if !self.publishes(port) {
            return Err(format!(
                "upstream port '{port}' not found in wiring registry"
            ));
        }
        let mut state = self.state.lock().unwrap();
        state.subscriptions.push(Subscription {
            sender_port: sender.port_name().to_string(),
            port: port.to_string(),
            addr,
            placement: Placement::Pending(sender.downgrade()),
        });
        let i = state.subscriptions.len() - 1;
        if !state.place(i) {
            state.subscriptions.pop();
        }
        Ok(())
    }

    /// Disconnect one subscription of `sender_port` to `(port, addr)`.
    pub fn unsubscribe(&self, sender_port: &str, port: &str, addr: i32) {
        let mut state = self.state.lock().unwrap();
        let Some(i) = state
            .subscriptions
            .iter()
            .position(|s| s.sender_port == sender_port && s.port == port && s.addr == addr)
        else {
            return;
        };
        if let Placement::Attached(at) = state.subscriptions.remove(i).placement {
            at.lock().take(sender_port);
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

        assert!(registry.output_at("DRV1", 0).is_some());
        assert!(registry.output_at("NONEXISTENT", 0).is_none());
    }

    #[test]
    fn single_address_port_ignores_ndarray_addr() {
        // asyn delivers a single-address port's arrays to a client at any
        // address, so NDArrayAddr=1 on a driver port still receives them.
        let registry = WiringRegistry::new();
        let output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        registry.register_output("DRV", output.clone());

        let (sender, _rx) = ndarray_channel("CONSUMER", 10);
        registry
            .subscribe(&sender, "DRV", 1)
            .expect("NDArrayAddr=1 on a single-address port must wire");
        assert_eq!(output.lock().num_senders(), 1);
    }

    #[test]
    fn per_address_driver_outputs_stay_distinct() {
        let registry = WiringRegistry::new();
        let image = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        let signal = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        registry.register_output("DRV", image.clone());
        registry.register_output_at("DRV", 3, signal.clone());

        let (sender, _rx) = ndarray_channel("CONSUMER", 10);
        registry.subscribe(&sender, "DRV", 3).unwrap();
        assert_eq!(signal.lock().num_senders(), 1);
        assert_eq!(image.lock().num_senders(), 0);
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
        registry.subscribe(&sender, "DRV", 0).unwrap();
        assert_eq!(drv_output.lock().num_senders(), 1);

        // Rewire: DRV -> STATS
        registry.unsubscribe("PLUGIN_A", "DRV", 0);
        registry.subscribe(&sender, "STATS", 0).unwrap();
        assert_eq!(drv_output.lock().num_senders(), 0);
        assert_eq!(stats_output.lock().num_senders(), 1);
    }

    #[test]
    fn test_rewire_self_rejected() {
        let registry = WiringRegistry::new();
        let output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        registry.register_output("SELF", output);

        let (sender, _rx) = ndarray_channel("SELF", 10);
        let result = registry.subscribe(&sender, "SELF", 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cannot wire port"));
    }

    #[test]
    fn test_rewire_nonexistent_port() {
        let registry = WiringRegistry::new();
        let (sender, _rx) = ndarray_channel("ORPHAN", 10);
        let result = registry.subscribe(&sender, "NO_SUCH_PORT", 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_unsubscribe_disconnects() {
        let registry = WiringRegistry::new();
        let drv_output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        registry.register_output("DISC_DRV", drv_output.clone());

        let (sender, _rx) = ndarray_channel("DISC_PLUGIN", 10);
        registry.subscribe(&sender, "DISC_DRV", 0).unwrap();
        assert_eq!(drv_output.lock().num_senders(), 1);

        registry.unsubscribe("DISC_PLUGIN", "DISC_DRV", 0);
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

        r1.subscribe(&sender1, "DRV", 0).unwrap();
        r2.subscribe(&sender2, "DRV", 0).unwrap();

        assert_eq!(out1.lock().num_senders(), 1);
        assert_eq!(out2.lock().num_senders(), 1);
    }

    #[test]
    fn a_subscription_moves_when_its_port_becomes_multi_address() {
        // Before address 3 is registered, (DRV, 3) reads as the
        // single-address port's output; once it is, it is address 3's.
        let registry = WiringRegistry::new();
        let image = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        registry.register_output("DRV", image.clone());
        let (sender, _rx) = ndarray_channel("CONSUMER", 10);
        registry.subscribe(&sender, "DRV", 3).unwrap();
        assert_eq!(image.lock().num_senders(), 1);

        let signal = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        registry.register_output_at("DRV", 3, signal.clone());
        assert_eq!(image.lock().num_senders(), 0);
        assert_eq!(signal.lock().num_senders(), 1);
    }

    #[test]
    fn a_pending_subscription_does_not_outlive_its_plugin() {
        let registry = WiringRegistry::new();
        registry.register_output(
            "DRV",
            Arc::new(parking_lot::Mutex::new(NDArrayOutput::new())),
        );
        let (sender, rx) = ndarray_channel("GONE", 10);
        registry.register_output_at(
            "DRV",
            1,
            Arc::new(parking_lot::Mutex::new(NDArrayOutput::new())),
        );
        registry.subscribe(&sender, "DRV", 2).unwrap();
        drop((sender, rx));
        let late = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        registry.register_output_at("DRV", 2, late.clone());
        assert_eq!(late.lock().num_senders(), 0);
        assert!(registry.state.lock().unwrap().subscriptions.is_empty());
    }

    #[test]
    fn re_registering_an_address_moves_its_subscribers() {
        let registry = WiringRegistry::new();
        let old = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        registry.register_output("DRV", old.clone());
        let (sender, _rx) = ndarray_channel("CONSUMER", 10);
        registry.subscribe(&sender, "DRV", 0).unwrap();
        let new = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        registry.register_output("DRV", new.clone());
        assert_eq!(old.lock().num_senders(), 0);
        assert_eq!(new.lock().num_senders(), 1);
    }
}
