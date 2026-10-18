use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};
use asyn_rs::port::InterfaceMask;
use parking_lot::Mutex;
use std::sync::Arc;

/// Scatter method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScatterMethod {
    RoundRobin = 0,
}

impl ScatterMethod {
    pub fn from_i32(_v: i32) -> Self {
        Self::RoundRobin
    }
}

/// Scatter processor: marks each frame to be distributed to a single
/// downstream consumer in round-robin order.
///
/// The target consumer — and the reroute-past-a-full-queue / drop-only-on-the-
/// last-node decisions — are owned by the plugin runtime delivery path, which
/// holds the persistent cursor (C++ `NDPluginScatter::nextClient_`) and is the
/// only layer that can see per-consumer queue state. The processor therefore
/// only flags the frame as a scatter frame, matching C++ `NDPluginScatter`,
/// which keeps no per-frame index of its own (the cursor lives entirely in
/// `nextClient_`).
pub struct ScatterProcessor {
    /// C `NDPluginScatter` has no per-frame method state; the PV write and the
    /// frame path can run on different threads, so the cached value is locked.
    method: Mutex<ScatterMethod>,
    method_idx: Option<usize>,
}

impl ScatterProcessor {
    pub fn new() -> Self {
        Self {
            method: Mutex::new(ScatterMethod::RoundRobin),
            method_idx: None,
        }
    }
}

impl Default for ScatterProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for ScatterProcessor {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_MULTIDEVICE;
    // NDPluginScatter.cpp:117-118, the same mask for both.
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::INT32_ARRAY
        .or(InterfaceMask::FLOAT64)
        .or(InterfaceMask::FLOAT64_ARRAY)
        .or(InterfaceMask::GENERIC_POINTER);
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;
    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        ProcessResult::scatter(array)
    }

    fn plugin_type(&self) -> &str {
        "NDPluginScatter"
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        use asyn_rs::param::ParamType;
        base.create_param("SCATTER_METHOD", ParamType::Int32)?;
        self.method_idx = base.find_param("SCATTER_METHOD");
        Ok(())
    }

    fn on_param_change(
        &self,
        reason: usize,
        params: &ad_core_rs::plugin::runtime::PluginParamSnapshot,
    ) -> ad_core_rs::plugin::runtime::ParamChangeResult {
        if Some(reason) == self.method_idx {
            *self.method.lock() = ScatterMethod::from_i32(params.value.as_i32());
        }
        ad_core_rs::plugin::runtime::ParamChangeResult::updates(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core_rs::ndarray::{NDDataType, NDDimension};

    /// C `getAttributes` on the copy it scatters (NDPluginScatter.cpp:38-40),
    /// which the runtime's `endProcessCallbacks` evaluation is here.
    #[test]
    fn a_scattered_frame_carries_the_plugin_attributes() {
        use crate::test_util::{publish, with_attributes_file};
        use ad_core_rs::attributes::NDAttrValue;
        let xml = r#"<Attributes>
            <Attribute name="Site" type="CONST" source="BL7"/>
        </Attributes>"#;
        let (handle, mut rx, _jh) =
            with_attributes_file("SCATTER_ATTRS", ScatterProcessor::new(), xml);
        let arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        publish(&handle, &arr);
        let out = rx.blocking_recv().unwrap();
        assert_eq!(
            out.attributes.get("Site").map(|a| a.value.clone()),
            Some(NDAttrValue::String("BL7".into()))
        );
    }

    #[test]
    fn test_scatter_processor_marks_scatter_frame() {
        // The processor flags every frame as a scatter frame and passes the
        // array through unchanged; the runtime owns the round-robin cursor and
        // the reroute/drop decisions (see ad_core_rs runtime scatter_publish
        // tests for the routing behavior itself).
        let proc = ScatterProcessor::new();
        let pool = NDArrayPool::new(1_000_000);

        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = 42;
        let arr = Arc::new(arr);

        for _ in 0..4 {
            let r = proc.process_array(&arr, &pool);
            assert!(
                r.scatter,
                "scatter processor must mark the frame as scatter"
            );
            assert!(
                Arc::ptr_eq(&r.output_arrays[0], &arr),
                "the input frame itself is scattered"
            );
            assert_eq!(r.output_arrays.len(), 1);
        }
    }
}
