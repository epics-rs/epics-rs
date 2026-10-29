use std::sync::Arc;

use crate::ndarray_pool::DriverPool;
use crate::plugin::channel::{NDArrayOutput, NDArraySender};
use crate::plugin::wiring::WiringRegistry;

/// Abstraction over a detector driver's runtime, providing what plugin
/// configure commands need: an array pool and a way to wire downstream.
pub trait DriverContext: Send + Sync {
    /// The shared NDArrayPool for array allocation.
    fn pool(&self) -> DriverPool;

    /// Connect a plugin's sender as a downstream consumer of this driver's arrays.
    fn connect_downstream(&self, sender: NDArraySender);
}

/// Generic DriverContext built from a pool and shared array output.
///
/// Eliminates the need for per-detector DriverContext implementations.
pub struct GenericDriverContext {
    pool: DriverPool,
    output: Arc<parking_lot::Mutex<NDArrayOutput>>,
}

impl GenericDriverContext {
    /// Registers `output` as `port_name`'s address 0, with the
    /// `ASYN_MULTIDEVICE` flag of the asyn port published under that name.
    pub fn new(
        pool: DriverPool,
        output: Arc<parking_lot::Mutex<NDArrayOutput>>,
        port_name: &str,
        wiring: &WiringRegistry,
    ) -> Self {
        if let Some(port) = asyn_rs::registry::get_port(port_name) {
            wiring.set_multi_device(port_name, port.handle.is_multi_device());
        }
        wiring.register_output(port_name, output.clone());
        Self { pool, output }
    }
}

impl DriverContext for GenericDriverContext {
    fn pool(&self) -> DriverPool {
        self.pool.clone()
    }

    fn connect_downstream(&self, sender: NDArraySender) {
        self.output.lock().add(sender);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::channel::ndarray_channel;
    use asyn_rs::port::{InterfaceMask, PortDriver, PortDriverBase, PortFlags};
    use asyn_rs::runtime::RuntimeConfig;
    use asyn_rs::runtime::port::create_port_runtime;

    struct Port(PortDriverBase);
    impl PortDriver for Port {
        fn base(&self) -> &PortDriverBase {
            &self.0
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.0
        }
    }

    /// ADMythen registers maxAddr 1 with ASYN_MULTIDEVICE (mythen.cpp:1279),
    /// so a plugin at NDArrayAddr 1 gets none of the arrays it posts at
    /// address 0 (asynPortDriver.cpp:3359).
    #[test]
    fn a_multi_device_drivers_other_address_gets_nothing() {
        let name = "GDC_MULTI_ONE";
        let claim = asyn_rs::registry::claim_port(name).unwrap();
        let base = PortDriverBase::new(
            name,
            1,
            PortFlags {
                multi_device: true,
                ..PortFlags::default()
            },
            InterfaceMask::GENERIC_POINTER,
            InterfaceMask::GENERIC_POINTER,
        );
        let (runtime, _actor) =
            create_port_runtime(&claim, Port(base), RuntimeConfig::default()).unwrap();
        claim
            .publish(
                runtime.port_handle().clone(),
                &asyn_rs::registry::open_owner(),
            )
            .unwrap();
        let wiring = WiringRegistry::new();
        let output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
        GenericDriverContext::new(
            DriverPool::standalone(1_000_000),
            output.clone(),
            name,
            &wiring,
        );
        let (at1, _rx1) = ndarray_channel("GDC_AT1", 10);
        wiring.subscribe(&at1, name, 1).unwrap();
        assert_eq!(output.lock().num_senders(), 0);
        let (at0, _rx0) = ndarray_channel("GDC_AT0", 10);
        wiring.subscribe(&at0, name, 0).unwrap();
        assert_eq!(output.lock().num_senders(), 1);
        asyn_rs::registry::unregister_port(name);
    }
}
