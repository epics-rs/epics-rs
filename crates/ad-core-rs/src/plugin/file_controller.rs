use std::path::PathBuf;
use std::sync::Arc;

use crate::error::{ADError, ADResult};
use crate::finalize::Finalize;
use crate::ndarray::{NDArray, NDDataType, NDDimension};

use super::file_base::{NDFileMode, NDFileWriter, NDPluginFileBase, with_open_file};
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
    pub lazy_open: Option<usize>,
    pub delete_driver_file: Option<usize>,
    pub free_capture: Option<usize>,
    /// ARRAY_COUNTER — overridden by the file plugin so it counts only
    /// saved frames, not every callback (G10).
    pub array_counter: Option<usize>,
}

/// Generic file plugin controller that wraps any NDFileWriter with the full
/// C ADCore NDPluginFile control-plane logic: auto_save, capture, stream,
/// temp_suffix rename, create_dir, param updates, error reporting.
///
/// Each file format plugin (TIFF, HDF5, JPEG) creates one of these and
/// delegates `process_array`, `register_params`, and `on_param_change` to it.
///
/// # Capture state invariant (B8)
///
/// MUST: `capture_active`, `file_base.capture_buffer` / `file_base.is_open`,
/// `file_base.num_captured` and the `CAPTURE` PV are only mutated together,
/// and only through `start_capture()` / `stop_capture()`. No other method may
/// flip `capture_active` directly. This makes capture start/stop a single
/// owned transition so a mode switch or buffer-full event cannot leave the
/// state inconsistent (e.g. a stream open while `capture_active` is false).
pub struct FilePluginController<W: NDFileWriter> {
    pub file_base: NDPluginFileBase,
    pub writer: W,
    pub params: FileParamIndices,
    pub auto_save: bool,
    /// Capture/stream-in-progress flag. INVARIANT: only `start_capture` /
    /// `stop_capture` may write this (B8).
    pub capture_active: bool,
    pub lazy_open: bool,
    pub delete_driver_file: bool,
    pub latest_array: Option<Arc<NDArray>>,
    /// Recorded dimensions of the first captured/streamed frame, for
    /// `isFrameValid` validation (G12 — applies to Capture and Stream).
    stream_dims: Option<Vec<usize>>,
    /// Recorded data type of the first captured/streamed frame.
    stream_data_type: Option<NDDataType>,
    /// This plugin's asyn port name, for FilePluginDestination matching (G9).
    port_name: String,
    /// Count of frames actually saved to disk. G10: ArrayCounter on a file
    /// plugin must count saved frames, not every callback — the runtime bumps
    /// ArrayCounter per callback, so the controller overrides it with this.
    saved_frames: i32,
}

impl<W: NDFileWriter> FilePluginController<W> {
    pub fn new(writer: W) -> Self {
        Self {
            file_base: NDPluginFileBase::new(),
            writer,
            params: FileParamIndices::default(),
            auto_save: false,
            capture_active: false,
            lazy_open: false,
            delete_driver_file: false,
            latest_array: None,
            stream_dims: None,
            stream_data_type: None,
            port_name: String::new(),
            saved_frames: 0,
        }
    }

    /// Set this plugin's asyn port name (used for FilePluginDestination
    /// routing, G9). Called once during plugin construction.
    pub fn set_port_name(&mut self, name: impl Into<String>) {
        self.port_name = name.into();
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
        self.params.lazy_open = base.find_param("FILE_LAZY_OPEN");
        self.params.delete_driver_file = base.find_param("DELETE_DRIVER_FILE");
        self.params.free_capture = base.find_param("FREE_CAPTURE");
        self.params.array_counter = base.find_param("ARRAY_COUNTER");
        Ok(())
    }

    // ── capture state owner (B8) ──

    /// Start capture/stream (single owner of the capture-state transition).
    ///
    /// B8: clears the capture buffer, resets validation state, sets
    /// `capture_active`, and emits the `CAPTURE`/`NUM_CAPTURED` PV updates.
    /// B9: for a non-lazy Stream plugin whose writer supports multiple arrays
    /// the file is opened eagerly here (C++ `doCapture`) so a bad path is
    /// reported at capture-start; a lazy plugin defers to the first frame.
    fn start_capture(&mut self, updates: &mut Vec<ParamUpdate>) -> ADResult<()> {
        self.file_base.clear_capture();
        self.stream_dims = None;
        self.stream_data_type = None;
        self.file_base.lazy_open = self.lazy_open;
        self.file_base.delete_driver_file = self.delete_driver_file;

        if self.file_base.mode() == NDFileMode::Stream
            && !self.lazy_open
            && self.writer.supports_multiple_arrays()
        {
            // B9: eager open — needs a frame to know the layout.
            if let Some(array) = self.latest_array.clone() {
                self.file_base.open_stream(&mut self.writer, &array)?;
            }
        }
        self.capture_active = true;
        self.push_capture_update(updates);
        self.push_num_captured_update(updates);
        Ok(())
    }

    /// Stop capture/stream (single owner of the capture-state transition).
    ///
    /// B8/B17: closes any open stream file, clears `capture_active`, and
    /// emits the `CAPTURE` PV update. Idempotent.
    ///
    /// The B8 invariant — that `capture_active`, the file-base capture state
    /// and the `CAPTURE` PV move together — holds for the failing close as
    /// well: the finalizer performs the whole transition on every exit path, so
    /// a `close_stream` error is reported without latching `Capture_RBV` at 1
    /// against a plugin that is no longer capturing.
    fn stop_capture(&mut self, updates: &mut Vec<ParamUpdate>) -> ADResult<()> {
        let mut stopping = Finalize::new(self, |ctrl: &mut Self| {
            ctrl.capture_active = false;
            ctrl.stream_dims = None;
            ctrl.stream_data_type = None;
            ctrl.push_capture_update(updates);
        });
        stopping.run(|ctrl| {
            if ctrl.file_base.mode() == NDFileMode::Stream {
                ctrl.file_base.close_stream(&mut ctrl.writer)?;
            }
            Ok(())
        })
    }

    /// `isFrameValid` (C++ NDPluginFile.cpp:665): a frame is valid if its
    /// dimensions and data type match the first frame of the capture/stream.
    /// G12: applies to Capture mode as well as Stream.
    fn frame_valid(&mut self, array: &NDArray) -> bool {
        let frame_dims: Vec<usize> = array.dims.iter().map(|d| d.size).collect();
        let frame_dtype = array.data.data_type();
        match (&self.stream_dims, self.stream_data_type) {
            (Some(dims), Some(dtype)) => &frame_dims == dims && frame_dtype == dtype,
            _ => {
                self.stream_dims = Some(frame_dims);
                self.stream_data_type = Some(frame_dtype);
                true
            }
        }
    }

    /// G9: decide whether this plugin should process the frame, based on the
    /// `FilePluginDestination` attribute (C++ `attrIsProcessingRequired`).
    /// If the attribute is set and is neither "all" nor this plugin's port
    /// name, the frame is not for this plugin.
    fn destination_matches(&self, array: &NDArray) -> bool {
        match array
            .attributes
            .get("FilePluginDestination")
            .and_then(|attr| attr.value.as_string_typed())
        {
            // C `getValueInfo` only runs the compare when the attribute is
            // string-typed; a numeric attribute is read as `ND_ERROR` and the
            // frame is processed (treated as "no destination set").
            Some(dest) => {
                // C runs the compare for any non-empty string (getValueInfo
                // size = strlen+1, guarded `> 1`).
                if dest.is_empty() {
                    return true;
                }
                // C tests "all" with `epicsStrnCaseCmp(dest,"all",min(len,3))`
                // — a 3-char (or shorter, for 1-2 char dest) prefix match — and
                // the port name with a full-length compare.
                let prefix = dest.len().min(3);
                let matches_all = dest.as_bytes()[..prefix].eq_ignore_ascii_case(&b"all"[..prefix]);
                matches_all || dest.eq_ignore_ascii_case(&self.port_name)
            }
            None => true,
        }
    }

    /// Re-evaluate whether the current `FilePath` directory exists and push a
    /// `FilePathExists` param update. C++ `NDPluginFile::checkPath()` runs this
    /// before each write, not only when FilePath changes.
    fn refresh_file_path_exists(&mut self, updates: &mut Vec<ParamUpdate>) {
        let idx = match self.params.file_path_exists {
            Some(idx) => idx,
            None => return,
        };
        let (normalized, exists) = check_file_path(&self.file_base.file_path);
        self.file_base.file_path = normalized;
        updates.push(ParamUpdate::Int32 {
            reason: idx,
            addr: 0,
            value: if exists { 1 } else { 0 },
        });
    }

    /// G9: apply the `FilePluginFileName` / `FilePluginFileNumber` attributes
    /// to the file base, returning param updates for the changed PVs and
    /// whether a mid-stream file reopen is required (C++ `attrFileNameSet` /
    /// `attrFileNameCheck`).
    fn apply_filename_attributes(
        &mut self,
        array: &NDArray,
        updates: &mut Vec<ParamUpdate>,
    ) -> bool {
        let mut reopen = false;
        // C `attrFileNameSet` guards on `attrDataType == NDAttrString` before
        // touching NDFileName; a numeric FilePluginFileName attribute is ignored
        // (the filename is not redefined to its decimal rendering).
        if let Some(name) = array
            .attributes
            .get("FilePluginFileName")
            .and_then(|attr| attr.value.as_string_typed())
        {
            if !name.is_empty() && name != self.file_base.file_name {
                self.file_base.file_name = name.to_string();
                reopen = true;
                if let Some(idx) = self.params.file_name {
                    updates.push(ParamUpdate::Octet {
                        reason: idx,
                        addr: 0,
                        value: name.to_string(),
                    });
                }
            }
        }
        if let Some(attr) = array.attributes.get("FilePluginFileNumber") {
            if let Some(num) = attr.value.as_i64() {
                let num = num as i32;
                if num != self.file_base.file_number {
                    self.file_base.file_number = num;
                    self.file_base.auto_increment = false; // C parity
                    reopen = true;
                    if let Some(idx) = self.params.file_number {
                        updates.push(ParamUpdate::Int32 {
                            reason: idx,
                            addr: 0,
                            value: num,
                        });
                    }
                }
            }
        }
        reopen
    }

    /// Process an incoming array: auto_save, capture buffering, stream write.
    pub fn process_array(&mut self, array: &NDArray) -> ProcessResult {
        let mut proc_result = ProcessResult::empty();
        let array = Arc::new(array.clone());
        self.latest_array = Some(array.clone());

        // G9: FilePluginDestination routing — skip frames not for this plugin.
        if !self.destination_matches(&array) {
            return proc_result;
        }

        // Re-check file-path existence before every write. C++ NDPluginFile
        // calls checkPath() before each write so a directory deleted after
        // FilePath was set is reflected; the Rust port previously updated
        // FilePathExists only on the FilePath param change.
        self.refresh_file_path_exists(&mut proc_result.param_updates);

        // G9: FilePluginClose attribute forces an immediate file close.
        let force_close = array
            .attributes
            .get("FilePluginClose")
            .and_then(|a| a.value.as_i64())
            .map(|v| v != 0)
            .unwrap_or(false);
        if force_close {
            if let Err(e) = self.file_base.force_close(&mut self.writer) {
                return ProcessResult::sink(self.error_updates(false, false, e.to_string()));
            }
            let _ = self.stop_capture(&mut proc_result.param_updates);
            return proc_result;
        }

        let result = match self.file_base.mode() {
            NDFileMode::Single => {
                if self.auto_save {
                    let r = self.write_single(array);
                    if r.is_ok() {
                        self.saved_frames += 1; // G10: count saved frame
                    }
                    r
                } else {
                    Ok(())
                }
            }
            NDFileMode::Capture => {
                if self.capture_active {
                    // G12: validate frame dims/dtype in Capture mode too.
                    if !self.frame_valid(&array) {
                        return proc_result;
                    }
                    self.file_base.capture_array(array);
                    self.push_num_captured_update(&mut proc_result.param_updates);
                    let target = self.file_base.num_capture_target();
                    if target > 0 && self.file_base.num_captured() >= target {
                        if self.auto_save {
                            let to_save = self.file_base.num_captured() as i32;
                            if let Err(err) = self.file_base.flush_capture(&mut self.writer) {
                                Err(err)
                            } else {
                                self.saved_frames += to_save; // G10
                                self.push_full_file_name_update(&mut proc_result.param_updates);
                                self.push_num_captured_update(&mut proc_result.param_updates);
                                self.stop_capture(&mut proc_result.param_updates).ok();
                                Ok(())
                            }
                        } else {
                            self.stop_capture(&mut proc_result.param_updates).ok();
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
                    // G12: validate frame dims/dtype against the first frame.
                    if !self.frame_valid(&array) {
                        return proc_result;
                    }
                    // G9: attribute-driven filename override / mid-stream reopen.
                    let reopen =
                        self.apply_filename_attributes(&array, &mut proc_result.param_updates);
                    if reopen && self.file_base.is_open() {
                        if let Err(e) = self.file_base.force_close(&mut self.writer) {
                            return ProcessResult::sink(self.error_updates(
                                false,
                                false,
                                e.to_string(),
                            ));
                        }
                    }
                    let r = self.file_base.process_array(array, &mut self.writer);
                    if r.is_ok() {
                        self.saved_frames += 1; // G10: count saved frame
                        // C++ NDPluginFile::processCallbacks publishes
                        // NDFileNumCaptured after every successful stream
                        // write — without this an unbounded stream
                        // (NumCapture=0) reports NumCaptured_RBV=0 forever.
                        self.push_num_captured_update(&mut proc_result.param_updates);
                    }
                    let target = self.file_base.num_capture_target();
                    if r.is_ok() && target > 0 && self.file_base.num_captured() >= target {
                        if let Err(e) = self.file_base.close_stream(&mut self.writer) {
                            return ProcessResult::sink(self.error_updates(
                                false,
                                false,
                                e.to_string(),
                            ));
                        }
                        self.stop_capture(&mut proc_result.param_updates).ok();
                        self.push_full_file_name_update(&mut proc_result.param_updates);
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
            // G10: override ArrayCounter (the runtime bumped it per callback)
            // with the saved-frame count so a file plugin's ArrayCounter_RBV
            // reflects frames actually written, not callbacks received.
            if let Some(idx) = self.params.array_counter {
                proc_result.param_updates.push(ParamUpdate::Int32 {
                    reason: idx,
                    addr: 0,
                    value: self.saved_frames,
                });
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
                let (normalized, exists) = check_file_path(s);
                self.file_base.file_path = normalized;
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
            // B17: WriteMode transitions are gated through stop_capture so a
            // mode switch mid-capture cannot leave a stream file open.
            let new_mode = NDFileMode::from_i32(params.value.as_i32());
            if self.capture_active && new_mode != self.file_base.mode() {
                if let Err(e) = self.stop_capture(&mut updates) {
                    return ParamChangeResult::updates(self.error_updates(
                        false,
                        false,
                        e.to_string(),
                    ));
                }
            }
            self.file_base.set_mode(new_mode);
        } else if Some(reason) == self.params.num_capture {
            // B7: numCapture==0 means "capture forever" — C++ buffers
            // indefinitely. Do not force a minimum of 1.
            self.file_base
                .set_num_capture(params.value.as_i32().max(0) as usize);
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
                    let layout = NDArray::new(vec![NDDimension::new(1)], NDDataType::UInt8);
                    let array = with_open_file(
                        &mut self.writer,
                        &path,
                        NDFileMode::Single,
                        &layout,
                        |w| w.read_file().map(Arc::new),
                    )?;
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
        } else if Some(reason) == self.params.lazy_open {
            self.lazy_open = params.value.as_i32() != 0;
        } else if Some(reason) == self.params.delete_driver_file {
            self.delete_driver_file = params.value.as_i32() != 0;
        } else if Some(reason) == self.params.free_capture {
            if params.value.as_i32() != 0 {
                self.file_base.clear_capture();
                self.push_num_captured_update(&mut updates);
            }
        } else if Some(reason) == self.params.capture {
            // B8: capture start/stop routes through the single owner.
            if params.value.as_i32() != 0 {
                if self.file_base.mode() == NDFileMode::Single {
                    // Capture is invalid in Single mode — leave it stopped.
                    let _ = self.stop_capture(&mut updates);
                    return ParamChangeResult::updates(self.error_updates(
                        false,
                        false,
                        "ERROR: capture not supported in Single mode".into(),
                    ));
                }
                if let Err(e) = self.start_capture(&mut updates) {
                    return ParamChangeResult::updates(self.error_updates(
                        false,
                        false,
                        e.to_string(),
                    ));
                }
            } else if let Err(e) = self.stop_capture(&mut updates) {
                return ParamChangeResult::updates(self.error_updates(false, false, e.to_string()));
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
        self.push_capture_update(&mut updates);
        if let Some(idx) = self.params.read_file {
            updates.push(ParamUpdate::Int32 {
                reason: idx,
                addr: 0,
                value: 0,
            });
        }
        updates
    }

    /// Emit the `CAPTURE` PV update reflecting the current `capture_active`
    /// flag (B8 — the single owner of the capture-state transition).
    fn push_capture_update(&self, updates: &mut Vec<ParamUpdate>) {
        if let Some(idx) = self.params.capture {
            updates.push(ParamUpdate::Int32 {
                reason: idx,
                addr: 0,
                value: if self.capture_active { 1 } else { 0 },
            });
        }
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
                value: self.file_base.last_written_name().to_string(),
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
        // B8: an error return replaces whatever updates the caller accumulated,
        // so the capture state has to be re-derived here or a `stop_capture`
        // that failed would report its error with `Capture_RBV` still reading 1.
        self.push_capture_update(&mut updates);
        updates
    }
}

/// Normalize `path` and report whether the directory exists, through the one
/// implementation of C++ `asynNDArrayDriver::checkPath` that the file plugins
/// inherit (`NDPluginFile` -> `NDPluginDriver` -> `asynNDArrayDriver`).
fn check_file_path(path: &str) -> (String, bool) {
    let mut normalized = path.to_string();
    let exists = crate::driver::ndarray_driver::check_path_str(&mut normalized);
    (normalized, exists)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
    use crate::ndarray::{NDArray, NDDataType, NDDimension};
    use std::path::Path;

    /// Mock writer that records operations.
    struct MockWriter {
        opens: usize,
        writes: usize,
        closes: usize,
        multi: bool,
        /// Make `close_file` fail, standing in for the ENOSPC / read-only-mount
        /// close that a real writer cannot complete.
        fail_close: bool,
    }
    impl MockWriter {
        fn new(multi: bool) -> Self {
            Self {
                opens: 0,
                writes: 0,
                closes: 0,
                multi,
                fail_close: false,
            }
        }
    }
    impl NDFileWriter for MockWriter {
        fn open_file(&mut self, _p: &Path, _m: NDFileMode, _a: &NDArray) -> ADResult<()> {
            self.opens += 1;
            Ok(())
        }
        fn write_file(&mut self, _a: &NDArray) -> ADResult<()> {
            self.writes += 1;
            Ok(())
        }
        fn read_file(&mut self) -> ADResult<NDArray> {
            Err(ADError::UnsupportedConversion("n/a".into()))
        }
        fn close_file(&mut self) -> ADResult<()> {
            self.closes += 1;
            if self.fail_close {
                return Err(ADError::UnsupportedConversion("disk full".into()));
            }
            Ok(())
        }
        fn supports_multiple_arrays(&self) -> bool {
            self.multi
        }
    }

    fn array(id: i32) -> NDArray {
        let mut a = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        a.unique_id = id;
        a
    }

    fn with_str_attr(mut a: NDArray, name: &str, val: &str) -> NDArray {
        a.attributes.add(NDAttribute::new_static(
            name,
            "",
            NDAttrSource::Driver,
            NDAttrValue::String(val.to_string()),
        ));
        a
    }

    fn with_i32_attr(mut a: NDArray, name: &str, val: i32) -> NDArray {
        a.attributes.add(NDAttribute::new_static(
            name,
            "",
            NDAttrSource::Driver,
            NDAttrValue::Int32(val),
        ));
        a
    }

    #[test]
    fn test_g9_destination_routing_skips_other_port() {
        // G9: a frame addressed to another plugin via FilePluginDestination
        // is not written by this plugin.
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("MYFILE");
        c.file_base.set_mode(NDFileMode::Single);
        c.auto_save = true;

        // Destination = a different port → skipped.
        c.process_array(&with_str_attr(array(1), "FilePluginDestination", "OTHER"));
        assert_eq!(c.writer.writes, 0, "frame for OTHER port must be skipped");

        // Destination = this port → written.
        c.process_array(&with_str_attr(array(2), "FilePluginDestination", "MYFILE"));
        assert_eq!(c.writer.writes, 1);

        // Destination = "all" → written.
        c.process_array(&with_str_attr(array(3), "FilePluginDestination", "all"));
        assert_eq!(c.writer.writes, 2);
    }

    #[test]
    fn test_g9_numeric_destination_attr_processed_not_stringified() {
        // ADC-11 parity: C `attrIsProcessingRequired` only compares a
        // *string-typed* FilePluginDestination (getValueInfo guard); a numeric
        // attribute reads as ND_ERROR and the frame is processed. The Rust port
        // must not stringify the numeric (42 -> "42") and then skip the frame.
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("MYFILE");
        c.file_base.set_mode(NDFileMode::Single);
        c.auto_save = true;

        c.process_array(&with_i32_attr(array(1), "FilePluginDestination", 42));
        assert_eq!(
            c.writer.writes, 1,
            "numeric FilePluginDestination must be ignored (frame processed), \
             not stringified to \"42\" and skipped"
        );
    }

    #[test]
    fn test_g9_numeric_filename_attr_ignored() {
        // ADC-11 parity: C `attrFileNameSet` only adopts a string-typed
        // FilePluginFileName; a numeric attribute leaves NDFileName unchanged
        // and forces no reopen.
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        let before = c.file_base.file_name.clone();
        let mut updates = Vec::new();
        let reopen = c.apply_filename_attributes(
            &with_i32_attr(array(1), "FilePluginFileName", 7),
            &mut updates,
        );
        assert!(
            !reopen,
            "numeric FilePluginFileName must not force a reopen"
        );
        assert_eq!(
            c.file_base.file_name, before,
            "numeric FilePluginFileName must not redefine the filename"
        );
        assert!(
            updates.is_empty(),
            "numeric FilePluginFileName must not post a FileName param update"
        );
    }

    #[test]
    fn test_g9_destination_compare_matches_c_attr_is_processing_required() {
        // ADC-12: replicate C `attrIsProcessingRequired` comparison exactly —
        // "all" is a 3-char prefix match and any non-empty string is compared
        // (no blanket `len <= 1 -> process`).
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("MYFILE");
        c.file_base.set_mode(NDFileMode::Single);
        c.auto_save = true;

        // "all" 3-char prefix (C: epicsStrnCaseCmp(dest,"all",3)==0) → processed.
        c.process_array(&with_str_attr(array(1), "FilePluginDestination", "allfoo"));
        assert_eq!(
            c.writer.writes, 1,
            "destination with \"all\" prefix is processed (C 3-char prefix match)"
        );

        // Non-empty 1-char destination that is neither an "all" prefix nor the
        // port name → skipped (C compares it; Rust must not blanket-process).
        c.process_array(&with_str_attr(array(2), "FilePluginDestination", "x"));
        assert_eq!(
            c.writer.writes, 1,
            "1-char non-matching destination is skipped, not blanket-processed"
        );
    }

    #[test]
    fn test_g9_file_close_attribute_forces_close() {
        // G9: a FilePluginClose attribute forces an open stream to close.
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        c.file_base.set_mode(NDFileMode::Stream);
        c.file_base.set_num_capture(10);
        c.lazy_open = true;
        let mut updates = Vec::new();
        c.process_array(&array(1)); // cache an array
        c.start_capture(&mut updates).unwrap();
        let _ = &updates;
        c.process_array(&array(2)); // opens + writes
        assert!(c.file_base.is_open());

        c.process_array(&with_i32_attr(array(3), "FilePluginClose", 1));
        assert!(
            !c.file_base.is_open(),
            "FilePluginClose must close the file"
        );
        assert!(!c.capture_active, "close attribute stops capture");
    }

    #[test]
    fn test_b8_capture_owner_round_trip() {
        // B8: start_capture / stop_capture own the capture state together.
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        c.file_base.set_mode(NDFileMode::Capture);
        c.params.capture = Some(7);
        let mut updates = Vec::new();
        c.start_capture(&mut updates).unwrap();
        assert!(c.capture_active);
        c.stop_capture(&mut updates).unwrap();
        assert!(!c.capture_active);
        // CAPTURE PV updates emitted on each transition.
        assert!(!updates.is_empty());
    }

    #[test]
    fn test_b9_non_lazy_opens_eagerly_at_capture_start() {
        // B9: a non-lazy stream plugin opens the file at capture-start.
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        c.file_base.set_mode(NDFileMode::Stream);
        c.lazy_open = false;
        c.process_array(&array(1)); // cache a frame for the layout
        let mut updates = Vec::new();
        c.start_capture(&mut updates).unwrap();
        assert!(
            c.file_base.is_open(),
            "non-lazy stream opens at capture start"
        );
        assert_eq!(c.writer.opens, 1);
    }

    #[test]
    fn test_b9_lazy_defers_open_to_first_frame() {
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        c.file_base.set_mode(NDFileMode::Stream);
        c.file_base.set_num_capture(10);
        c.lazy_open = true;
        c.process_array(&array(1));
        let mut updates = Vec::new();
        c.start_capture(&mut updates).unwrap();
        assert!(
            !c.file_base.is_open(),
            "lazy stream does NOT open at capture start"
        );
        c.process_array(&array(2));
        assert!(c.file_base.is_open(), "lazy stream opens on first frame");
    }

    #[test]
    fn test_g12_capture_mode_validates_frames() {
        // G12: Capture mode rejects frames of mismatched dimensions.
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        c.file_base.set_mode(NDFileMode::Capture);
        c.file_base.set_num_capture(10);
        let mut updates = Vec::new();
        c.start_capture(&mut updates).unwrap();

        c.process_array(&array(1)); // first frame: 4-element, recorded
        assert_eq!(c.file_base.num_captured(), 1);

        // Mismatched frame: different dimension size → rejected.
        let mut big = NDArray::new(vec![NDDimension::new(8)], NDDataType::UInt8);
        big.unique_id = 2;
        c.process_array(&big);
        assert_eq!(c.file_base.num_captured(), 1, "mismatched frame rejected");

        // Matching frame: accepted.
        c.process_array(&array(3));
        assert_eq!(c.file_base.num_captured(), 2);
    }

    /// D6: `ReadFile` on a truncated or malformed file must still close the
    /// writer it opened, or every attempt leaks another open handle and leaves
    /// `current_path` set for the next Single-mode write to run against.
    #[test]
    fn read_file_closes_the_writer_when_the_read_fails() {
        let mut c = FilePluginController::new(MockWriter::new(false));
        c.set_port_name("F");
        c.params.read_file = Some(9);

        // MockWriter::read_file always fails, standing in for the malformed file.
        let snap = PluginParamSnapshot {
            enable_callbacks: true,
            reason: 9,
            addr: 0,
            value: ParamChangeValue::Int32(1),
        };
        c.on_param_change(9, &snap);

        assert_eq!(c.writer.opens, 1);
        assert_eq!(
            c.writer.closes, 1,
            "a failed read must still close the file it opened"
        );
    }

    /// D3: `stop_capture` owns the whole B8 transition, so a failing
    /// `close_stream` must not leave `capture_active` set — the error is
    /// reported, the state still moves.
    #[test]
    fn stop_capture_clears_capture_active_when_the_close_fails() {
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        c.params.capture = Some(7);
        c.file_base.set_mode(NDFileMode::Stream);
        c.file_base.set_num_capture(0);
        c.process_array(&array(1));

        let mut updates = Vec::new();
        c.start_capture(&mut updates).unwrap();
        assert!(c.capture_active);
        assert!(c.file_base.is_open());

        c.writer.fail_close = true;
        let mut updates = Vec::new();
        assert!(
            c.stop_capture(&mut updates).is_err(),
            "the close failure is still reported"
        );
        assert!(
            !c.capture_active,
            "capture state must not latch on a failed close"
        );
        assert!(
            updates.iter().any(|u| matches!(
                u,
                ParamUpdate::Int32 {
                    reason: 7,
                    value: 0,
                    ..
                }
            )),
            "CAPTURE=0 must still be posted"
        );
    }

    /// D3, the operator-visible path: `caput Capture 0` with a failing stream
    /// close must leave `Capture_RBV` at 0, not latched at 1.
    #[test]
    fn capture_off_param_write_reports_error_with_capture_rbv_cleared() {
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        c.params.capture = Some(7);
        c.file_base.set_mode(NDFileMode::Stream);
        c.file_base.set_num_capture(0);
        c.process_array(&array(1));

        let on = PluginParamSnapshot {
            enable_callbacks: true,
            reason: 7,
            addr: 0,
            value: ParamChangeValue::Int32(1),
        };
        c.on_param_change(7, &on);
        assert!(c.capture_active);

        c.writer.fail_close = true;
        let off = PluginParamSnapshot {
            enable_callbacks: true,
            reason: 7,
            addr: 0,
            value: ParamChangeValue::Int32(0),
        };
        let result = c.on_param_change(7, &off);
        assert!(!c.capture_active);
        assert!(
            result.param_updates.iter().any(|u| matches!(
                u,
                ParamUpdate::Int32 {
                    reason: 7,
                    value: 0,
                    ..
                }
            )),
            "Capture_RBV must read 0 after the failed close"
        );
    }

    #[test]
    fn test_b17_write_mode_switch_closes_open_stream() {
        // B17: switching WriteMode mid-stream closes the open file.
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        c.file_base.set_mode(NDFileMode::Stream);
        c.file_base.set_num_capture(10);
        c.params.write_mode = Some(5);
        c.lazy_open = false;
        c.process_array(&array(1));
        let mut updates = Vec::new();
        c.start_capture(&mut updates).unwrap();
        assert!(c.file_base.is_open());

        // Switch to Capture mode while a stream is open.
        let snap = PluginParamSnapshot {
            enable_callbacks: true,
            reason: 5,
            addr: 0,
            value: ParamChangeValue::Int32(NDFileMode::Capture as i32),
        };
        c.on_param_change(5, &snap);
        assert!(
            !c.file_base.is_open(),
            "mode switch must close the open stream"
        );
        assert!(!c.capture_active);
    }

    #[test]
    fn test_b7_capture_num_capture_zero_buffers_forever() {
        // B7: num_capture==0 buffers indefinitely (no auto-flush).
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        c.file_base.set_mode(NDFileMode::Capture);
        c.file_base.set_num_capture(0);
        c.auto_save = true;
        let mut updates = Vec::new();
        c.start_capture(&mut updates).unwrap();
        for id in 1..=5 {
            c.process_array(&array(id));
        }
        assert_eq!(
            c.file_base.num_captured(),
            5,
            "all frames buffered, no flush"
        );
        assert_eq!(c.writer.writes, 0, "num_capture==0 never auto-flushes");
        assert!(c.capture_active, "still capturing");
    }

    #[test]
    fn stream_mode_publishes_num_captured_per_frame() {
        // C++ NDPluginFile::processCallbacks sets NDFileNumCaptured after
        // every successful stream write. With NumCapture=0 (unbounded — the
        // ophyd-async writer convention) the target-reached branch never
        // runs, so a per-frame update is the only way NumCaptured_RBV moves.
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        c.params.num_captured = Some(42);
        c.file_base.set_mode(NDFileMode::Stream);
        c.file_base.set_num_capture(0);
        let mut updates = Vec::new();
        c.process_array(&array(1)); // latest_array for the eager open
        c.start_capture(&mut updates).unwrap();
        for (id, expected) in [(2, 1), (3, 2)] {
            let r = c.process_array(&array(id));
            let published = r.param_updates.iter().find_map(|u| match u {
                ParamUpdate::Int32 {
                    reason: 42, value, ..
                } => Some(*value),
                _ => None,
            });
            assert_eq!(
                published,
                Some(expected),
                "frame {id}: NUM_CAPTURED update missing or wrong"
            );
        }
    }

    #[test]
    fn test_g10_array_counter_counts_saved_frames() {
        // G10: ArrayCounter override reflects saved frames.
        let mut c = FilePluginController::new(MockWriter::new(false));
        c.set_port_name("F");
        c.params.array_counter = Some(99);
        c.file_base.set_mode(NDFileMode::Single);
        c.auto_save = true;
        let r1 = c.process_array(&array(1));
        let counter1 = r1.param_updates.iter().find_map(|u| match u {
            ParamUpdate::Int32 {
                reason: 99, value, ..
            } => Some(*value),
            _ => None,
        });
        assert_eq!(counter1, Some(1), "first saved frame → ArrayCounter 1");
        let r2 = c.process_array(&array(2));
        let counter2 = r2.param_updates.iter().find_map(|u| match u {
            ParamUpdate::Int32 {
                reason: 99, value, ..
            } => Some(*value),
            _ => None,
        });
        assert_eq!(counter2, Some(2));
    }
}
