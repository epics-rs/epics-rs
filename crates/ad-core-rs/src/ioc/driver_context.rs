use std::sync::Arc;

use crate::ndarray_pool::NDArrayPool;
use crate::plugin::channel::{NDArrayOutput, NDArraySender};
use crate::plugin::wiring::WiringRegistry;

/// Abstraction over a detector driver's runtime, providing what plugin
/// configure commands need: an array pool and a way to wire downstream.
pub trait DriverContext: Send + Sync {
    /// The shared NDArrayPool for array allocation.
    fn pool(&self) -> Arc<NDArrayPool>;

    /// Connect a plugin's sender as a downstream consumer of this driver's arrays.
    fn connect_downstream(&self, sender: NDArraySender);
}

/// Generic DriverContext built from a pool and shared array output.
///
/// Eliminates the need for per-detector DriverContext implementations.
pub struct GenericDriverContext {
    pool: Arc<NDArrayPool>,
    output: Arc<parking_lot::Mutex<NDArrayOutput>>,
}

impl GenericDriverContext {
    pub fn new(
        pool: Arc<NDArrayPool>,
        output: Arc<parking_lot::Mutex<NDArrayOutput>>,
        port_name: &str,
        wiring: &WiringRegistry,
    ) -> Self {
        wiring.register_output(port_name, output.clone());
        Self { pool, output }
    }
}

impl DriverContext for GenericDriverContext {
    fn pool(&self) -> Arc<NDArrayPool> {
        self.pool.clone()
    }

    fn connect_downstream(&self, sender: NDArraySender) {
        self.output.lock().add(sender);
    }
}
