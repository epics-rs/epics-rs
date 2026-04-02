use std::path::PathBuf;
use std::sync::Arc;

use crate::error::{ADError, ADResult};
use crate::ndarray::{NDArray, NDDataType, NDDimension};

use super::file_base::{NDFileMode, NDFileWriter, NDPluginFileBase};
use super::runtime::{
    ParamChangeResult, ParamChangeValue, ParamUpdate, PluginParamSnapshot, ProcessResult,
};

/// Param indices for file plugin control (looked up once at registration time).
#[derive(Default)]
pub struct FileParamIndices {
    pub file_path: Option<usize>,
    pub file_name: Option<usize>,
    pub file_number: Option<usize>,
    pub file_template: Option<usize>,
    pub auto_increment: Option<usize>,
    pub write_file: Option<usize>,
    pub read_file: Option<usize>,
    pub write_mode: Option<usize>,
    pub num_capture: Option<usize>,
    pub capture: Option<usize>,
    pub auto_save: Option<usize>,
    pub create_dir: Option<usize>,
    pub file_path_exists: Option<usize>,
    pub write_status: Option<usize>,
    pub write_message: Option<usize>,
    pub full_file_name: Option<usize>,
    pub file_temp_suffix: Option<usize>,
    pub num_captured: Option<usize>,
}

/// Generic file plugin controller that wraps any NDFileWriter with the full
/// C ADCore NDPluginFile control-plane logic: auto_save, capture, stream,
/// temp_suffix rename, create_dir, param updates, error reporting.
///
/// Each file format plugin (TIFF, HDF5, JPEG) creates one of these and
/// delegates `process_array`, `register_params`, and `on_param_change` to it.
pub struct FilePluginController<W: NDFileWriter> {
    pub file_base: NDPluginFileBase,
    pub writer: W,
    pub params: FileParamIndices,
    pub auto_save: bool,
    pub capture_active: bool,
    pub latest_array: Option<Arc<NDArray>>,
}

impl<W: NDFileWriter> FilePluginController<W> {
    pub fn new(writer: W) -> Self {
        Self {
            file_base: NDPluginFileBase::new(),
            writer,
            params: FileParamIndices::default(),
            auto_save: false,
            capture_active: false,
            latest_array: None,
        }
    }

    /// Look up all standard file param indices from the port driver base.
    pub fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        self.params.file_path = base.find_param("FILE_PATH");
        self.params.file_name = base.find_param("FILE_NAME");
        self.params.file_number = base.find_param("FILE_NUMBER");
        self.params.file_template = base.find_param("FILE_TEMPLATE");
        self.params.auto_increment = base.find_param("AUTO_INCREMENT");
        self.params.write_file = base.find_param("WRITE_FILE");
        self.params.read_file = base.find_param("READ_FILE");
        self.params.write_mode = base.find_param("WRITE_MODE");
        self.params.num_capture = base.find_param("NUM_CAPTURE");
        self.params.capture = base.find_param("CAPTURE");
        self.params.auto_save = base.find_param("AUTO_SAVE");
        self.params.create_dir = base.find_param("CREATE_DIR");
        self.params.file_path_exists = base.find_param("FILE_PATH_EXISTS");
        self.params.write_status = base.find_param("WRITE_STATUS");
        self.params.write_message = base.find_param("WRITE_MESSAGE");
        self.params.full_file_name = base.find_param("FULL_FILE_NAME");
        self.params.file_temp_suffix = base.find_param("FILE_TEMP_SUFFIX");
        self.params.num_captured = base.find_param("NUM_CAPTURED");
        Ok(())
    }

    /// Process an incoming array: auto_save, capture buffering, stream write.
    pub fn process_array(&mut self, array: &NDArray) -> ProcessResult {
        let mut proc_result = ProcessResult::empty();
        let array = Arc::new(array.clone());
        self.latest_array = Some(array.clone());

        let result = match self.file_base.mode() {
            NDFileMode::Single => {
                if self.auto_save {
                    self.write_single(array)
                } else {
                    Ok(())
                }
            }
            NDFileMode::Capture => {
                if self.capture_active {
                    self.file_base.capture_array(array);
                    self.push_num_captured_update(&mut proc_result.param_updates);
                    if self.file_base.num_captured() >= self.file_base.num_capture_target() {
                        if self.auto_save {
                            if let Err(err) = self.file_base.flush_capture(&mut self.writer) {
                                Err(err)
                            } else {
                                self.push_full_file_name_update(&mut proc_result.param_updates);
                                self.push_num_captured_update(&mut proc_result.param_updates);
                                self.capture_active = false;
                                Ok(())
                            }
                        } else {
                            self.capture_active = false;
                            Ok(())
                        }
                    } else {
                        Ok(())
                    }
                } else {
                    Ok(())
                }
            }
            NDFileMode::Stream => {
                if self.capture_active {
                    let r = self.file_base.process_array(array, &mut self.writer);
                    let target = self.file_base.num_capture_target();
                    if r.is_ok() && target > 0 && self.file_base.num_captured() >= target {
                        if let Err(e) = self.file_base.close_stream(&mut self.writer) {
                            return ProcessResult::sink(self.error_updates(
                                false,
                                false,
                                e.to_string(),
                            ));
                        }
                        self.capture_active = false;
                        self.push_full_file_name_update(&mut proc_result.param_updates);
                        self.push_num_captured_update(&mut proc_result.param_updates);
                    }
                    r
                } else {
                    Ok(())
                }
            }
        };

        if result.is_ok() {
            proc_result.param_updates.extend(self.success_updates());
            if self.file_base.mode() == NDFileMode::Single && self.auto_save {
                self.push_full_file_name_update(&mut proc_result.param_updates);
            }
            if self.file_base.mode() == NDFileMode::Stream && self.capture_active {
                self.push_full_file_name_update(&mut proc_result.param_updates);
            }
        } else if let Err(err) = result {
            proc_result.param_updates = self.error_updates(false, false, err.to_string());
        }
        proc_result
    }

    /// Handle a control-plane param change. Returns true if the reason was handled.
    pub fn on_param_change(
        &mut self,
        reason: usize,
        params: &PluginParamSnapshot,
    ) -> ParamChangeResult {
        let mut updates = Vec::new();

        if Some(reason) == self.params.file_path {
            if let ParamChangeValue::Octet(s) = &params.value {
                let normalized = normalize_file_path(s);
                self.file_base.file_path = normalized.clone();
                let exists =
                    std::path::Path::new(normalized.trim_end_matches(std::path::MAIN_SEPARATOR))
                        .is_dir();
                if let Some(idx) = self.params.file_path_exists {
                    updates.push(ParamUpdate::Int32 {
                        reason: idx,
                        addr: 0,
                        value: if exists { 1 } else { 0 },
                    });
                }
            }
        } else if Some(reason) == self.params.file_name {
            if let ParamChangeValue::Octet(s) = &params.value {
                self.file_base.file_name = s.clone();
            }
        } else if Some(reason) == self.params.file_number {
            self.file_base.file_number = params.value.as_i32();
        } else if Some(reason) == self.params.file_template {
            if let ParamChangeValue::Octet(s) = &params.value {
                self.file_base.file_template = s.clone();
            }
        } else if Some(reason) == self.params.auto_increment {
            self.file_base.auto_increment = params.value.as_i32() != 0;
        } else if Some(reason) == self.params.auto_save {
            self.auto_save = params.value.as_i32() != 0;
        } else if Some(reason) == self.params.write_mode {
            self.file_base
                .set_mode(NDFileMode::from_i32(params.value.as_i32()));
        } else if Some(reason) == self.params.num_capture {
            self.file_base
                .set_num_capture(params.value.as_i32().max(1) as usize);
        } else if Some(reason) == self.params.create_dir {
            self.file_base.create_dir = params.value.as_i32();
        } else if Some(reason) == self.params.file_temp_suffix {
            if let ParamChangeValue::Octet(s) = &params.value {
                self.file_base.temp_suffix = s.clone();
            }
        } else if Some(reason) == self.params.write_file {
            if params.value.as_i32() != 0 {
                let result = match self.file_base.mode() {
                    NDFileMode::Single => {
                        if let Some(array) = self.latest_array.clone() {
                            self.write_single(array)
                        } else {
                            Err(ADError::UnsupportedConversion(
                                "no array available for write".into(),
                            ))
                        }
                    }
                    NDFileMode::Capture => self.file_base.flush_capture(&mut self.writer),
                    NDFileMode::Stream => {
                        if let Some(array) = self.latest_array.clone() {
                            self.file_base.process_array(array, &mut self.writer)
                        } else {
                            Err(ADError::UnsupportedConversion(
                                "no array available for write".into(),
                            ))
                        }
                    }
                };
                match result {
                    Ok(()) => {
                        updates.extend(self.success_updates());
                        self.push_num_captured_update(&mut updates);
                        self.push_full_file_name_update(&mut updates);
                    }
                    Err(err) => {
                        return ParamChangeResult::updates(self.error_updates(
                            false,
                            true,
                            err.to_string(),
                        ));
                    }
                }
            }
        } else if Some(reason) == self.params.read_file {
            if params.value.as_i32() != 0 {
                let result = (|| -> ADResult<Arc<NDArray>> {
                    let path = PathBuf::from(self.file_base.create_file_name());
                    self.writer.open_file(
                        &path,
                        NDFileMode::Single,
                        &NDArray::new(vec![NDDimension::new(1)], NDDataType::UInt8),
                    )?;
                    let array = Arc::new(self.writer.read_file()?);
                    self.writer.close_file()?;
                    self.latest_array = Some(array.clone());
                    Ok(array)
                })();
                match result {
                    Ok(array) => {
                        updates.extend(self.success_updates());
                        self.push_full_file_name_update(&mut updates);
                        return ParamChangeResult::combined(vec![array], updates);
                    }
                    Err(err) => {
                        return ParamChangeResult::updates(self.error_updates(
                            true,
                            false,
                            err.to_string(),
                        ));
                    }
                }
            }
        } else if Some(reason) == self.params.capture {
            if params.value.as_i32() != 0 {
                match self.file_base.mode() {
                    NDFileMode::Single => {
                        self.capture_active = false;
                        return ParamChangeResult::updates(self.error_updates(
                            false,
                            false,
                            "ERROR: capture not supported in Single mode".into(),
                        ));
                    }
                    NDFileMode::Capture => {
                        self.file_base.clear_capture();
                        self.capture_active = true;
                        self.push_num_captured_update(&mut updates);
                    }
                    NDFileMode::Stream => {
                        self.capture_active = true;
                        self.push_num_captured_update(&mut updates);
                    }
                }
            } else {
                if self.file_base.mode() == NDFileMode::Stream {
                    if let Err(err) = self.file_base.close_stream(&mut self.writer) {
                        return ParamChangeResult::updates(self.error_updates(
                            false,
                            false,
                            err.to_string(),
                        ));
                    }
                }
                self.capture_active = false;
            }
        }

        ParamChangeResult::updates(updates)
    }

    // ── helpers ──

    fn write_single(&mut self, array: Arc<NDArray>) -> ADResult<()> {
        self.file_base.ensure_directory()?;
        self.file_base.process_array(array, &mut self.writer)
    }

    fn success_updates(&self) -> Vec<ParamUpdate> {
        let mut updates = Vec::new();
        if let Some(idx) = self.params.file_number {
            updates.push(ParamUpdate::Int32 {
                reason: idx,
                addr: 0,
                value: self.file_base.file_number,
            });
        }
        if let Some(idx) = self.params.write_status {
            updates.push(ParamUpdate::Int32 {
                reason: idx,
                addr: 0,
                value: 0,
            });
        }
        if let Some(idx) = self.params.write_message {
            updates.push(ParamUpdate::Octet {
                reason: idx,
                addr: 0,
                value: String::new(),
            });
        }
        if let Some(idx) = self.params.write_file {
            updates.push(ParamUpdate::Int32 {
                reason: idx,
                addr: 0,
                value: 0,
            });
        }
        if let Some(idx) = self.params.capture {
            updates.push(ParamUpdate::Int32 {
                reason: idx,
                addr: 0,
                value: if self.capture_active { 1 } else { 0 },
            });
        }
        if let Some(idx) = self.params.read_file {
            updates.push(ParamUpdate::Int32 {
                reason: idx,
                addr: 0,
                value: 0,
            });
        }
        updates
    }

    fn push_num_captured_update(&self, updates: &mut Vec<ParamUpdate>) {
        if let Some(idx) = self.params.num_captured {
            updates.push(ParamUpdate::Int32 {
                reason: idx,
                addr: 0,
                value: self.file_base.num_captured() as i32,
            });
        }
    }

    fn push_full_file_name_update(&self, updates: &mut Vec<ParamUpdate>) {
        if let Some(idx) = self.params.full_file_name {
            updates.push(ParamUpdate::Octet {
                reason: idx,
                addr: 0,
                value: self.file_base.create_file_name(),
            });
        }
    }

    fn error_updates(
        &self,
        read_reason: bool,
        write_reason: bool,
        message: String,
    ) -> Vec<ParamUpdate> {
        let mut updates = Vec::new();
        if write_reason {
            if let Some(idx) = self.params.write_file {
                updates.push(ParamUpdate::Int32 {
                    reason: idx,
                    addr: 0,
                    value: 0,
                });
            }
        }
        if read_reason {
            if let Some(idx) = self.params.read_file {
                updates.push(ParamUpdate::Int32 {
                    reason: idx,
                    addr: 0,
                    value: 0,
                });
            }
        }
        if let Some(idx) = self.params.write_status {
            updates.push(ParamUpdate::Int32 {
                reason: idx,
                addr: 0,
                value: 1,
            });
        }
        if let Some(idx) = self.params.write_message {
            updates.push(ParamUpdate::Octet {
                reason: idx,
                addr: 0,
                value: message,
            });
        }
        updates
    }
}

fn normalize_file_path(path: &str) -> String {
    if path.is_empty() || path.ends_with(std::path::MAIN_SEPARATOR) {
        path.to_string()
    } else {
        format!("{path}{}", std::path::MAIN_SEPARATOR)
    }
}
