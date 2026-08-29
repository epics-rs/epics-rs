use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{
    NDPluginProcess, ParamChangeResult, ParamUpdate, PluginParamSnapshot, ProcessResult,
};
use parking_lot::Mutex;

/// Maximum number of gather input ports.
pub const MAX_GATHER_PORTS: usize = 8;

/// Per-port param indices for one gather source.
#[derive(Debug, Clone, Copy, Default)]
struct GatherPortParams {
    /// Param index for GATHER_NDARRAY_PORT_N (Octet).
    port_idx: Option<usize>,
    /// Param index for GATHER_NDARRAY_ADDR_N (Int32).
    addr_idx: Option<usize>,
}

/// Pure gather processing logic: merges arrays from multiple upstream ports
/// into a single output stream.
///
/// Multi-source subscription is achieved at the IOC wiring level:
/// `NDGatherConfigure` registers the same `NDArraySender` with multiple
/// upstream `NDArrayOutput`s, so arrays from any configured source arrive
/// on the plugin's single input channel.
///
/// The processor stores the configured source port names and addresses as
/// params (GATHER_NDARRAY_PORT_1..8, GATHER_NDARRAY_ADDR_1..8) for
/// introspection and runtime reconfiguration via PVs.
pub struct GatherProcessor {
    /// Total arrays received across all sources.
    count: AtomicU64,
    /// The configured sources, rewritten by a `GATHER_NDARRAY_*` param write
    /// while the frame path may be running.
    sources: Mutex<GatherSources>,
    /// Param indices for per-port params.
    port_params: [GatherPortParams; MAX_GATHER_PORTS],
    /// Param index for GATHER_NUM_PORTS.
    num_ports_idx: Option<usize>,
}

/// The configured source set: the port names, their addresses, and how many
/// of the names are non-empty.
#[derive(Default)]
struct GatherSources {
    /// Number of configured source ports (set during construction or param change).
    num_ports: usize,
    /// Configured source port names (indexed 0..MAX_GATHER_PORTS-1).
    ports: [String; MAX_GATHER_PORTS],
    /// Configured source addresses (indexed 0..MAX_GATHER_PORTS-1).
    addrs: [i32; MAX_GATHER_PORTS],
}

impl GatherProcessor {
    pub fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            sources: Mutex::new(GatherSources::default()),
            port_params: [GatherPortParams::default(); MAX_GATHER_PORTS],
            num_ports_idx: None,
        }
    }

    /// Create a GatherProcessor pre-configured with the given source port names.
    pub fn with_ports(ports: &[&str]) -> Self {
        let proc = Self::new();
        let n = ports.len().min(MAX_GATHER_PORTS);
        {
            let mut src = proc.sources.lock();
            src.num_ports = n;
            for (i, &name) in ports.iter().take(n).enumerate() {
                src.ports[i] = name.to_string();
            }
        }
        proc
    }

    pub fn total_received(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Number of configured source ports.
    pub fn num_ports(&self) -> usize {
        self.sources.lock().num_ports
    }

    /// Get the configured source port name for the given index (0-based).
    pub fn source_port(&self, index: usize) -> String {
        if index < MAX_GATHER_PORTS {
            self.sources.lock().ports[index].clone()
        } else {
            String::new()
        }
    }
}

impl Default for GatherProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for GatherProcessor {
    fn process_array(&self, array: &NDArray, _pool: &NDArrayPool) -> ProcessResult {
        self.count.fetch_add(1, Ordering::Relaxed);
        ProcessResult::arrays(vec![Arc::new(array.clone())])
    }

    fn plugin_type(&self) -> &str {
        "NDPluginGather"
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        use asyn_rs::param::ParamType;

        // Register per-port params and store their indices
        for i in 0..MAX_GATHER_PORTS {
            let port_name = format!("GATHER_NDARRAY_PORT_{}", i + 1);
            let addr_name = format!("GATHER_NDARRAY_ADDR_{}", i + 1);
            base.create_param(&port_name, ParamType::Octet)?;
            base.create_param(&addr_name, ParamType::Int32)?;
            self.port_params[i].port_idx = base.find_param(&port_name);
            self.port_params[i].addr_idx = base.find_param(&addr_name);
        }

        // Register aggregate param for number of configured ports
        base.create_param("GATHER_NUM_PORTS", ParamType::Int32)?;
        self.num_ports_idx = base.find_param("GATHER_NUM_PORTS");

        Ok(())
    }

    fn on_param_change(&self, reason: usize, params: &PluginParamSnapshot) -> ParamChangeResult {
        // Check if this is a GATHER_NDARRAY_PORT_N change
        for i in 0..MAX_GATHER_PORTS {
            if Some(reason) == self.port_params[i].port_idx {
                if let Some(new_port) = params.value.as_string() {
                    let mut src = self.sources.lock();
                    src.ports[i] = new_port.to_string();
                    // Recount active ports
                    src.num_ports = src.ports.iter().filter(|s| !s.is_empty()).count();
                    let n = src.num_ports as i32;
                    drop(src);
                    if let Some(idx) = self.num_ports_idx {
                        return ParamChangeResult::updates(vec![ParamUpdate::int32(idx, n)]);
                    }
                }
                return ParamChangeResult::empty();
            }
            if Some(reason) == self.port_params[i].addr_idx {
                self.sources.lock().addrs[i] = params.value.as_i32();
                return ParamChangeResult::empty();
            }
        }

        ParamChangeResult::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::{NDDataType, NDDimension};

    #[test]
    fn test_gather_processor_passthrough() {
        let proc = GatherProcessor::new();
        let pool = NDArrayPool::new(1_000_000);

        let arr1 = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        let arr2 = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);

        let result1 = proc.process_array(&arr1, &pool);
        let result2 = proc.process_array(&arr2, &pool);

        assert_eq!(result1.output_arrays.len(), 1);
        assert_eq!(result2.output_arrays.len(), 1);
        assert_eq!(proc.total_received(), 2);
    }

    #[test]
    fn test_gather_with_ports() {
        let proc = GatherProcessor::with_ports(&["SIM1", "SIM2", "SIM3"]);
        assert_eq!(proc.num_ports(), 3);
        assert_eq!(proc.source_port(0), "SIM1");
        assert_eq!(proc.source_port(1), "SIM2");
        assert_eq!(proc.source_port(2), "SIM3");
        assert_eq!(proc.source_port(3), "");
    }

    #[test]
    fn test_gather_multi_source_counting() {
        let proc = GatherProcessor::with_ports(&["DRV1", "DRV2"]);
        let pool = NDArrayPool::new(1_000_000);

        // Simulate arrays arriving from different sources (all arrive on same channel)
        for _ in 0..5 {
            let arr = NDArray::new(vec![NDDimension::new(10)], NDDataType::UInt16);
            proc.process_array(&arr, &pool);
        }

        assert_eq!(proc.total_received(), 5);
    }

    #[test]
    fn test_gather_default() {
        let proc = GatherProcessor::default();
        assert_eq!(proc.total_received(), 0);
        assert_eq!(proc.num_ports(), 0);
    }

    #[test]
    fn test_gather_max_ports_clamped() {
        // More ports than MAX should be clamped
        let names: Vec<&str> = (0..12).map(|_| "PORT").collect();
        let proc = GatherProcessor::with_ports(&names);
        assert_eq!(proc.num_ports(), MAX_GATHER_PORTS);
    }
}
