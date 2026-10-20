//! NDFileNull: a file plugin that writes nothing, so a driver's own files can
//! be deleted (`DeleteDriverFile`) without being re-written (NDFileNull.cpp:1-4).

use std::path::Path;
use std::sync::Arc;

use ad_core_rs::error::{ADError, ADResult};
use ad_core_rs::ndarray::NDArray;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::file_base::{NDFileOpenMode, NDFileWriter};
use ad_core_rs::plugin::file_controller::{FileControl, FilePluginController};
use ad_core_rs::plugin::runtime::{NDPluginProcess, ProcessResult};
use asyn_rs::port::InterfaceMask;
use parking_lot::Mutex;

/// C `NDFileNull`: open, write and close succeed, read is refused
/// (NDFileNull.cpp:27-56).
#[derive(Default)]
pub struct NullWriter;

impl NDFileWriter for NullWriter {
    fn open_file(&mut self, _path: &Path, _mode: NDFileOpenMode, _array: &NDArray) -> ADResult<()> {
        Ok(())
    }

    fn write_file(&mut self, _array: &Arc<NDArray>) -> ADResult<()> {
        Ok(())
    }

    fn read_file(&mut self) -> ADResult<NDArray> {
        Err(ADError::UnsupportedConversion(
            "NDFileNull cannot read".into(),
        ))
    }

    fn close_file(&mut self) -> ADResult<()> {
        Ok(())
    }

    /// `supportsMultipleArrays = 0` (NDFileNull.cpp:85).
    fn supports_multiple_arrays(&self) -> bool {
        false
    }
}

/// NDFileNull processor wrapping `FilePluginController<NullWriter>`.
pub struct NullFileProcessor {
    ctrl: Mutex<FilePluginController<NullWriter>>,
    /// The plugin's driver, for the posts a capture flush makes.
    attributes: Option<ad_core_rs::plugin::runtime::PluginAttributes>,
}

impl NullFileProcessor {
    pub fn new() -> Self {
        Self {
            ctrl: Mutex::new(FilePluginController::new(NullWriter)),
            attributes: None,
        }
    }
}

impl Default for NullFileProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl NDPluginProcess for NullFileProcessor {
    const ASYN_FLAGS: asyn_rs::port::PortFlags = ad_core_rs::plugin::runtime::ASYN_CANBLOCK;
    // NDFileNull.cpp:78, the same mask for both.
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::GENERIC_POINTER;
    const INTERRUPT_MASK: InterfaceMask = Self::INTERFACE_MASK;

    fn posts_before_write(&self, reason: usize, value: i32) -> bool {
        self.ctrl.lock().posts_before_write(reason, value)
    }

    fn processing_required(&self, array: &NDArray) -> bool {
        self.ctrl.lock().processing_required(array)
    }

    /// C NDPluginFile::writeInt32 (NDPluginFile.cpp:799-870).
    fn validate_int32(
        &self,
        reason: usize,
        _addr: i32,
        value: i32,
        verdict: ad_core_rs::plugin::runtime::WriteVerdict<i32>,
        params: &mut ad_core_rs::plugin::runtime::WritePort<'_>,
    ) -> ad_core_rs::plugin::runtime::WriteOutcome<i32> {
        self.ctrl.lock().write_int32(reason, value, verdict, params)
    }

    /// C NDPluginFile::writeOctet: the setting is stored inside the write.
    fn validate_octet(
        &self,
        reason: usize,
        _addr: i32,
        _value: &str,
        verdict: ad_core_rs::plugin::runtime::WriteVerdict<String>,
        _params: &asyn_rs::port::PortDriverBase,
    ) -> ad_core_rs::plugin::runtime::WriteOutcome<String> {
        self.ctrl.lock().write_octet(reason, verdict)
    }

    fn process_array(&self, array: &Arc<NDArray>, _pool: &NDArrayPool) -> ProcessResult {
        // Made before the controller lock: its reads wait on the actor,
        // which takes that lock in its writes.
        let mut post = ad_core_rs::plugin::runtime::PortPost::of(self.attributes.as_ref());
        self.ctrl.lock().process_array(array, &mut post)
    }

    fn bind_attributes(&mut self, attributes: ad_core_rs::plugin::runtime::PluginAttributes) {
        self.attributes = Some(attributes);
    }

    fn plugin_type(&self) -> &str {
        "NDFileNull"
    }

    /// C `NDPluginFile.cpp:948` sets `NDArrayCallbacks = 0`.
    fn array_callbacks_seed(&self) -> Option<bool> {
        Some(false)
    }

    fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        self.ctrl.lock().register_params(base)
    }
}
