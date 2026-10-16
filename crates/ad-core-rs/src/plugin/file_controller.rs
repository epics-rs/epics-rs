use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::{Mutex, MutexGuard};

use crate::error::{ADError, ADResult};
use crate::ndarray::{NDArray, NDDataType, NDDimension};

use super::file_base::{FileIo, NDFileMode, NDFileWriter, NDPluginFileBase, read_file_base};
use super::runtime::{ParamUpdate, ProcessResult, WriteOutcome, WriteVerdict};
use asyn_rs::trace::TraceMask;

/// Param indices for file plugin control (looked up once at registration time).
#[derive(Default, Clone, Copy)]
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
    /// C's `fileMutexId`: held for file I/O, which runs with the port lock
    /// released. Never locked from inside it.
    pub writer: Arc<Mutex<W>>,
    /// `supportsMultipleArrays`, a constructor constant in C.
    multiple_arrays: bool,
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
            multiple_arrays: writer.supports_multiple_arrays(),
            writer: Arc::new(Mutex::new(writer)),
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
        let Some((normalized, exists)) = check_file_path(&self.file_base.file_path) else {
            return;
        };
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

    /// C posts a nonzero `WriteFile`/`ReadFile` before its file I/O
    /// (NDPluginFile.cpp:808-811, :823-826).
    pub fn posts_before_write(&self, reason: usize, value: i32) -> bool {
        value != 0 && [self.params.write_file, self.params.read_file].contains(&Some(reason))
    }

    /// C `NDPluginFile::writeOctet` on the port thread: the setting is in
    /// the controller before any file I/O written after it.
    pub fn write_octet(
        &mut self,
        reason: usize,
        verdict: WriteVerdict<String>,
    ) -> WriteOutcome<String> {
        self.store_octet(reason, &verdict.kept());
        verdict.into()
    }

    /// [`FileControl::store_int32`] for the octet settings.
    pub fn store_octet(&mut self, reason: usize, value: &str) {
        let p = self.params;
        let field = if Some(reason) == p.file_path {
            // The port's write already ran `checkPath` and set FilePathExists.
            &mut self.file_base.file_path
        } else if Some(reason) == p.file_name {
            &mut self.file_base.file_name
        } else if Some(reason) == p.file_template {
            &mut self.file_base.file_template
        } else if Some(reason) == p.file_temp_suffix {
            &mut self.file_base.temp_suffix
        } else {
            return;
        };
        *field = value.to_owned();
    }

    /// `doCapture`'s checks and the readbacks C sets before it
    /// (NDPluginFile.cpp:831-856, :418-490).
    fn check_capture(&self, value: i32, verdict: WriteVerdict<i32>) -> WriteOutcome<i32> {
        let p = &self.params;
        let set_int = |out: WriteOutcome<i32>, idx: Option<usize>, v: i32| match idx {
            Some(i) => out.set_int32(i, 0, v),
            None => out,
        };
        let set_str = |out: WriteOutcome<i32>, idx: Option<usize>, v: &str| match idx {
            Some(i) => out.set_octet(i, 0, v),
            None => out,
        };
        let has_array = self.latest_array.is_some();
        let out = WriteOutcome::from(verdict.succeed());
        // C latches LazyOpen when a capture starts (:843).
        let lazy = if value != 0 {
            self.lazy_open
        } else {
            self.file_base.lazy_open
        };
        let mut out = out;
        if value != 0 {
            out = set_int(out, p.write_status, 0);
            out = set_str(out, p.write_message, "");
            out = set_str(out, p.full_file_name, "");
        }
        let failed = |out: WriteOutcome<i32>, line: &str| {
            WriteOutcome {
                verdict: WriteVerdict::Accept(0),
                ..out
            }
            .print(TraceMask::ERROR, line)
            .settled()
        };
        if !has_array && !lazy {
            if value == 0 {
                return out;
            }
            return failed(
                out,
                "NDPluginFile::doCapture: ERROR, must collect an array to get dimensions first",
            );
        }
        match self.file_base.mode() {
            NDFileMode::Single => {
                return failed(
                    set_str(
                        out,
                        p.write_message,
                        "ERROR: capture not supported in Single mode",
                    ),
                    "NDPluginFile:doCapture ERROR: capture not supported in Single mode",
                );
            }
            NDFileMode::Capture if value != 0 && !has_array => {
                return failed(
                    out,
                    "NDPluginFile::doCapture ERROR: No arrays collected: cannot allocate capture buffer",
                );
            }
            NDFileMode::Stream => out = set_int(out, p.write_file, (value != 0) as i32),
            NDFileMode::Capture => {}
        }
        if lazy {
            out = set_str(out, p.write_message, "Lazy Open...");
        }
        out
    }

    // ── helpers ──

    fn success_updates(&self) -> Vec<ParamUpdate> {
        let mut updates = Vec::new();
        self.push_file_number_update(&mut updates);
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

    fn push_file_number_update(&self, updates: &mut Vec<ParamUpdate>) {
        if let Some(idx) = self.params.file_number {
            updates.push(ParamUpdate::Int32 {
                reason: idx,
                addr: 0,
                value: self.file_base.file_number,
            });
        }
    }

    /// Re-publish every readback `NDFileBase` owns, read back out of it.
    ///
    /// A file operation can fail part-way through and leave that state moved.
    /// `flush_capture`'s single-image arm walks the capture buffer frame by
    /// frame (`file_base.rs:419-436`), dropping each landed frame through
    /// `take_landed` — which is what sets `num_captured` — and bumping
    /// `file_number` per frame, so a `?` on frame 5 of a 10-frame flush leaves
    /// `num_captured` at 5 and `file_number` five higher than the cycle began
    /// with, while the value the controller pushed before the flush still says
    /// 10. Re-reading is the only way the readbacks can name what is actually
    /// on disk, so every exit that follows a file operation does it rather than
    /// trusting what an earlier line in the same cycle pushed.
    fn push_file_base_readbacks(&self, updates: &mut Vec<ParamUpdate>) {
        self.push_num_captured_update(updates);
        self.push_file_number_update(updates);
        self.push_full_file_name_update(updates);
    }

    /// The failure exit of one `process_array` cycle: keep everything the cycle
    /// accumulated, add the error surface, and re-derive the `file_base`
    /// readbacks over the top of whatever the cycle published before it failed.
    fn fail_cycle(&mut self, proc_result: &mut ProcessResult, err: &ADError) {
        let ProcessResult {
            param_updates,
            trace,
            ..
        } = proc_result;
        self.push_error_updates(param_updates, trace, false, false, err);
        self.push_file_base_readbacks(&mut proc_result.param_updates);
    }

    fn push_full_file_name_update(&self, updates: &mut Vec<ParamUpdate>) {
        if let Some(idx) = self.params.full_file_name {
            updates.push(ParamUpdate::Octet {
                reason: idx,
                addr: 0,
                value: self.file_base.full_file_name().to_string(),
            });
        }
    }

    /// Append the error surface for a failed file operation onto the updates
    /// the caller has already accumulated, and the lines C prints for it onto
    /// `trace`. `NDFileWriteMessage` takes the error's text.
    ///
    /// Takes the accumulator rather than returning a fresh `Vec`, so an error
    /// exit **cannot** replace what the same cycle published before it failed:
    /// there is no return value to assign over one. Every site here used to
    /// build a fresh vector and then either assign it (`process_array`) or
    /// return it in place of the accumulator (`on_param_change`), which threw
    /// away the `NDFileNumCaptured`, `FilePathExists` and `Capture_RBV` updates
    /// that same cycle had already produced.
    ///
    /// C++ has no equivalent move. `NDPluginFile::processCallbacks` writes into
    /// the asyn parameter library as it goes and flushes once at its single
    /// exit, `callParamCallbacks()` (`NDPluginFile.cpp:789`), and
    /// `writeFileBase` sets `NDFileWriteStatus`/`NDFileWriteMessage` clean at
    /// `:231-232` then overwrites them with the error at `:259-260`. No line in
    /// it discards a parameter another line already set.
    fn push_error_updates(
        &self,
        updates: &mut Vec<ParamUpdate>,
        trace: &mut Vec<(TraceMask, String)>,
        read_reason: bool,
        write_reason: bool,
        err: &ADError,
    ) {
        trace.extend(err.lines().iter().map(|l| (TraceMask::ERROR, l.clone())));
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
                value: err.to_string(),
            });
        }
        // The capture state belongs on the error surface in its own right: a
        // `stop_capture` that failed must not report the error with
        // `Capture_RBV` still reading 1.
        self.push_capture_update(updates);
    }
}

/// The plugin's port lock as the file I/O sees it. C releases it around
/// every `openFile`/`writeFile`/`readFile`/`closeFile` (NDPluginFile.cpp:
/// 62-76, :247-251, :296-300), so a frame callback is never queued behind a
/// file write.
pub trait PortLock<C> {
    fn controller(&mut self) -> &mut C;
    fn unlocked<R>(&mut self, f: impl FnOnce() -> R) -> R;
}

impl<C> PortLock<C> for MutexGuard<'_, C> {
    fn controller(&mut self) -> &mut C {
        self
    }

    fn unlocked<R>(&mut self, f: impl FnOnce() -> R) -> R {
        MutexGuard::unlocked(self, f)
    }
}

/// A controller no lock guards, as in the unit tests.
#[cfg(test)]
impl<W: NDFileWriter> PortLock<Self> for FilePluginController<W> {
    fn controller(&mut self) -> &mut Self {
        self
    }

    fn unlocked<R>(&mut self, f: impl FnOnce() -> R) -> R {
        f()
    }
}

/// The file-base step of a port: `base` under the port lock, `io` with it
/// released and the writer's lock held.
struct PortIo<'a, P, W>(&'a mut P, PhantomData<fn() -> W>);

impl<'a, P, W> PortIo<'a, P, W> {
    fn new(port: &'a mut P) -> Self {
        Self(port, PhantomData)
    }
}

impl<W: NDFileWriter, P: PortLock<FilePluginController<W>>> FileIo for PortIo<'_, P, W> {
    fn base(&mut self) -> &mut NDPluginFileBase {
        &mut self.0.controller().file_base
    }

    fn io<R>(&mut self, f: impl FnOnce(&mut dyn NDFileWriter) -> R) -> R {
        let writer = Arc::clone(&self.0.controller().writer);
        self.0.unlocked(|| f(&mut *writer.lock()))
    }
}

/// The NDPluginFile entry points that reach the file, called on the locked
/// controller so the I/O can release the lock.
pub trait FileControl<W> {
    /// Process an incoming array: auto_save, capture buffering, stream write.
    ///
    /// The frame is kept by reference, as C's `pArray->reserve()` keeps it
    /// (NDPluginFile.cpp:744); nothing here copies the data.
    fn process_array(&mut self, array: &Arc<NDArray>) -> ProcessResult;

    /// C `NDPluginFile::writeInt32` (NDPluginFile.cpp:799-870) on the port
    /// thread: `validate_int32` then, unless that settled the write,
    /// `store_int32` with the value the param keeps.
    fn write_int32(
        &mut self,
        reason: usize,
        value: i32,
        verdict: WriteVerdict<i32>,
    ) -> WriteOutcome<i32>;

    /// The writes `NDPluginFile::writeInt32` acts on (NDPluginFile.cpp:
    /// 808-856), with the file I/O inside the write as C does it; the
    /// readbacks they leave are side params. C ends in `status =
    /// callParamCallbacks();` (:865), so every write succeeds; a WriteFile
    /// with no array or a `doCapture` that fails (:418-490) is settled with
    /// C's console line.
    fn validate_int32(
        &mut self,
        reason: usize,
        value: i32,
        verdict: WriteVerdict<i32>,
    ) -> WriteOutcome<i32>;

    /// Mirror a stored int32 setting into the controller. The settings live
    /// here, not in the param library, because `process_array` runs without
    /// it; they are written only from the write path, in write order. `out`
    /// is the write's outcome so far, and the value stored is what it keeps.
    fn store_int32(&mut self, reason: usize, out: WriteOutcome<i32>) -> WriteOutcome<i32>;
}

impl<W: NDFileWriter, P: PortLock<FilePluginController<W>>> FileControl<W> for P {
    fn process_array(&mut self, array: &Arc<NDArray>) -> ProcessResult {
        let mut proc_result = ProcessResult::empty();
        let array = Arc::clone(array);
        let c = self.controller();
        c.latest_array = Some(Arc::clone(&array));

        // G9: FilePluginDestination routing — skip frames not for this plugin.
        if !c.destination_matches(&array) {
            return proc_result;
        }

        // Re-check file-path existence before every write. C++ NDPluginFile
        // calls checkPath() before each write so a directory deleted after
        // FilePath was set is reflected; the Rust port previously updated
        // FilePathExists only on the FilePath param change.
        c.refresh_file_path_exists(&mut proc_result.param_updates);

        // G9: FilePluginClose attribute forces an immediate file close.
        let force_close = array
            .attributes
            .get("FilePluginClose")
            .and_then(|a| a.value.as_i64())
            .map(|v| v != 0)
            .unwrap_or(false);
        if force_close {
            if let Err(e) = NDPluginFileBase::force_close_io(&mut PortIo::new(self)) {
                self.controller().fail_cycle(&mut proc_result, &e);
                return proc_result;
            }
            let _ = stop_capture(self, &mut proc_result.param_updates);
            return proc_result;
        }

        // `Ok(true)` only where C calls writeFileBase, which alone sets
        // WriteStatus/WriteMessage (NDPluginFile.cpp:745-785).
        let c = self.controller();
        let result: ADResult<bool> = match c.file_base.mode() {
            NDFileMode::Single => {
                if c.auto_save {
                    let r = write_single(self, array);
                    if r.is_ok() {
                        self.controller().saved_frames += 1; // G10: count saved frame
                    }
                    r.map(|()| true)
                } else {
                    Ok(false)
                }
            }
            NDFileMode::Capture => {
                if c.capture_active {
                    // G12: validate frame dims/dtype in Capture mode too.
                    if !c.frame_valid(&array) {
                        return proc_result;
                    }
                    c.file_base.capture_array(array);
                    c.push_num_captured_update(&mut proc_result.param_updates);
                    let target = c.file_base.num_capture_target();
                    if target > 0 && c.file_base.num_captured() >= target {
                        if c.auto_save {
                            let to_save = c.file_base.num_captured() as i32;
                            if let Err(err) =
                                NDPluginFileBase::flush_capture_io(&mut PortIo::new(self))
                            {
                                Err(err)
                            } else {
                                let c = self.controller();
                                c.saved_frames += to_save; // G10
                                c.push_full_file_name_update(&mut proc_result.param_updates);
                                c.push_num_captured_update(&mut proc_result.param_updates);
                                stop_capture(self, &mut proc_result.param_updates).ok();
                                Ok(true)
                            }
                        } else {
                            stop_capture(self, &mut proc_result.param_updates).ok();
                            Ok(false)
                        }
                    } else {
                        Ok(false)
                    }
                } else {
                    Ok(false)
                }
            }
            NDFileMode::Stream => {
                if c.capture_active {
                    // G12: validate frame dims/dtype against the first frame.
                    if !c.frame_valid(&array) {
                        return proc_result;
                    }
                    // G9: attribute-driven filename override / mid-stream reopen.
                    let reopen =
                        c.apply_filename_attributes(&array, &mut proc_result.param_updates);
                    if reopen && c.file_base.is_open() {
                        if let Err(e) = NDPluginFileBase::force_close_io(&mut PortIo::new(self)) {
                            self.controller().fail_cycle(&mut proc_result, &e);
                            return proc_result;
                        }
                    }
                    let r = NDPluginFileBase::process_array_io(&mut PortIo::new(self), array);
                    let c = self.controller();
                    if r.is_ok() {
                        c.saved_frames += 1; // G10: count saved frame
                        // C++ NDPluginFile::processCallbacks publishes
                        // NDFileNumCaptured after every successful stream
                        // write — without this an unbounded stream
                        // (NumCapture=0) reports NumCaptured_RBV=0 forever.
                        c.push_num_captured_update(&mut proc_result.param_updates);
                    }
                    let target = c.file_base.num_capture_target();
                    if r.is_ok() && target > 0 && c.file_base.num_captured() >= target {
                        if let Err(e) = NDPluginFileBase::close_stream_io(&mut PortIo::new(self)) {
                            self.controller().fail_cycle(&mut proc_result, &e);
                            return proc_result;
                        }
                        stop_capture(self, &mut proc_result.param_updates).ok();
                        self.controller()
                            .push_full_file_name_update(&mut proc_result.param_updates);
                    }
                    r.map(|()| true)
                } else {
                    Ok(false)
                }
            }
        };

        let c = self.controller();
        if let Ok(wrote) = result {
            if wrote {
                proc_result.param_updates.extend(c.success_updates());
                c.push_full_file_name_update(&mut proc_result.param_updates);
            }
            // G10: override ArrayCounter (the runtime bumped it per callback)
            // with the saved-frame count so a file plugin's ArrayCounter_RBV
            // reflects frames actually written, not callbacks received.
            if let Some(idx) = c.params.array_counter {
                proc_result.param_updates.push(ParamUpdate::Int32 {
                    reason: idx,
                    addr: 0,
                    value: c.saved_frames,
                });
            }
        } else if let Err(err) = result {
            c.fail_cycle(&mut proc_result, &err);
        }
        proc_result
    }

    fn write_int32(
        &mut self,
        reason: usize,
        value: i32,
        verdict: WriteVerdict<i32>,
    ) -> WriteOutcome<i32> {
        let out = self.validate_int32(reason, value, verdict);
        if out.settled {
            return out;
        }
        self.store_int32(reason, out)
    }

    fn validate_int32(
        &mut self,
        reason: usize,
        value: i32,
        verdict: WriteVerdict<i32>,
    ) -> WriteOutcome<i32> {
        let c = self.controller();
        let p = c.params;
        let mut updates = Vec::new();
        if Some(reason) == p.write_file && value != 0 {
            let Some(array) = c.latest_array.clone() else {
                return WriteOutcome::from(WriteVerdict::Accept(0))
                    .print(
                        TraceMask::ERROR,
                        "NDPluginFile::writeInt32: ERROR, no valid array to write",
                    )
                    .settled();
            };
            let result = match c.file_base.mode() {
                NDFileMode::Single => write_single(self, array),
                NDFileMode::Capture => NDPluginFileBase::flush_capture_io(&mut PortIo::new(self)),
                NDFileMode::Stream => {
                    NDPluginFileBase::process_array_io(&mut PortIo::new(self), array)
                }
            };
            let c = self.controller();
            // C sets the flag back to 0, since this could be a busy record (:820).
            let mut out = WriteOutcome::from(WriteVerdict::Accept(0));
            match result {
                Ok(()) => {
                    updates.extend(c.success_updates());
                    c.push_num_captured_update(&mut updates);
                    c.push_full_file_name_update(&mut updates);
                }
                Err(err) => {
                    // The write that failed is `flush_capture` in Capture
                    // mode, which lands frames one at a time, so the
                    // readbacks have to come back off `file_base`.
                    c.push_error_updates(&mut updates, &mut out.trace, false, true, &err);
                    c.push_file_base_readbacks(&mut updates);
                }
            }
            return out.set_updates(updates);
        }
        if Some(reason) == p.read_file && value != 0 {
            let path = PathBuf::from(c.file_base.open_file_name());
            let layout = NDArray::new(vec![NDDimension::new(1)], NDDataType::UInt8);
            let result = PortIo::new(self).io(|w| read_file_base(w, &path, &layout).map(Arc::new));
            let c = self.controller();
            let mut out = WriteOutcome::from(WriteVerdict::Accept(0));
            return match result {
                Ok(array) => {
                    c.latest_array = Some(array.clone());
                    updates.extend(c.success_updates());
                    c.push_full_file_name_update(&mut updates);
                    out.set_updates(updates).publish(array)
                }
                Err(err) => {
                    // The name, and FileNumber past it, are set before the
                    // open that failed (:146-155).
                    c.push_error_updates(&mut updates, &mut out.trace, true, false, &err);
                    c.push_file_number_update(&mut updates);
                    c.push_full_file_name_update(&mut updates);
                    out.set_updates(updates)
                }
            };
        }
        // C frees on any write, 0 included (:855-856).
        if Some(reason) == p.free_capture {
            c.file_base.clear_capture();
            c.push_num_captured_update(&mut updates);
            return WriteOutcome::from(verdict.succeed()).set_updates(updates);
        }
        if Some(reason) != p.capture {
            return verdict.succeed().into();
        }
        let mut out = c.check_capture(value, verdict);
        if out.settled {
            return out;
        }
        // B8: capture start/stop routes through the single owner.
        let started = if value != 0 {
            start_capture(self, &mut updates)
        } else {
            stop_capture(self, &mut updates)
        };
        match started {
            Ok(()) => out.set_updates(updates),
            Err(e) => {
                self.controller().push_error_updates(
                    &mut updates,
                    &mut out.trace,
                    false,
                    false,
                    &e,
                );
                WriteOutcome {
                    verdict: WriteVerdict::Accept(0),
                    ..out
                }
                .set_updates(updates)
            }
        }
    }

    fn store_int32(&mut self, reason: usize, mut out: WriteOutcome<i32>) -> WriteOutcome<i32> {
        let value = out.verdict.kept();
        let c = self.controller();
        let p = c.params;
        let mut updates = Vec::new();
        if Some(reason) == p.file_number {
            c.file_base.file_number = value;
        } else if Some(reason) == p.auto_increment {
            c.file_base.auto_increment = value != 0;
        } else if Some(reason) == p.auto_save {
            c.auto_save = value != 0;
        } else if Some(reason) == p.write_mode {
            // B17: WriteMode transitions are gated through stop_capture so a
            // mode switch mid-capture cannot leave a stream file open.
            let new_mode = NDFileMode::from_i32(value);
            if c.capture_active && new_mode != c.file_base.mode() {
                if let Err(e) = stop_capture(self, &mut updates) {
                    self.controller().push_error_updates(
                        &mut updates,
                        &mut out.trace,
                        false,
                        false,
                        &e,
                    );
                    return out.set_updates(updates);
                }
            }
            self.controller().file_base.set_mode(new_mode);
        } else if Some(reason) == p.num_capture {
            // B7: numCapture==0 means "capture forever" — C++ buffers
            // indefinitely. Do not force a minimum of 1.
            c.file_base.set_num_capture(value.max(0) as usize);
        } else if Some(reason) == p.create_dir {
            c.file_base.create_dir = value;
        } else if Some(reason) == p.lazy_open {
            c.lazy_open = value != 0;
        } else if Some(reason) == p.delete_driver_file {
            c.delete_driver_file = value != 0;
        }
        out.set_updates(updates)
    }
}

/// Start capture/stream (single owner of the capture-state transition).
///
/// B8: clears the capture buffer, resets validation state, sets
/// `capture_active`, and emits the `CAPTURE`/`NUM_CAPTURED` PV updates.
/// B9: for a non-lazy Stream plugin whose writer supports multiple arrays
/// the file is opened eagerly here (C++ `doCapture`) so a bad path is
/// reported at capture-start; a lazy plugin defers to the first frame.
fn start_capture<W: NDFileWriter, P: PortLock<FilePluginController<W>>>(
    port: &mut P,
    updates: &mut Vec<ParamUpdate>,
) -> ADResult<()> {
    let c = port.controller();
    c.file_base.clear_capture();
    c.stream_dims = None;
    c.stream_data_type = None;
    c.file_base.lazy_open = c.lazy_open;
    c.file_base.delete_driver_file = c.delete_driver_file;
    // C clears NDFullFileName as a capture starts (NDPluginFile.cpp:847).
    c.file_base.clear_full_file_name();

    if c.file_base.mode() == NDFileMode::Stream && !c.lazy_open && c.multiple_arrays {
        // B9: eager open — needs a frame to know the layout.
        if let Some(array) = c.latest_array.clone() {
            let opened = NDPluginFileBase::open_stream_io(&mut PortIo::new(port), &array);
            let c = port.controller();
            c.push_file_number_update(updates);
            c.push_full_file_name_update(updates);
            opened?;
        }
    }
    let c = port.controller();
    c.capture_active = true;
    c.push_capture_update(updates);
    c.push_num_captured_update(updates);
    Ok(())
}

/// Stop capture/stream (single owner of the capture-state transition).
///
/// B8/B17: closes any open stream file, clears `capture_active`, and
/// emits the `CAPTURE` PV update. Idempotent.
///
/// The B8 invariant — that `capture_active`, the file-base capture state
/// and the `CAPTURE` PV move together — holds for the failing close as
/// well: the transition runs after the close whatever it returned, so a
/// `close_stream` error is reported without latching `Capture_RBV` at 1
/// against a plugin that is no longer capturing.
fn stop_capture<W: NDFileWriter, P: PortLock<FilePluginController<W>>>(
    port: &mut P,
    updates: &mut Vec<ParamUpdate>,
) -> ADResult<()> {
    let closed = if port.controller().file_base.mode() == NDFileMode::Stream {
        NDPluginFileBase::close_stream_io(&mut PortIo::new(port))
    } else {
        Ok(())
    };
    let c = port.controller();
    c.capture_active = false;
    c.stream_dims = None;
    c.stream_data_type = None;
    c.push_capture_update(updates);
    closed
}

fn write_single<W: NDFileWriter, P: PortLock<FilePluginController<W>>>(
    port: &mut P,
    array: Arc<NDArray>,
) -> ADResult<()> {
    port.controller().file_base.ensure_directory()?;
    NDPluginFileBase::process_array_io(&mut PortIo::new(port), array)
}

/// Normalize `path` and report whether the directory exists, through the one
/// implementation of C++ `asynNDArrayDriver::checkPath` that the file plugins
/// inherit (`NDPluginFile` -> `NDPluginDriver` -> `asynNDArrayDriver`).
/// `None` for an empty path, which C returns on before touching
/// NDFilePathExists (asynNDArrayDriver.cpp:105).
fn check_file_path(path: &str) -> Option<(String, bool)> {
    if path.is_empty() {
        return None;
    }
    let mut normalized = path.to_string();
    let exists = crate::driver::ndarray_driver::check_path_str(&mut normalized);
    Some((normalized, exists))
}

/// The port's side of a writer's settings: a write lands here at once, never
/// behind a file write that holds the writer (C stores the param; the writer
/// reads it at open, per frame and at close).
pub struct SettingsStore<T>(Arc<Mutex<(u64, T)>>);

impl<T> Clone for SettingsStore<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> SettingsStore<T> {
    pub fn update(&self, f: impl FnOnce(&mut T)) {
        let mut store = self.0.lock();
        f(&mut store.1);
        store.0 += 1;
    }

    pub fn get<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        f(&self.0.lock().1)
    }
}

/// The writer's copy of its settings, refreshed from the [`SettingsStore`]
/// only where the writer calls [`WriterSettings::refresh`].
pub struct WriterSettings<T> {
    store: SettingsStore<T>,
    cfg: T,
    seen: u64,
}

impl<T: Clone> WriterSettings<T> {
    pub fn new(cfg: T) -> Self {
        Self {
            store: SettingsStore(Arc::new(Mutex::new((0, cfg.clone())))),
            cfg,
            seen: 0,
        }
    }

    pub fn store(&self) -> SettingsStore<T> {
        self.store.clone()
    }

    pub fn refresh(&mut self) {
        let store = self.store.0.lock();
        if store.0 != self.seen {
            self.cfg = store.1.clone();
            self.seen = store.0;
        }
    }
}

impl<T> std::ops::Deref for WriterSettings<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
    use crate::error::ADError;
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
        /// Fail every `write_file` past this many successful ones, standing in
        /// for a volume that fills part way through a multi-frame flush.
        fail_write_after: Option<usize>,
        /// Make `read_file` return `array(77)`.
        readable: bool,
        /// Make `open_file` fail with a line of the writer's own.
        fail_open: bool,
    }
    impl MockWriter {
        fn new(multi: bool) -> Self {
            Self {
                opens: 0,
                writes: 0,
                closes: 0,
                multi,
                fail_close: false,
                fail_write_after: None,
                readable: false,
                fail_open: false,
            }
        }
    }
    impl NDFileWriter for MockWriter {
        fn open_file(&mut self, _p: &Path, _m: NDFileMode, _a: &NDArray) -> ADResult<()> {
            self.opens += 1;
            if self.fail_open {
                return Err(ADError::Reported {
                    lines: vec!["Mock::openFile cannot open".into()],
                    message: "cannot open".into(),
                });
            }
            Ok(())
        }
        fn write_file(&mut self, _a: &Arc<NDArray>) -> ADResult<()> {
            self.writes += 1;
            if let Some(n) = self.fail_write_after {
                if self.writes > n {
                    return Err(ADError::UnsupportedConversion("disk full".into()));
                }
            }
            Ok(())
        }
        fn read_file(&mut self) -> ADResult<NDArray> {
            if self.readable {
                return Ok(array(77));
            }
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
        c.process_array(&Arc::new(with_str_attr(
            array(1),
            "FilePluginDestination",
            "OTHER",
        )));
        assert_eq!(
            c.writer.lock().writes,
            0,
            "frame for OTHER port must be skipped"
        );

        // Destination = this port → written.
        c.process_array(&Arc::new(with_str_attr(
            array(2),
            "FilePluginDestination",
            "MYFILE",
        )));
        assert_eq!(c.writer.lock().writes, 1);

        // Destination = "all" → written.
        c.process_array(&Arc::new(with_str_attr(
            array(3),
            "FilePluginDestination",
            "all",
        )));
        assert_eq!(c.writer.lock().writes, 2);
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

        c.process_array(&Arc::new(with_i32_attr(
            array(1),
            "FilePluginDestination",
            42,
        )));
        assert_eq!(
            c.writer.lock().writes,
            1,
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
        c.process_array(&Arc::new(with_str_attr(
            array(1),
            "FilePluginDestination",
            "allfoo",
        )));
        assert_eq!(
            c.writer.lock().writes,
            1,
            "destination with \"all\" prefix is processed (C 3-char prefix match)"
        );

        // Non-empty 1-char destination that is neither an "all" prefix nor the
        // port name → skipped (C compares it; Rust must not blanket-process).
        c.process_array(&Arc::new(with_str_attr(
            array(2),
            "FilePluginDestination",
            "x",
        )));
        assert_eq!(
            c.writer.lock().writes,
            1,
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
        c.process_array(&Arc::new(array(1))); // cache an array
        start_capture(&mut c, &mut updates).unwrap();
        let _ = &updates;
        c.process_array(&Arc::new(array(2))); // opens + writes
        assert!(c.file_base.is_open());

        c.process_array(&Arc::new(with_i32_attr(array(3), "FilePluginClose", 1)));
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
        start_capture(&mut c, &mut updates).unwrap();
        assert!(c.capture_active);
        stop_capture(&mut c, &mut updates).unwrap();
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
        c.process_array(&Arc::new(array(1))); // cache a frame for the layout
        let mut updates = Vec::new();
        start_capture(&mut c, &mut updates).unwrap();
        assert!(
            c.file_base.is_open(),
            "non-lazy stream opens at capture start"
        );
        assert_eq!(c.writer.lock().opens, 1);
    }

    #[test]
    fn test_b9_lazy_defers_open_to_first_frame() {
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        c.file_base.set_mode(NDFileMode::Stream);
        c.file_base.set_num_capture(10);
        c.lazy_open = true;
        c.process_array(&Arc::new(array(1)));
        let mut updates = Vec::new();
        start_capture(&mut c, &mut updates).unwrap();
        assert!(
            !c.file_base.is_open(),
            "lazy stream does NOT open at capture start"
        );
        c.process_array(&Arc::new(array(2)));
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
        start_capture(&mut c, &mut updates).unwrap();

        c.process_array(&Arc::new(array(1))); // first frame: 4-element, recorded
        assert_eq!(c.file_base.num_captured(), 1);

        // Mismatched frame: different dimension size → rejected.
        let mut big = NDArray::new(vec![NDDimension::new(8)], NDDataType::UInt8);
        big.unique_id = 2;
        c.process_array(&Arc::new(big));
        assert_eq!(c.file_base.num_captured(), 1, "mismatched frame rejected");

        // Matching frame: accepted.
        c.process_array(&Arc::new(array(3)));
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
        c.write_int32(9, 1, WriteVerdict::Accept(1));

        assert_eq!(c.writer.lock().opens, 1);
        assert_eq!(
            c.writer.lock().closes,
            1,
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
        c.process_array(&Arc::new(array(1)));

        let mut updates = Vec::new();
        start_capture(&mut c, &mut updates).unwrap();
        assert!(c.capture_active);
        assert!(c.file_base.is_open());

        c.writer.lock().fail_close = true;
        let mut updates = Vec::new();
        assert!(
            stop_capture(&mut c, &mut updates).is_err(),
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
        c.process_array(&Arc::new(array(1)));

        c.write_int32(7, 1, WriteVerdict::Accept(1));
        assert!(c.capture_active);

        c.writer.lock().fail_close = true;
        let out = c.write_int32(7, 0, WriteVerdict::Accept(0));
        assert!(!c.capture_active);
        assert_eq!(
            side_int(&out, 7),
            Some(0),
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
        c.process_array(&Arc::new(array(1)));
        let mut updates = Vec::new();
        start_capture(&mut c, &mut updates).unwrap();
        assert!(c.file_base.is_open());

        // Switch to Capture mode while a stream is open.
        let mode = NDFileMode::Capture as i32;
        c.write_int32(5, mode, WriteVerdict::Accept(mode));
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
        start_capture(&mut c, &mut updates).unwrap();
        for id in 1..=5 {
            c.process_array(&Arc::new(array(id)));
        }
        assert_eq!(
            c.file_base.num_captured(),
            5,
            "all frames buffered, no flush"
        );
        assert_eq!(
            c.writer.lock().writes,
            0,
            "num_capture==0 never auto-flushes"
        );
        assert!(c.capture_active, "still capturing");
    }

    /// The controller keeps the frame it was handed, as C's `pArray->reserve()`
    /// does (NDPluginFile.cpp:744); it never copies the data.
    #[test]
    fn the_latest_array_is_the_input_arc_itself() {
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        let input = Arc::new(array(1));
        c.process_array(&input);
        assert!(Arc::ptr_eq(c.latest_array.as_ref().unwrap(), &input));
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
        c.process_array(&Arc::new(array(1))); // latest_array for the eager open
        start_capture(&mut c, &mut updates).unwrap();
        for (id, expected) in [(2, 1), (3, 2)] {
            let r = c.process_array(&Arc::new(array(id)));
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
        let r1 = c.process_array(&Arc::new(array(1)));
        let counter1 = r1.param_updates.iter().find_map(|u| match u {
            ParamUpdate::Int32 {
                reason: 99, value, ..
            } => Some(*value),
            _ => None,
        });
        assert_eq!(counter1, Some(1), "first saved frame → ArrayCounter 1");
        let r2 = c.process_array(&Arc::new(array(2)));
        let counter2 = r2.param_updates.iter().find_map(|u| match u {
            ParamUpdate::Int32 {
                reason: 99, value, ..
            } => Some(*value),
            _ => None,
        });
        assert_eq!(counter2, Some(2));
    }

    /// The value the parameter library would end up holding for `reason`:
    /// the LAST update in the cycle wins, exactly as repeated
    /// `setIntegerParam` calls do before a single `callParamCallbacks`.
    fn int_update(updates: &[ParamUpdate], reason: usize) -> Option<i32> {
        updates.iter().rev().find_map(|u| match u {
            ParamUpdate::Int32 {
                reason: r, value, ..
            } if *r == reason => Some(*value),
            _ => None,
        })
    }

    /// `int_update` for the side params a write stores.
    fn side_int<T>(out: &WriteOutcome<T>, reason: usize) -> Option<i32> {
        out.side.iter().rev().find_map(|s| match s {
            super::super::runtime::SideParam::Int32 {
                reason: r, value, ..
            } if *r == reason => Some(*value),
            _ => None,
        })
    }

    const NUM_CAPTURED: usize = 61;
    const WRITE_STATUS: usize = 62;
    const FILE_NUMBER: usize = 63;
    const PATH_EXISTS: usize = 64;
    const WRITE_FILE: usize = 65;

    fn capture_controller(multi: bool) -> FilePluginController<MockWriter> {
        let mut c = FilePluginController::new(MockWriter::new(multi));
        c.set_port_name("F");
        c.params.num_captured = Some(NUM_CAPTURED);
        c.params.write_status = Some(WRITE_STATUS);
        c.params.file_number = Some(FILE_NUMBER);
        c.params.file_path_exists = Some(PATH_EXISTS);
        c.params.write_file = Some(WRITE_FILE);
        c.file_base.set_mode(NDFileMode::Capture);
        // The single-image arm burns a file number per name it makes, which
        // is what makes NDFileNumber_RBV stale after a partial flush.
        c.file_base.auto_increment = true;
        c.auto_save = true;
        c.capture_active = true;
        c
    }

    /// The headline case. A single-image flush lands frames one at a time, so a
    /// writer that fills the volume on frame 3 of 4 leaves two frames queued
    /// and `file_number` three higher. `NDFileNumCaptured_RBV` has to name the
    /// two that are still owed — before this it kept the 4 the controller
    /// pushed BEFORE the flush, because the error exit assigned over it.
    #[test]
    fn a_partial_capture_flush_republishes_the_frames_still_queued() {
        let mut c = capture_controller(false);
        c.file_base.set_num_capture(4);
        c.writer.lock().fail_write_after = Some(2);
        let first_number = c.file_base.file_number;

        let mut last = ProcessResult::empty();
        for id in 1..=4 {
            last = c.process_array(&Arc::new(array(id)));
        }

        assert_eq!(
            c.file_base.num_captured(),
            2,
            "two frames never reached disk and stay queued"
        );
        assert_eq!(
            int_update(&last.param_updates, NUM_CAPTURED),
            Some(2),
            "the readback must name the frames still owed, not the 4 pushed \
             before the flush was attempted"
        );
        assert_eq!(
            int_update(&last.param_updates, FILE_NUMBER),
            Some(first_number + 3),
            "every name made burns a number, the failed frame's included \
             (asynNDArrayDriver.cpp:216-218)"
        );
        assert_eq!(
            int_update(&last.param_updates, WRITE_STATUS),
            Some(1),
            "the failure is still reported"
        );
    }

    /// The same failing cycle also ran `refresh_file_path_exists` before it
    /// touched the writer. That update belongs to the cycle and the error exit
    /// must not drop it — C++ accumulates into the parameter library and
    /// flushes once at `NDPluginFile.cpp:789`.
    #[test]
    fn a_partial_capture_flush_keeps_the_path_check_of_the_same_cycle() {
        let mut c = capture_controller(false);
        c.file_base.file_path = std::env::temp_dir().display().to_string();
        c.file_base.set_num_capture(4);
        c.writer.lock().fail_write_after = Some(2);

        let mut last = ProcessResult::empty();
        for id in 1..=4 {
            last = c.process_array(&Arc::new(array(id)));
        }

        assert!(
            int_update(&last.param_updates, PATH_EXISTS).is_some(),
            "FilePathExists was checked this cycle; the error exit dropped it"
        );
    }

    /// C `checkPath` returns before NDFilePathExists on an empty FilePath
    /// (asynNDArrayDriver.cpp:105), so a frame written with no path leaves
    /// FilePathExists_RBV undefined.
    #[test]
    fn an_empty_file_path_does_not_define_file_path_exists() {
        let mut c = capture_controller(false);
        c.params.file_path = Some(66);
        c.store_octet(66, "");
        let r = c.process_array(&Arc::new(array(1)));
        assert_eq!(int_update(&r.param_updates, PATH_EXISTS), None);
    }

    /// A formerly-bypassing exit: the Stream close ran its own
    /// `return ProcessResult::sink(..)` past the tail, so the
    /// `NDFileNumCaptured` this cycle had already published for the frame that
    /// DID reach disk went with it.
    #[test]
    fn a_failed_stream_close_keeps_the_count_the_same_cycle_published() {
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        c.params.num_captured = Some(NUM_CAPTURED);
        c.params.write_status = Some(WRITE_STATUS);
        c.file_base.set_mode(NDFileMode::Stream);
        c.file_base.set_num_capture(1);
        c.capture_active = true;
        c.writer.lock().fail_close = true;

        let r = c.process_array(&Arc::new(array(1)));

        assert_eq!(
            int_update(&r.param_updates, NUM_CAPTURED),
            Some(1),
            "the frame reached disk before the close failed"
        );
        assert_eq!(
            int_update(&r.param_updates, WRITE_STATUS),
            Some(1),
            "the close failure is still reported"
        );
    }

    /// The control-plane twin of the headline case: a manual `WriteFile` on a
    /// buffered capture flushes through the same partial path, and its error
    /// exit published no count at all.
    #[test]
    fn a_failed_manual_write_republishes_the_capture_count() {
        let mut c = capture_controller(false);
        c.file_base.set_num_capture(0); // buffer forever, never auto-flush
        for id in 1..=4 {
            c.process_array(&Arc::new(array(id)));
        }
        assert_eq!(c.file_base.num_captured(), 4, "all four are buffered");

        c.writer.lock().fail_write_after = Some(2);
        let out = c.write_int32(WRITE_FILE, 1, WriteVerdict::Accept(1));

        assert_eq!(
            side_int(&out, NUM_CAPTURED),
            Some(2),
            "a failed manual flush must still say how many frames are owed"
        );
        assert_eq!(side_int(&out, WRITE_STATUS), Some(1));
    }

    /// C sets WriteStatus/WriteMessage only inside writeFileBase
    /// (NDPluginFile.cpp:745-785), so a frame that writes nothing — Single
    /// with AutoSave=0, Capture short of its target, Stream while not
    /// capturing — leaves a
    /// failed WriteFile's status standing.
    #[test]
    fn a_frame_that_writes_nothing_keeps_a_failed_write_status() {
        for mode in [NDFileMode::Single, NDFileMode::Capture, NDFileMode::Stream] {
            let mut c = capture_controller(false);
            c.file_base.set_mode(mode);
            c.auto_save = false;
            // Capture buffers without a target, so the flush has a frame to
            // fail on and no frame reaches a write.
            c.capture_active = mode == NDFileMode::Capture;
            c.file_base.set_num_capture(0);
            c.process_array(&Arc::new(array(1)));
            c.writer.lock().fail_write_after = Some(0);
            let out = c.write_int32(WRITE_FILE, 1, WriteVerdict::Accept(1));
            assert_eq!(
                side_int(&out, WRITE_STATUS),
                Some(1),
                "{mode:?} write fails"
            );

            let r = c.process_array(&Arc::new(array(2)));
            assert_eq!(
                int_update(&r.param_updates, WRITE_STATUS),
                None,
                "{mode:?}: a frame that wrote nothing must not reset WriteStatus"
            );
        }
    }

    /// The success boundary: nothing above may turn a clean flush into an
    /// error surface or leave a stale count behind.
    #[test]
    fn a_clean_capture_flush_reports_no_frames_left_and_no_error() {
        let mut c = capture_controller(false);
        c.file_base.set_num_capture(4);

        let mut last = ProcessResult::empty();
        for id in 1..=4 {
            last = c.process_array(&Arc::new(array(id)));
        }

        assert_eq!(c.file_base.num_captured(), 0);
        assert_eq!(int_update(&last.param_updates, NUM_CAPTURED), Some(0));
        assert_eq!(int_update(&last.param_updates, WRITE_STATUS), Some(0));
    }

    /// A controller wired to a port with the NDFile params, at the given
    /// LazyOpen and FileWriteMode.
    fn write_port(lazy: i32, mode: i32) -> FilePluginController<MockWriter> {
        let mut base = asyn_rs::port::PortDriverBase::new("F", 1, Default::default());
        crate::params::ndarray_driver::NDArrayDriverParams::create(&mut base).unwrap();
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.register_params(&mut base).unwrap();
        c.store_int32(
            c.params.lazy_open.unwrap(),
            WriteVerdict::Accept(lazy).into(),
        );
        c.store_int32(
            c.params.write_mode.unwrap(),
            WriteVerdict::Accept(mode).into(),
        );
        c
    }

    fn capture_on(c: &mut FilePluginController<MockWriter>) -> WriteOutcome<i32> {
        c.write_int32(c.params.capture.unwrap(), 1, WriteVerdict::Accept(1))
    }

    fn message(c: &FilePluginController<MockWriter>, out: &WriteOutcome<i32>) -> Option<String> {
        out.side.iter().rev().find_map(|s| match s {
            super::super::runtime::SideParam::Octet { reason, value, .. }
                if Some(*reason) == c.params.write_message =>
            {
                Some(value.clone())
            }
            _ => None,
        })
    }

    /// `"%s::%s: ERROR, must collect an array to get dimensions first\n"`
    /// (NDPluginFile.cpp:435): Capture and Stream alike, Capture left 0.
    #[test]
    fn capture_without_an_array_fails_unless_lazy_as_c_do_capture_does() {
        for mode in [1, 2] {
            let mut c = write_port(0, mode);
            let out = capture_on(&mut c);
            assert_eq!(out.verdict, WriteVerdict::Accept(0));
            assert!(out.settled);
            assert_eq!(
                out.trace,
                vec![(
                    TraceMask::ERROR,
                    "NDPluginFile::doCapture: ERROR, must collect an array to get dimensions first"
                        .to_string()
                )]
            );
            assert_eq!(message(&c, &out).as_deref(), Some(""));
        }
    }

    /// `"%s::%s ERROR: No arrays collected: cannot allocate capture buffer\n"`
    /// (NDPluginFile.cpp:465): a lazy Capture still needs an array.
    #[test]
    fn a_lazy_capture_without_an_array_fails_as_c_do_capture_does() {
        let mut c = write_port(1, 1);
        let out = capture_on(&mut c);
        assert_eq!(out.verdict, WriteVerdict::Accept(0));
        assert!(out.settled);
        assert_eq!(
            out.trace,
            vec![(
                TraceMask::ERROR,
                "NDPluginFile::doCapture ERROR: No arrays collected: cannot allocate capture buffer"
                    .to_string()
            )]
        );
    }

    /// `if (this->lazyOpen) setStringParam(NDFileWriteMessage, "Lazy Open...");`
    /// (NDPluginFile.cpp:852): a lazy Stream starts with no array.
    #[test]
    fn a_lazy_stream_without_an_array_starts_as_c_write_int32_does() {
        let mut c = write_port(1, 2);
        let out = capture_on(&mut c);
        assert_eq!(out.verdict, WriteVerdict::Accept(1));
        assert!(!out.settled);
        assert!(out.trace.is_empty());
        assert_eq!(message(&c, &out).as_deref(), Some("Lazy Open..."));
    }

    /// `"%s::%s: ERROR, no valid array to write"` (NDPluginFile.cpp:816):
    /// WriteFile drops back to 0, WriteMessage is untouched, the write
    /// succeeds.
    #[test]
    fn write_file_without_an_array_prints_as_c_write_int32_does() {
        let mut c = write_port(0, 0);
        let write_file = c.params.write_file.unwrap();
        let out = c.write_int32(write_file, 1, WriteVerdict::Accept(1));
        assert_eq!(out.verdict, WriteVerdict::Accept(0));
        assert!(out.settled);
        assert!(out.side.is_empty());
        assert_eq!(
            out.trace,
            vec![(
                TraceMask::ERROR,
                "NDPluginFile::writeInt32: ERROR, no valid array to write".to_string()
            )]
        );
    }

    /// `status = callParamCallbacks();` (NDPluginFile.cpp:865) drops a base
    /// class failure.
    #[test]
    fn a_refused_base_write_succeeds_as_c_write_int32_does() {
        let mut c = write_port(0, 0);
        let clamp = WriteVerdict::Refuse {
            keep: 1,
            message: String::new(),
        };
        let out = c.write_int32(usize::MAX, 98, clamp);
        assert_eq!(out.verdict, WriteVerdict::Accept(1));
        assert!(!out.settled);
    }

    /// NDPluginFile.cpp:808-822: WriteFile writes inside the write, leaves
    /// the readbacks for the same callParamCallbacks, and drops back to 0.
    #[test]
    fn write_file_writes_inside_the_write_as_c_write_int32_does() {
        let mut c = write_port(0, 0);
        let p = c.params;
        let dir = std::env::temp_dir().to_string_lossy().into_owned() + "/";
        c.store_octet(p.file_path.unwrap(), &dir);
        c.store_octet(p.file_template.unwrap(), "%s%s_%3.3d.x");
        c.store_int32(p.auto_increment.unwrap(), WriteVerdict::Accept(1).into());
        c.store_int32(p.file_number.unwrap(), WriteVerdict::Accept(7).into());
        c.latest_array = Some(Arc::new(array(1)));
        let out = c.write_int32(p.write_file.unwrap(), 1, WriteVerdict::Accept(1));
        assert_eq!(out.verdict, WriteVerdict::Accept(0));
        assert_eq!(c.writer.lock().writes, 1);
        assert_eq!(side_int(&out, p.write_status.unwrap()), Some(0));
        assert_eq!(side_int(&out, p.file_number.unwrap()), Some(8));
    }

    /// A writer whose `write_file` holds until the test lets it go.
    struct GateWriter(Arc<std::sync::Barrier>);

    impl NDFileWriter for GateWriter {
        fn open_file(&mut self, _p: &Path, _m: NDFileMode, _a: &NDArray) -> ADResult<()> {
            Ok(())
        }
        fn write_file(&mut self, _a: &Arc<NDArray>) -> ADResult<()> {
            self.0.wait(); // entered
            self.0.wait(); // released
            Ok(())
        }
        fn read_file(&mut self) -> ADResult<NDArray> {
            Ok(array(0))
        }
        fn close_file(&mut self) -> ADResult<()> {
            Ok(())
        }
    }

    /// C unlocks the port around `writeFile` (NDPluginFile.cpp:247-251), so
    /// a frame callback during a busy WriteFile runs instead of queueing
    /// until the file is written.
    #[test]
    fn a_frame_during_a_busy_write_file_is_not_blocked() {
        let gate = Arc::new(std::sync::Barrier::new(2));
        let mut base = asyn_rs::port::PortDriverBase::new("F", 1, Default::default());
        crate::params::ndarray_driver::NDArrayDriverParams::create(&mut base).unwrap();
        let mut c = FilePluginController::new(GateWriter(Arc::clone(&gate)));
        c.register_params(&mut base).unwrap();
        let p = c.params;
        c.store_octet(
            p.file_path.unwrap(),
            &(std::env::temp_dir().to_string_lossy() + "/"),
        );
        c.store_octet(p.file_template.unwrap(), "%s%s_%d.x");
        c.latest_array = Some(Arc::new(array(1)));
        let ctrl = Arc::new(Mutex::new(c));

        let writing = {
            let ctrl = Arc::clone(&ctrl);
            std::thread::spawn(move || {
                ctrl.lock()
                    .write_int32(p.write_file.unwrap(), 1, WriteVerdict::Accept(1))
            })
        };
        gate.wait(); // the write is inside write_file
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let frame = {
            let ctrl = Arc::clone(&ctrl);
            std::thread::spawn(move || {
                ctrl.lock().process_array(&Arc::new(array(2)));
                done_tx.send(()).unwrap();
            })
        };
        let returned = done_rx.recv_timeout(std::time::Duration::from_secs(5));
        gate.wait(); // release the write
        assert!(returned.is_ok(), "process_array waited for write_file");
        frame.join().unwrap();
        let out = writing.join().unwrap();
        assert_eq!(side_int(&out, p.write_status.unwrap()), Some(0));
        assert_eq!(ctrl.lock().latest_array.as_ref().unwrap().unique_id, 2);
    }

    /// NDPluginFile.cpp:189-193: the array ReadFile reads goes downstream
    /// from the write, and ReadFile drops back to 0 (:829).
    #[test]
    fn read_file_publishes_from_the_write_as_c_read_file_base_does() {
        let mut c = write_port(0, 0);
        c.writer.lock().readable = true;
        let read_file = c.params.read_file.unwrap();
        let out = c.write_int32(read_file, 1, WriteVerdict::Accept(1));
        assert_eq!(out.verdict, WriteVerdict::Accept(0));
        assert_eq!(out.publish.0.len(), 1);
        assert_eq!(out.publish.0[0].unique_id, 77);
        assert!(Arc::ptr_eq(
            &out.publish.0[0],
            c.latest_array.as_ref().unwrap()
        ));
        assert_eq!(side_int(&out, c.params.write_status.unwrap()), Some(0));
    }

    /// asynNDArrayDriver.cpp:216-218 via readFileBase (:146): a ReadFile
    /// makes the name, so AutoIncrement moves FileNumber, read or not.
    #[test]
    fn read_file_moves_file_number_as_c_create_file_name_does() {
        for readable in [true, false] {
            let mut c = write_port(0, 0);
            let p = c.params;
            c.writer.lock().readable = readable;
            c.store_octet(p.file_template.unwrap(), "%s%s_%d.x");
            c.store_int32(p.auto_increment.unwrap(), WriteVerdict::Accept(1).into());
            c.store_int32(p.file_number.unwrap(), WriteVerdict::Accept(5).into());
            let out = c.write_int32(p.read_file.unwrap(), 1, WriteVerdict::Accept(1));
            assert_eq!(
                side_int(&out, p.file_number.unwrap()),
                Some(6),
                "{readable}"
            );
            assert_eq!(c.file_base.full_file_name(), "_5.x");
        }
    }

    /// A setting written before a Capture is what the capture runs with:
    /// both are applied on the write path, in write order.
    #[test]
    fn a_capture_runs_with_the_settings_written_before_it() {
        let mut c = write_port(0, 0);
        let p = c.params;
        c.latest_array = Some(Arc::new(array(1)));
        c.write_int32(p.write_mode.unwrap(), 2, WriteVerdict::Accept(2));
        let out = capture_on(&mut c);
        assert_eq!(out.verdict, WriteVerdict::Accept(1));
        assert!(c.capture_active);
        assert!(c.file_base.is_open(), "a non-lazy stream opens at capture");
    }

    /// NDPluginFile.cpp:855-856: FreeCapture frees the buffer whatever the
    /// written value.
    #[test]
    fn free_capture_frees_on_any_write_as_c_write_int32_does() {
        let mut c = capture_controller(false);
        c.params.free_capture = Some(66);
        c.file_base.set_num_capture(0);
        c.process_array(&Arc::new(array(1)));
        assert_eq!(c.file_base.num_captured(), 1);
        let out = c.write_int32(66, 0, WriteVerdict::Accept(0));
        assert_eq!(c.file_base.num_captured(), 0);
        assert_eq!(side_int(&out, NUM_CAPTURED), Some(0));
    }

    /// A Single-mode port named `/d/f_<n>` with a frame to write and the
    /// WriteMessage/WriteStatus readbacks wired.
    fn base_port() -> FilePluginController<MockWriter> {
        let mut c = write_port(0, 0);
        c.file_base.file_path = "/d/".into();
        c.file_base.file_name = "f".into();
        c.file_base.file_template = "%s%s_%d".into();
        c.latest_array = Some(Arc::new(array(1)));
        c
    }

    fn write_file(c: &mut FilePluginController<MockWriter>) -> WriteOutcome<i32> {
        let reason = c.params.write_file.unwrap();
        c.write_int32(reason, 1, WriteVerdict::Accept(1))
    }

    fn error_lines(lines: &[&str]) -> Vec<(TraceMask, String)> {
        lines
            .iter()
            .map(|l| (TraceMask::ERROR, l.to_string()))
            .collect()
    }

    /// NDPluginFile.cpp:66-73: the writer's own lines, then C's, and C's
    /// text in WriteMessage.
    #[test]
    fn a_failed_open_prints_open_file_base_after_the_writer() {
        let mut c = base_port();
        c.writer.lock().fail_open = true;
        let out = write_file(&mut c);
        assert_eq!(
            out.trace,
            error_lines(&[
                "Mock::openFile cannot open",
                "NDPluginFile::openFileBase Error opening file /d/f_0, status=3",
            ])
        );
        assert_eq!(
            message(&c, &out).as_deref(),
            Some("Error opening file /d/f_0, status=3")
        );
        assert_eq!(side_int(&out, c.params.write_status.unwrap()), Some(1));
    }

    /// NDPluginFile.cpp:254-260, from a frame (AutoSave) as from WriteFile.
    #[test]
    fn a_failed_frame_write_prints_write_file_base() {
        let mut c = base_port();
        c.auto_save = true;
        c.writer.lock().fail_write_after = Some(0);
        let result = c.process_array(&Arc::new(array(2)));
        assert_eq!(
            result.trace,
            error_lines(&["NDPluginFile::writeFileBase Error writing file, status=3"])
        );
        let message = result.param_updates.iter().rev().find_map(|u| match u {
            ParamUpdate::Octet { reason, value, .. } if Some(*reason) == c.params.write_message => {
                Some(value.clone())
            }
            _ => None,
        });
        assert_eq!(message.as_deref(), Some("Error writing file, status=3"));
    }

    /// NDPluginFile.cpp:104-127: a failed close, and a temp file that cannot
    /// be renamed — the mock never creates it.
    #[test]
    fn a_failed_close_or_rename_prints_close_file_base() {
        let mut c = base_port();
        c.writer.lock().fail_close = true;
        let out = write_file(&mut c);
        assert_eq!(
            out.trace,
            error_lines(&["NDPluginFile::closeFileBase Error closing file, status=3"])
        );
        assert_eq!(
            message(&c, &out).as_deref(),
            Some("Error closing file, status=3")
        );

        let mut c = base_port();
        c.file_base.temp_suffix = ".tmp".into();
        let out = write_file(&mut c);
        let text = "Error renaming temporary file /d/f_0.tmp to /d/f_0";
        assert_eq!(
            out.trace,
            error_lines(&[&format!("NDPluginFile::closeFileBase {text}")])
        );
        assert_eq!(message(&c, &out).as_deref(), Some(text));
    }

    /// NDPluginFile.cpp:160-180, the double space included.
    #[test]
    fn a_failed_read_prints_read_file_base() {
        let mut c = base_port();
        let reason = c.params.read_file.unwrap();
        let out = c.write_int32(reason, 1, WriteVerdict::Accept(1));
        assert_eq!(
            out.trace,
            error_lines(&["NDPluginFile::readFileBase Error  file /d/f_0, status=3"])
        );
        assert_eq!(
            message(&c, &out).as_deref(),
            Some("Error  file /d/f_0, status=3")
        );

        let mut c = base_port();
        c.writer.lock().fail_open = true;
        let out = c.write_int32(reason, 1, WriteVerdict::Accept(1));
        assert_eq!(
            out.trace,
            error_lines(&[
                "Mock::openFile cannot open",
                "NDPluginFile::readFileBase Error opening file /d/f_0, status=3",
            ])
        );
    }
}
