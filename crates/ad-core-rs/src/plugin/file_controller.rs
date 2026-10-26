use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::{Mutex, MutexGuard};

use crate::attributes::NDAttributeList;
use crate::error::{ADError, ADResult};
use crate::ndarray::{NDArray, NDDataType, NDDimension};
use crate::ndarray_pool::DriverPool;

use super::file_base::{
    Deleted, FileIo, FlushStep, FrameStatus, NDFileMode, NDFileWriter, NDPluginFileBase,
    read_file_base,
};
use super::runtime::{
    AttributeView, ParamPost, ParamUpdate, PluginPool, ProcessResult, WriteOutcome, WriteVerdict,
};
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
    pub array_counter: Option<usize>,
    pub execution_time: Option<usize>,
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
/// and only through `start_capture()` / `stop_capture()` / `abort_capture()`.
/// No other method may
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
    /// `stop_capture` / `abort_capture` may write this (B8).
    pub capture_active: bool,
    pub lazy_open: bool,
    pub latest_array: Option<Arc<NDArray>>,
    /// This plugin's asyn port name, for FilePluginDestination matching (G9).
    port_name: String,
    /// The `getAttributes` view of the callback or write running now, from
    /// its [`ParamPost`]; the writer and each hand-on read it.
    view: Option<AttributeView>,
    /// C `pNDArrayPool`, which `readFile` allocates from.
    pool: PluginPool,
}

impl<W: NDFileWriter> FilePluginController<W> {
    /// Record `sets`, made but not posted yet, into the view.
    fn record(&self, sets: &[ParamUpdate]) {
        if let Some(view) = &self.view {
            view.record(sets.iter().filter_map(ParamUpdate::stored));
        }
    }

    /// The sets C has made by the time the writer runs that the store does
    /// not have yet: `createFileName`'s FullFileName, FileNumber and
    /// FilePathExists.
    fn file_sets(&self) -> Vec<ParamUpdate> {
        let mut updates = Vec::new();
        self.push_create_file_name_updates(&mut updates);
        self.push_full_file_name_update(&mut updates);
        updates
    }

    pub fn new(writer: W) -> Self {
        Self {
            file_base: NDPluginFileBase::new(),
            multiple_arrays: writer.supports_multiple_arrays(),
            writer: Arc::new(Mutex::new(writer)),
            params: FileParamIndices::default(),
            auto_save: false,
            capture_active: false,
            lazy_open: false,
            latest_array: None,
            port_name: String::new(),
            view: None,
            pool: PluginPool::new(DriverPool::standalone(0)),
        }
    }

    /// Take the plugin's pool, as its
    /// [`NDPluginProcess::bind_pool`](super::runtime::NDPluginProcess::bind_pool)
    /// gets it; until then, a standalone one.
    pub fn bind_pool(&mut self, pool: PluginPool) {
        self.pool = pool;
    }

    /// Set this plugin's asyn port name (used for FilePluginDestination
    /// routing, G9); [`Self::register_params`] sets it from the port.
    pub fn set_port_name(&mut self, name: impl Into<String>) {
        self.port_name = name.into();
    }

    /// Look up all standard file param indices from the port driver base.
    pub fn register_params(
        &mut self,
        base: &mut asyn_rs::port::PortDriverBase,
    ) -> asyn_rs::error::AsynResult<()> {
        self.port_name = base.port_name.to_string();
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
        self.params.execution_time = base.find_param("EXECUTION_TIME");
        Ok(())
    }

    // ── capture state owner (B8) ──

    /// C `attrIsProcessingRequired` (NDPluginFile.cpp:629-651), the file
    /// plugins'
    /// [`crate::plugin::runtime::NDPluginProcess::processing_required`]: a string
    /// `FilePluginDestination` other than "all" or this plugin's port name
    /// means the frame is not for this plugin.
    pub fn processing_required(&self, array: &NDArray) -> bool {
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
                // C tests "all" with `epicsStrnCaseCmp(dest,"all",min(len,3))`,
                // `len` counting the NUL, so "a" and "al" are compared with
                // their terminator and fail; the port name with a
                // full-length compare.
                let matches_all =
                    dest.len() >= 3 && dest.as_bytes()[..3].eq_ignore_ascii_case(b"all");
                matches_all || dest.eq_ignore_ascii_case(&self.port_name)
            }
            None => true,
        }
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
        if Some(reason) == p.file_path {
            self.file_base.set_file_path(value);
            return;
        }
        let field = if Some(reason) == p.file_name {
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
        let out = WriteOutcome::from(verdict);
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
                verdict: zeroed(false),
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
                "NDPluginFile::doCapture: ERROR, must collect an array to get dimensions first\n",
            );
        }
        match self.file_base.mode() {
            // C refuses a stop in Single mode as well, against its own
            // comment that only capture=1 is the error (NDPluginFile.cpp:452);
            // not copied, a stop is a no-op as with no array (:431).
            NDFileMode::Single if value != 0 => {
                return failed(
                    set_str(
                        out,
                        p.write_message,
                        "ERROR: capture not supported in Single mode",
                    ),
                    "NDPluginFile:doCapture ERROR: capture not supported in Single mode\n",
                );
            }
            NDFileMode::Capture if value != 0 && !has_array => {
                return failed(
                    out,
                    "NDPluginFile::doCapture ERROR: No arrays collected: cannot allocate capture buffer\n",
                );
            }
            NDFileMode::Stream => out = set_int(out, p.write_file, (value != 0) as i32),
            NDFileMode::Single | NDFileMode::Capture => {}
        }
        if lazy {
            out = set_str(out, p.write_message, "Lazy Open...");
        }
        out
    }

    // ── helpers ──

    fn success_updates(&self) -> Vec<ParamUpdate> {
        let mut updates = Vec::new();
        self.push_create_file_name_updates(&mut updates);
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

    /// What C `writeFileBase` sets and posts before a Single or Capture
    /// write (NDPluginFile.cpp:231-232, :240-241, :278-279); both end by
    /// setting WriteFile back to 0 (:265, :329). Stream mode leaves
    /// WriteFile to `doCapture`.
    fn write_file_start(&self) -> Vec<ParamUpdate> {
        let mut sets = self.write_ok();
        sets.extend(self.params.write_file.map(|r| ParamUpdate::int32(r, 1)));
        sets
    }

    /// WriteStatus OK and an empty WriteMessage, which `writeFileBase` and
    /// `openFileBase` set first (NDPluginFile.cpp:41-42, :231-232). A frame
    /// is handed on only after its own open, or in a multi-array file while
    /// no frame has failed, so every hand-on reads them.
    fn write_ok(&self) -> Vec<ParamUpdate> {
        let p = &self.params;
        [
            p.write_status.map(|r| ParamUpdate::int32(r, 0)),
            p.write_message
                .map(|r| ParamUpdate::octet(r, String::new())),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    fn push_write_file_end(&self, updates: &mut Vec<ParamUpdate>) {
        if let Some(reason) = self.params.write_file {
            updates.push(ParamUpdate::int32(reason, 0));
        }
    }

    /// ArrayCounter as a capture flush starts, which it counts up per
    /// frame "so the user can see the array rate" and is set back to once
    /// the frames are written (NDPluginFile.cpp:283-287, :316-323).
    fn flush_array_counter(&self, post: &dyn ParamPost) -> Option<(usize, i32)> {
        let reason = self.params.array_counter?;
        Some((reason, post.int32(reason)?))
    }

    /// The posts C makes inside a capture flush, per [`FlushStep`], each
    /// after the frame it follows is handed on (NDPluginFile.cpp:301-321);
    /// a frame's failure lines go onto `trace`. `pending`, what the callback
    /// has set, goes out with WriteFile 1 in C's one `callParamCallbacks`
    /// (NDPluginFile.cpp:278-279), and stays unposted when the flush never
    /// starts.
    fn flush_post<'a>(
        &self,
        post: &'a mut dyn ParamPost,
        trace: &'a mut Vec<(TraceMask, String)>,
        pending: &'a mut Vec<ParamUpdate>,
    ) -> impl FnMut(FlushStep<'_>) + 'a {
        let mut start = Some(self.write_file_start());
        let mut array_counter = self.flush_array_counter(post);
        let p = self.params;
        let (view, write_ok) = (self.view.clone(), self.write_ok());
        move |step| match step {
            FlushStep::Start => {
                let mut sets = std::mem::take(pending);
                sets.extend(start.take().unwrap_or_default());
                post.post(sets);
            }
            FlushStep::Frame {
                num_captured,
                elapsed,
                status,
                handed_on: frame,
                file_number,
                full_file_name,
                file_path_exists,
            } => {
                if let Some(frame) = frame {
                    if let Some(view) = &view {
                        view.record(write_ok.iter().filter_map(ParamUpdate::stored));
                    }
                    post.hand_on(frame);
                }
                if let Some((_, count)) = &mut array_counter {
                    *count = count.wrapping_add(1);
                }
                let status = match status {
                    FrameStatus::Untouched => None,
                    FrameStatus::Ok => Some((0, String::new())),
                    FrameStatus::Failed(e) => {
                        trace.extend(e.lines().iter().map(|l| (TraceMask::ERROR, l.clone())));
                        Some((1, e.to_string()))
                    }
                };
                let (status, message) = status.unzip();
                post.post(
                    [
                        p.num_captured
                            .map(|r| ParamUpdate::int32(r, num_captured as i32)),
                        array_counter.map(|(r, count)| ParamUpdate::int32(r, count)),
                        p.execution_time
                            .map(|r| ParamUpdate::float64(r, elapsed.as_secs_f64() * 1e3)),
                        p.file_number.map(|r| ParamUpdate::int32(r, file_number)),
                        p.full_file_name
                            .map(|r| ParamUpdate::octet(r, full_file_name.to_string())),
                        p.file_path_exists
                            .zip(file_path_exists)
                            .map(|(r, v)| ParamUpdate::int32(r, v as i32)),
                        p.write_status
                            .zip(status)
                            .map(|(r, v)| ParamUpdate::int32(r, v)),
                        p.write_message
                            .zip(message)
                            .map(|(r, v)| ParamUpdate::octet(r, v)),
                    ]
                    .into_iter()
                    .flatten()
                    .collect(),
                )
            }
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

    /// What C `createFileName` sets besides the name: FileNumber, and
    /// FilePathExists from its `checkPath` (asynNDArrayDriver.cpp:203-218).
    fn push_create_file_name_updates(&self, updates: &mut Vec<ParamUpdate>) {
        self.push_file_path_exists_update(updates);
        if let Some(idx) = self.params.file_number {
            updates.push(ParamUpdate::Int32 {
                reason: idx,
                addr: 0,
                value: self.file_base.file_number,
            });
        }
    }

    fn push_file_path_exists_update(&self, updates: &mut Vec<ParamUpdate>) {
        if let (Some(idx), Some(exists)) = (
            self.params.file_path_exists,
            self.file_base.file_path_exists(),
        ) {
            updates.push(ParamUpdate::Int32 {
                reason: idx,
                addr: 0,
                value: exists as i32,
            });
        }
    }

    /// Re-publish every readback `NDFileBase` owns, read back out of it.
    ///
    /// A file operation can fail part-way through and leave that state moved:
    /// a failed capture flush has freed the buffer and burnt a file number per
    /// name it made, while the value the controller pushed before the flush
    /// still names the frames it held. Every exit that follows a file
    /// operation re-reads rather than trusting what an earlier line in the
    /// same cycle pushed.
    fn push_file_base_readbacks(&self, updates: &mut Vec<ParamUpdate>) {
        self.push_num_captured_update(updates);
        self.push_create_file_name_updates(updates);
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
        let c = self.0.controller();
        let writer = Arc::clone(&c.writer);
        c.record(&c.file_sets());
        let view = c.view.clone();
        self.0.unlocked(|| {
            let mut writer = writer.lock();
            match &view {
                Some(view) => view.with_evaluated(|list| writer.set_plugin_attributes(list)),
                None => writer.set_plugin_attributes(&NDAttributeList::new()),
            }
            f(&mut *writer)
        })
    }
}

/// The NDPluginFile entry points that reach the file, called on the locked
/// controller so the I/O can release the lock.
pub trait FileControl<W> {
    /// Process an incoming array: auto_save, capture buffering, stream write.
    ///
    /// The frame is kept by reference, as C's `pArray->reserve()` keeps it
    /// (NDPluginFile.cpp:744); nothing here copies the data. `post` takes
    /// the posts C makes inside the callback, ahead of the result's updates.
    fn process_array(&mut self, array: &Arc<NDArray>, post: &mut dyn ParamPost) -> ProcessResult;

    /// C `NDPluginFile::writeInt32` (NDPluginFile.cpp:799-870) on the port
    /// thread: `validate_int32` then, unless that settled the write,
    /// `store_int32` with the value the param keeps.
    fn write_int32(
        &mut self,
        reason: usize,
        value: i32,
        verdict: WriteVerdict<i32>,
        post: &mut dyn ParamPost,
    ) -> WriteOutcome<i32>;

    /// The writes `NDPluginFile::writeInt32` acts on (NDPluginFile.cpp:
    /// 808-856), with the file I/O inside the write as C does it; the
    /// readbacks they leave are side params. A WriteFile with no array or a
    /// `doCapture` that fails (:418-490) is settled with C's console line.
    /// C drops the status each leaves, and the base class's, with `status =
    /// callParamCallbacks();` (:865); not copied, the write fails with
    /// C's `if (status)` line (:867). `post` takes the posts C makes inside
    /// the write.
    fn validate_int32(
        &mut self,
        reason: usize,
        value: i32,
        verdict: WriteVerdict<i32>,
        post: &mut dyn ParamPost,
    ) -> WriteOutcome<i32>;

    /// Mirror a stored int32 setting into the controller. The settings live
    /// here, not in the param library, because `process_array` runs without
    /// it; they are written only from the write path, in write order. `out`
    /// is the write's outcome so far, and the value stored is what it keeps.
    fn store_int32(&mut self, reason: usize, out: WriteOutcome<i32>) -> WriteOutcome<i32>;
}

impl<W: NDFileWriter, P: PortLock<FilePluginController<W>>> FileControl<W> for P {
    fn process_array(&mut self, array: &Arc<NDArray>, post: &mut dyn ParamPost) -> ProcessResult {
        let mut proc_result = ProcessResult::empty();
        let array = Arc::clone(array);
        let c = self.controller();
        c.view = post.attribute_view();
        c.latest_array = Some(Arc::clone(&array));
        // C counts only a frame it saves, buffers or streams, and sets
        // ArrayCounter back for any other (NDPluginFile.cpp:727-731,
        // :749-781, :789).
        proc_result.counted = match c.file_base.mode() {
            NDFileMode::Single => c.auto_save,
            NDFileMode::Capture => false,
            NDFileMode::Stream => c.capture_active,
        };

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
        let mut deleted: Deleted = Ok(());
        let result: ADResult<bool> = match c.file_base.mode() {
            NDFileMode::Single => {
                if c.auto_save {
                    let mut started = std::mem::take(&mut proc_result.param_updates);
                    started.extend(c.write_file_start());
                    post.post(started);
                    let r;
                    (r, deleted) = write_single(self, array, post);
                    let c = self.controller();
                    c.push_num_captured_update(&mut proc_result.param_updates);
                    c.push_write_file_end(&mut proc_result.param_updates);
                    abort_on_writer_abort(self, &mut proc_result.param_updates, &r);
                    r.map(|()| true)
                } else {
                    Ok(false)
                }
            }
            NDFileMode::Capture => {
                if c.capture_active {
                    // C buffers while NumCaptured < NumCapture and ends the
                    // capture once they are equal, whatever the flush returns
                    // (NDPluginFile.cpp:756-768), so NumCapture 0 buffers
                    // nothing and ends on the first frame. `>=` rather than C's
                    // `==`: NumCapture lowered below NumCaptured mid-capture, or
                    // negative, leaves C capturing nothing forever.
                    let target = c.file_base.num_capture_target();
                    // G12: validate frame dims/dtype in Capture mode too.
                    let latest = Arc::clone(&array);
                    let valid = (c.file_base.num_captured() as i64) < i64::from(target)
                        && c.file_base.frame_valid(&array).map_or_else(
                            |lines| {
                                let lines = lines.into_iter().map(|l| (TraceMask::ERROR, l));
                                proc_result.trace.extend(lines);
                                false
                            },
                            |()| true,
                        );
                    if valid {
                        proc_result.counted = true;
                        c.file_base.capture_array(array);
                        c.push_num_captured_update(&mut proc_result.param_updates);
                    }
                    if c.file_base.num_captured() as i64 >= i64::from(target) {
                        let saved = if c.auto_save {
                            let array_counter = c.flush_array_counter(post);
                            let mut posted = c.flush_post(
                                post,
                                &mut proc_result.trace,
                                &mut proc_result.param_updates,
                            );
                            let r;
                            (r, deleted) = NDPluginFileBase::flush_capture_status_io(
                                &mut PortIo::new(self),
                                &latest,
                                &mut posted,
                            );
                            drop(posted);
                            let c = self.controller();
                            proc_result.param_updates.extend(
                                array_counter.map(|(r, start)| ParamUpdate::int32(r, start)),
                            );
                            c.push_write_file_end(&mut proc_result.param_updates);
                            c.push_num_captured_update(&mut proc_result.param_updates);
                            if r.is_ok() {
                                c.push_full_file_name_update(&mut proc_result.param_updates);
                            }
                            r.map(|()| true)
                        } else {
                            Ok(false)
                        };
                        end_capture(self, &mut proc_result.param_updates, &saved);
                        saved
                    } else {
                        Ok(false)
                    }
                } else {
                    Ok(false)
                }
            }
            NDFileMode::Stream => {
                if c.capture_active {
                    // G9: attribute-driven filename override / mid-stream reopen.
                    let reopen =
                        c.apply_filename_attributes(&array, &mut proc_result.param_updates);
                    if reopen && c.file_base.is_open() {
                        if let Err(e) = NDPluginFileBase::force_close_io(&mut PortIo::new(self)) {
                            self.controller().fail_cycle(&mut proc_result, &e);
                            return proc_result;
                        }
                    }
                    let c = self.controller();
                    c.record(&proc_result.param_updates);
                    c.record(&c.write_ok());
                    let r;
                    (r, deleted) = NDPluginFileBase::process_array_status_io(
                        &mut PortIo::new(self),
                        array,
                        &mut |a| post.hand_on(a),
                    );
                    abort_on_writer_abort(self, &mut proc_result.param_updates, &r);
                    let c = self.controller();
                    if r.is_ok() {
                        // C++ NDPluginFile::processCallbacks publishes
                        // NDFileNumCaptured after every successful stream
                        // write — without this an unbounded stream
                        // (NumCapture=0) reports NumCaptured_RBV=0 forever.
                        c.push_num_captured_update(&mut proc_result.param_updates);
                    }
                    let target = c.file_base.num_capture_target();
                    if r.is_ok()
                        && target > 0
                        && c.file_base.num_captured() as i64 >= i64::from(target)
                    {
                        if let Err(e) = NDPluginFileBase::close_stream_io(&mut PortIo::new(self)) {
                            print_deleted(&mut proc_result.trace, &deleted);
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
        } else if let Err(err) = result {
            c.fail_cycle(&mut proc_result, &err);
        }
        // processCallbacks ignores writeFileBase's status here, but not its
        // line; Stream's count is the comment in `process_array_inner`.
        print_deleted(&mut proc_result.trace, &deleted);
        proc_result
    }

    fn write_int32(
        &mut self,
        reason: usize,
        value: i32,
        verdict: WriteVerdict<i32>,
        post: &mut dyn ParamPost,
    ) -> WriteOutcome<i32> {
        let out = self.validate_int32(reason, value, verdict, post);
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
        post: &mut dyn ParamPost,
    ) -> WriteOutcome<i32> {
        let out = act_int32(self, reason, value, verdict, post);
        match out.verdict {
            WriteVerdict::Refuse { .. } => out.print(
                TraceMask::ERROR,
                format!(
                    "NDPluginFile::writeInt32 error, status=3 function={reason}, value={value}\n"
                ),
            ),
            WriteVerdict::Accept(_) => out,
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
            // C keeps the int as written: 0 or less ends a Capture on the first
            // frame here and never ends a Stream (NDPluginFile.cpp:756-781).
            c.file_base.set_num_capture(value);
        } else if Some(reason) == p.create_dir {
            c.file_base.create_dir = value;
        } else if Some(reason) == p.lazy_open {
            c.lazy_open = value != 0;
        } else if Some(reason) == p.delete_driver_file {
            // Unlike LazyOpen, C reads this at each write (NDPluginFile.cpp:378).
            c.file_base.delete_driver_file = value != 0;
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
    if c.file_base.mode() == NDFileMode::Capture {
        // NDPluginFile.cpp:470.
        c.file_base.register_init_frame(c.latest_array.as_deref());
    }
    c.file_base.lazy_open = c.lazy_open;
    // C clears NDFullFileName as a capture starts (NDPluginFile.cpp:847).
    c.file_base.clear_full_file_name();

    if c.file_base.mode() == NDFileMode::Stream && !c.lazy_open && c.multiple_arrays {
        // B9: eager open — needs a frame to know the layout.
        if let Some(array) = c.latest_array.clone() {
            let opened = NDPluginFileBase::open_stream_io(&mut PortIo::new(port), &array);
            let c = port.controller();
            c.push_create_file_name_updates(updates);
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
    c.push_capture_update(updates);
    closed
}

/// The one owner that ends a capture after its flush, whatever the flush
/// returned (NDPluginFile.cpp:762-768): a writer abort through
/// `abort_capture`, anything else through `stop_capture`.
fn end_capture<T, W: NDFileWriter, P: PortLock<FilePluginController<W>>>(
    port: &mut P,
    updates: &mut Vec<ParamUpdate>,
    flushed: &ADResult<T>,
) {
    if !abort_on_writer_abort(port, updates, flushed) {
        let _ = stop_capture(port, updates);
    }
}

/// Run `abort_capture` if `written` is a writer abort, and say whether it ran.
fn abort_on_writer_abort<T, W: NDFileWriter, P: PortLock<FilePluginController<W>>>(
    port: &mut P,
    updates: &mut Vec<ParamUpdate>,
    written: &ADResult<T>,
) -> bool {
    let aborted = matches!(written, Err(e) if e.is_aborted());
    if aborted {
        abort_capture(port, updates);
    }
    aborted
}

/// End the capture after the writer aborted ([`ADError::Aborted`]), the
/// third path of the B8 transition. C's writer sets NDFileCapture and
/// NDWriteFile to 0 (NDFileHDF5.cpp:1549-1552) and has closed its file, so
/// unlike `stop_capture` there is no close and no temp rename.
fn abort_capture<W: NDFileWriter, P: PortLock<FilePluginController<W>>>(
    port: &mut P,
    updates: &mut Vec<ParamUpdate>,
) {
    let c = port.controller();
    c.file_base.writer_aborted();
    c.capture_active = false;
    c.push_capture_update(updates);
    if let Some(idx) = c.params.write_file {
        updates.push(ParamUpdate::Int32 {
            reason: idx,
            addr: 0,
            value: 0,
        });
    }
}

/// The writes `NDPluginFile::writeInt32` acts on (NDPluginFile.cpp:808-856)
/// and the status each leaves, which `FileControl::validate_int32` keeps.
fn act_int32<W: NDFileWriter, P: PortLock<FilePluginController<W>>>(
    port: &mut P,
    reason: usize,
    value: i32,
    verdict: WriteVerdict<i32>,
    post: &mut dyn ParamPost,
) -> WriteOutcome<i32> {
    let c = port.controller();
    c.view = post.attribute_view();
    let p = c.params;
    let mut updates = Vec::new();
    let mut trace = Vec::new();
    if Some(reason) == p.write_file && value != 0 {
        let Some(array) = c.latest_array.clone() else {
            return WriteOutcome::from(zeroed(false))
                .print(
                    TraceMask::ERROR,
                    "NDPluginFile::writeInt32: ERROR, no valid array to write",
                )
                .settled();
        };
        let (result, deleted) = match c.file_base.mode() {
            NDFileMode::Single => {
                post.post(c.write_file_start());
                write_single(port, array, post)
            }
            NDFileMode::Capture => {
                updates.extend(
                    c.flush_array_counter(post)
                        .map(|(r, start)| ParamUpdate::int32(r, start)),
                );
                let mut nothing_set = Vec::new();
                let mut posted = c.flush_post(post, &mut trace, &mut nothing_set);
                NDPluginFileBase::flush_capture_status_io(
                    &mut PortIo::new(port),
                    &array,
                    &mut posted,
                )
            }
            NDFileMode::Stream => {
                c.record(&c.write_ok());
                NDPluginFileBase::process_array_status_io(&mut PortIo::new(port), array, &mut |a| {
                    post.hand_on(a)
                })
            }
        };
        let c = port.controller();
        // C sets the flag back to 0, since this could be a busy record (:820).
        let mut out = WriteOutcome::from(zeroed(deleted.is_ok()));
        out.trace = trace;
        match &result {
            Ok(()) => {
                updates.extend(c.success_updates());
                c.push_num_captured_update(&mut updates);
                c.push_full_file_name_update(&mut updates);
            }
            Err(err) => {
                // The write that failed is `flush_capture` in Capture
                // mode, which lands frames one at a time, so the
                // readbacks have to come back off `file_base`.
                c.push_error_updates(&mut updates, &mut out.trace, false, true, err);
                c.push_file_base_readbacks(&mut updates);
            }
        }
        print_deleted(&mut out.trace, &deleted);
        abort_on_writer_abort(port, &mut updates, &result);
        return out.set_updates(updates);
    }
    if Some(reason) == p.read_file && value != 0 {
        if let Err(err) = c.file_base.refuse_beside_stream("readFileBase") {
            let mut out = WriteOutcome::from(zeroed(false));
            c.push_error_updates(&mut updates, &mut out.trace, true, false, &err);
            return out.set_updates(updates);
        }
        let path = PathBuf::from(c.file_base.open_file_name());
        let layout = NDArray::new(vec![NDDimension::new(1)], NDDataType::UInt8);
        let pool = c.pool.current();
        let result =
            PortIo::new(port).io(|w| read_file_base(w, &path, &layout, pool.pool()).map(Arc::new));
        let c = port.controller();
        let mut out = WriteOutcome::from(zeroed(result.is_ok()));
        return match result {
            Ok(array) => {
                c.latest_array = Some(array.clone());
                updates.extend(c.success_updates());
                c.push_full_file_name_update(&mut updates);
                out.set_updates(updates).publish_read(array)
            }
            Err(err) => {
                // The name, and FileNumber past it, are set before the
                // open that failed (:146-155).
                c.push_error_updates(&mut updates, &mut out.trace, true, false, &err);
                c.push_create_file_name_updates(&mut updates);
                c.push_full_file_name_update(&mut updates);
                let refused: ADResult<()> = Err(err);
                abort_on_writer_abort(port, &mut updates, &refused);
                out.set_updates(updates)
            }
        };
    }
    // C frees on any write, 0 included (:855-856).
    if Some(reason) == p.free_capture {
        c.file_base.clear_capture();
        c.push_num_captured_update(&mut updates);
        return WriteOutcome::from(verdict).set_updates(updates);
    }
    if Some(reason) != p.capture {
        return verdict.into();
    }
    let mut out = c.check_capture(value, verdict);
    if out.settled {
        return out;
    }
    // B8: capture start/stop routes through the single owner.
    let started = if value != 0 {
        start_capture(port, &mut updates)
    } else {
        stop_capture(port, &mut updates)
    };
    match started {
        Ok(()) => out.set_updates(updates),
        Err(e) => {
            port.controller()
                .push_error_updates(&mut updates, &mut out.trace, false, false, &e);
            WriteOutcome {
                verdict: zeroed(false),
                ..out
            }
            .set_updates(updates)
        }
    }
}

/// The flag a C write sets back to 0 (NDPluginFile.cpp:820, :829, :854),
/// failing the write unless the status it leaves is `asynSuccess`.
fn zeroed(status_ok: bool) -> WriteVerdict<i32> {
    if status_ok {
        WriteVerdict::Accept(0)
    } else {
        WriteVerdict::Refuse {
            keep: 0,
            message: String::new(),
        }
    }
}

fn write_single<W: NDFileWriter, P: PortLock<FilePluginController<W>>>(
    port: &mut P,
    array: Arc<NDArray>,
    post: &mut dyn ParamPost,
) -> (ADResult<()>, Deleted) {
    let c = port.controller();
    if let Err(e) = c.file_base.count_single() {
        return (Err(e), Err(None));
    }
    let mut set = Vec::new();
    c.push_num_captured_update(&mut set);
    c.record(&set);
    if let Err(e) = c.file_base.ensure_directory() {
        return (Err(e), Err(None));
    }
    NDPluginFileBase::process_array_status_io(&mut PortIo::new(port), array, &mut |a| {
        post.hand_on(a)
    })
}

/// The line C prints when writeFileBase cannot delete the driver's file
/// (NDPluginFile.cpp:385-387).
fn print_deleted(trace: &mut Vec<(TraceMask, String)>, deleted: &Deleted) {
    if let Err(Some(line)) = deleted {
        trace.push((TraceMask::ERROR, line.clone()));
    }
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
    use crate::plugin::file_base::NDFileOpenMode;
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
        /// Fail only the `write_file` call with this number, from 1.
        fail_write_number: Option<usize>,
        /// Make `read_file` return `array(77)`.
        readable: bool,
        /// Make `open_file` fail with a line of the writer's own.
        fail_open: bool,
        /// Make `write_file` close its file and abort, as NDFileHDF5 does.
        abort_write: bool,
        /// How long each `write_file` takes.
        write_time: std::time::Duration,
        /// The last `set_num_capture`.
        num_capture: Option<i32>,
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
                fail_write_number: None,
                readable: false,
                fail_open: false,
                abort_write: false,
                write_time: std::time::Duration::ZERO,
                num_capture: None,
            }
        }
    }
    impl NDFileWriter for MockWriter {
        fn open_file(&mut self, _p: &Path, _m: NDFileOpenMode, _a: &NDArray) -> ADResult<()> {
            self.opens += 1;
            if self.fail_open {
                return Err(ADError::Reported {
                    lines: vec!["Mock::openFile cannot open\n".into()],
                    message: "cannot open".into(),
                });
            }
            Ok(())
        }
        fn write_file(&mut self, _a: &Arc<NDArray>) -> ADResult<()> {
            self.writes += 1;
            std::thread::sleep(self.write_time);
            if self.abort_write {
                return Err(ADError::Aborted(Box::new(ADError::Reported {
                    lines: vec!["Mock::writeFile Aborting\n".into()],
                    message: "aborted".into(),
                })));
            }
            if let Some(n) = self.fail_write_after {
                if self.writes > n {
                    return Err(ADError::UnsupportedConversion("disk full".into()));
                }
            }
            if self.fail_write_number == Some(self.writes) {
                return Err(ADError::UnsupportedConversion("disk full".into()));
            }
            Ok(())
        }
        fn read_file(&mut self, _pool: &crate::ndarray_pool::NDArrayPool) -> ADResult<NDArray> {
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
        fn set_num_capture(&mut self, n: i32) {
            self.num_capture = Some(n);
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
        // is not this plugin's; one for its port or "all" is.
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("MYFILE");
        let required =
            |dest| c.processing_required(&with_str_attr(array(1), "FilePluginDestination", dest));
        assert!(!required("OTHER"));
        assert!(required("MYFILE"));
        assert!(required("all"));
    }

    #[test]
    fn test_g9_numeric_destination_attr_processed_not_stringified() {
        // ADC-11 parity: C `attrIsProcessingRequired` only compares a
        // *string-typed* FilePluginDestination (getValueInfo guard); a numeric
        // attribute reads as ND_ERROR and the frame is processed. The Rust port
        // must not stringify the numeric (42 -> "42") and then skip the frame.
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("MYFILE");
        assert!(c.processing_required(&with_i32_attr(array(1), "FilePluginDestination", 42)));
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
        let required =
            |dest| c.processing_required(&with_str_attr(array(1), "FilePluginDestination", dest));
        assert!(required("allfoo"));
        assert!(!required("x"));
        assert!(!required("a"));
        assert!(!required("al"));
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
        c.process_array(&Arc::new(array(1)), &mut ()); // cache an array
        start_capture(&mut c, &mut updates).unwrap();
        let _ = &updates;
        c.process_array(&Arc::new(array(2)), &mut ()); // opens + writes
        assert!(c.file_base.is_open());

        c.process_array(
            &Arc::new(with_i32_attr(array(3), "FilePluginClose", 1)),
            &mut (),
        );
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
        c.process_array(&Arc::new(array(1)), &mut ()); // cache a frame for the layout
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
        c.process_array(&Arc::new(array(1)), &mut ());
        let mut updates = Vec::new();
        start_capture(&mut c, &mut updates).unwrap();
        assert!(
            !c.file_base.is_open(),
            "lazy stream does NOT open at capture start"
        );
        c.process_array(&Arc::new(array(2)), &mut ());
        assert!(c.file_base.is_open(), "lazy stream opens on first frame");
    }

    #[test]
    fn test_g12_capture_mode_validates_frames() {
        // G12: Capture mode rejects frames of mismatched dimensions.
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        c.file_base.set_mode(NDFileMode::Capture);
        c.file_base.set_num_capture(10);
        c.latest_array = Some(Arc::new(array(0)));
        let mut updates = Vec::new();
        start_capture(&mut c, &mut updates).unwrap();

        c.process_array(&Arc::new(array(1)), &mut ()); // as the array before the capture
        assert_eq!(c.file_base.num_captured(), 1);

        // Mismatched frame: different dimension size → rejected.
        let mut big = NDArray::new(vec![NDDimension::new(8)], NDDataType::UInt8);
        big.unique_id = 2;
        c.process_array(&Arc::new(big), &mut ());
        assert_eq!(c.file_base.num_captured(), 1, "mismatched frame rejected");

        // Matching frame: accepted.
        c.process_array(&Arc::new(array(3)), &mut ());
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
        c.write_int32(9, 1, WriteVerdict::Accept(1), &mut ());

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
        c.process_array(&Arc::new(array(1)), &mut ());

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
        c.process_array(&Arc::new(array(1)), &mut ());

        c.write_int32(7, 1, WriteVerdict::Accept(1), &mut ());
        assert!(c.capture_active);

        c.writer.lock().fail_close = true;
        let out = c.write_int32(7, 0, WriteVerdict::Accept(0), &mut ());
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
        c.process_array(&Arc::new(array(1)), &mut ());
        let mut updates = Vec::new();
        start_capture(&mut c, &mut updates).unwrap();
        assert!(c.file_base.is_open());

        // Switch to Capture mode while a stream is open.
        let mode = NDFileMode::Capture as i32;
        c.write_int32(5, mode, WriteVerdict::Accept(mode), &mut ());
        assert!(
            !c.file_base.is_open(),
            "mode switch must close the open stream"
        );
        assert!(!c.capture_active);
    }

    /// C buffers a Capture frame only while NumCaptured < NumCapture and ends
    /// the capture once they are equal (NDPluginFile.cpp:756-768), so
    /// NumCapture 0 buffers nothing and ends on the first frame; with AutoSave
    /// that frame's writeFileBase finds the buffer empty (:270-276).
    #[test]
    fn capture_with_num_capture_zero_ends_on_the_first_frame() {
        for auto_save in [false, true] {
            let mut c = capture_controller(true);
            c.file_base.set_num_capture(0);
            c.auto_save = auto_save;
            let r = c.process_array(&Arc::new(array(1)), &mut ());
            assert_eq!(c.file_base.num_captured(), 0, "autosave {auto_save}");
            assert_eq!(c.writer.lock().opens, 0, "autosave {auto_save}");
            assert!(!c.capture_active, "autosave {auto_save}");
            let status = int_update(&r.param_updates, WRITE_STATUS);
            assert_eq!(status, auto_save.then_some(1), "autosave {auto_save}");
            let lines: Vec<&str> = r.trace.iter().map(|(_, l)| l.as_str()).collect();
            let want: &[&str] = if auto_save {
                &["NDPluginFile::writeFileBase: ERROR, capture buffer is empty\n"]
            } else {
                &[]
            };
            assert_eq!(lines, want, "autosave {auto_save}");
        }
    }

    /// C hands the writer NumCapture as the int it is (NDPluginFile.cpp:228,
    /// NDFileHDF5.cpp:187), and a Stream with a negative one never ends (:780).
    #[test]
    fn a_negative_num_capture_reaches_the_writer_signed() {
        let mut c = write_port(0, NDFileMode::Stream as i32);
        c.store_int32(
            c.params.num_capture.unwrap(),
            WriteVerdict::Accept(-1).into(),
        );
        c.latest_array = Some(Arc::new(array(1)));
        capture_on(&mut c);
        for id in 1..=3 {
            c.process_array(&Arc::new(array(id)), &mut ());
        }
        assert_eq!(c.writer.lock().num_capture, Some(-1));
        assert_eq!(c.file_base.num_captured(), 3);
        assert!(c.capture_active);
    }

    /// The controller keeps the frame it was handed, as C's `pArray->reserve()`
    /// does (NDPluginFile.cpp:744); it never copies the data.
    #[test]
    fn the_latest_array_is_the_input_arc_itself() {
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        let input = Arc::new(array(1));
        c.process_array(&input, &mut ());
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
        c.process_array(&Arc::new(array(1)), &mut ()); // latest_array for the eager open
        start_capture(&mut c, &mut updates).unwrap();
        for (id, expected) in [(2, 1), (3, 2)] {
            let r = c.process_array(&Arc::new(array(id)), &mut ());
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

    /// C counts a frame it saves, buffers or streams, whether or not the
    /// write then fails, and sets ArrayCounter back for any other
    /// (NDPluginFile.cpp:727-731, :749-781, :789).
    #[test]
    fn a_file_plugin_counts_the_frames_c_counts() {
        let small = || NDArray::new(vec![NDDimension::new(2)], NDDataType::UInt8);
        for (what, mode, auto_save, capture, fail, frame, counted) in [
            (
                "single autosave",
                NDFileMode::Single,
                true,
                false,
                false,
                array(1),
                true,
            ),
            (
                "single autosave failing",
                NDFileMode::Single,
                true,
                false,
                true,
                array(1),
                true,
            ),
            (
                "single",
                NDFileMode::Single,
                false,
                false,
                false,
                array(1),
                false,
            ),
            (
                "capture buffered",
                NDFileMode::Capture,
                false,
                true,
                false,
                array(1),
                true,
            ),
            (
                "capture invalid",
                NDFileMode::Capture,
                false,
                true,
                false,
                small(),
                false,
            ),
            (
                "capture idle",
                NDFileMode::Capture,
                true,
                false,
                false,
                array(1),
                false,
            ),
            (
                "stream",
                NDFileMode::Stream,
                false,
                true,
                false,
                array(1),
                true,
            ),
            (
                "stream failing",
                NDFileMode::Stream,
                false,
                true,
                true,
                array(1),
                true,
            ),
            (
                "stream idle",
                NDFileMode::Stream,
                false,
                false,
                false,
                array(1),
                false,
            ),
        ] {
            let mut c = capture_controller(true);
            c.file_base.set_mode(mode);
            c.file_base.set_num_capture(5);
            c.auto_save = auto_save;
            c.capture_active = capture;
            // The first frame sets the dims a later one is checked against.
            c.process_array(&Arc::new(array(0)), &mut ());
            c.writer.lock().fail_write_after = fail.then_some(0);
            let r = c.process_array(&Arc::new(frame), &mut ());
            assert_eq!(r.counted, counted, "{what}");
        }
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
        // The frame doCapture(1) registers (NDPluginFile.cpp:470).
        c.file_base.register_init_frame(Some(&array(0)));
        c
    }

    /// C's single-image flush tries every frame under its own name and frees
    /// the buffer however it ends (NDPluginFile.cpp:288-325), so a writer
    /// failing frames 3 and 4 of 4 leaves NumCaptured 0, four numbers burnt
    /// and the failure standing.
    #[test]
    fn a_failed_single_image_flush_frees_the_buffer() {
        let mut c = capture_controller(false);
        c.file_base.set_num_capture(4);
        c.writer.lock().fail_write_after = Some(2);
        let first_number = c.file_base.file_number;

        let mut last = ProcessResult::empty();
        for id in 1..=4 {
            last = c.process_array(&Arc::new(array(id)), &mut ());
        }

        assert_eq!(c.writer.lock().writes, 4, "C writes past the failure");
        assert_eq!(c.file_base.num_captured(), 0);
        assert_eq!(int_update(&last.param_updates, NUM_CAPTURED), Some(0));
        assert_eq!(
            int_update(&last.param_updates, FILE_NUMBER),
            Some(first_number + 4),
            "every name made burns a number, the failed frames' included \
             (asynNDArrayDriver.cpp:216-218)"
        );
        assert_eq!(int_update(&last.param_updates, WRITE_STATUS), Some(1));
    }

    /// C checks the path only where `createFileName` names a file
    /// (asynNDArrayDriver.cpp:203), so a buffered frame and the flush's
    /// WriteFile post (NDPluginFile.cpp:278-279) carry no FilePathExists, and
    /// each flushed frame's post (:320) carries its open's check, the failed
    /// ones' included, as does the error exit.
    #[test]
    fn a_capture_flush_posts_the_path_check_of_each_open() {
        let mut c = capture_controller(false);
        c.file_base.file_path = std::env::temp_dir().display().to_string();
        c.file_base.set_num_capture(4);
        c.writer.lock().fail_write_after = Some(2);

        for id in 1..=3 {
            let r = c.process_array(&Arc::new(array(id)), &mut ());
            assert_eq!(int_update(&r.param_updates, PATH_EXISTS), None, "{id}");
        }
        let mut posts = Vec::new();
        let r = c.process_array(&Arc::new(array(4)), &mut posts);

        assert_eq!(int_update(&posts[0], PATH_EXISTS), None);
        assert_eq!(posts.len(), 5);
        for frame in &posts[1..] {
            assert_eq!(int_update(frame, PATH_EXISTS), Some(1));
        }
        assert_eq!(int_update(&r.param_updates, PATH_EXISTS), Some(1));
    }

    /// C `checkPath` returns before NDFilePathExists on an empty FilePath
    /// (asynNDArrayDriver.cpp:105), so a frame written with no path leaves
    /// FilePathExists_RBV undefined.
    #[test]
    fn an_empty_file_path_does_not_define_file_path_exists() {
        let mut c = capture_controller(false);
        c.params.file_path = Some(66);
        c.store_octet(66, "");
        let r = c.process_array(&Arc::new(array(1)), &mut ());
        assert_eq!(int_update(&r.param_updates, PATH_EXISTS), None);
    }

    /// C sets FilePathExists only in `checkPath`, at `createFileName` and at
    /// a FilePath write (asynNDArrayDriver.cpp:203, :530), so a frame
    /// streamed into an open file leaves the FilePath write's value standing.
    #[test]
    fn a_stream_frame_into_an_open_file_keeps_the_file_path_writes_check() {
        let mut c = FilePluginController::new(MockWriter::new(true));
        c.set_port_name("F");
        c.params.file_path = Some(66);
        c.params.file_path_exists = Some(PATH_EXISTS);
        c.file_base.set_mode(NDFileMode::Stream);
        c.file_base.set_num_capture(0);
        c.capture_active = true;
        c.store_octet(66, &std::env::temp_dir().display().to_string());
        let r = c.process_array(&Arc::new(array(1)), &mut ());
        assert_eq!(int_update(&r.param_updates, PATH_EXISTS), Some(1));

        // The port's FilePath write has set FilePathExists 0 (base_octet).
        c.store_octet(66, "/adcore-no-such-dir-for-path-exists/");
        let r = c.process_array(&Arc::new(array(2)), &mut ());
        assert_eq!(c.writer.lock().opens, 1, "the second frame opens nothing");
        assert_ne!(int_update(&r.param_updates, PATH_EXISTS), Some(1));
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

        let r = c.process_array(&Arc::new(array(1)), &mut ());

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

    /// The control-plane twin of `a_failed_single_image_flush_frees_the_buffer`:
    /// a manual `WriteFile` frees the buffer the same way, and its error exit
    /// publishes the count.
    #[test]
    fn a_failed_manual_write_republishes_the_capture_count() {
        let mut c = capture_controller(false);
        c.file_base.set_num_capture(10); // short of it: buffered, never written
        for id in 1..=4 {
            c.process_array(&Arc::new(array(id)), &mut ());
        }
        assert_eq!(c.file_base.num_captured(), 4, "all four are buffered");

        c.writer.lock().fail_write_after = Some(2);
        let out = c.write_int32(WRITE_FILE, 1, WriteVerdict::Accept(1), &mut ());

        assert_eq!(c.writer.lock().writes, 4, "C writes past the failure");
        assert_eq!(side_int(&out, NUM_CAPTURED), Some(0));
        assert_eq!(side_int(&out, WRITE_STATUS), Some(1));
    }

    /// A multi-array flush frees the buffer and posts NumCaptured 0 however
    /// it ends, a failed open or write included (NDPluginFile.cpp:282, :325,
    /// freeCaptureBuffer); the writer is not called past the failing frame
    /// (:295).
    #[test]
    fn a_failed_multi_array_flush_frees_the_buffer() {
        for fail_open in [false, true] {
            let mut c = capture_controller(true);
            c.file_base.set_num_capture(10); // short of it: buffered, never written
            for id in 1..=3 {
                c.process_array(&Arc::new(array(id)), &mut ());
            }
            c.writer.lock().fail_write_after = Some(1);
            c.writer.lock().fail_open = fail_open;
            let out = c.write_int32(WRITE_FILE, 1, WriteVerdict::Accept(1), &mut ());
            assert_eq!(
                side_int(&out, WRITE_STATUS),
                Some(1),
                "open fails: {fail_open}"
            );
            assert_eq!(
                side_int(&out, NUM_CAPTURED),
                Some(0),
                "open fails: {fail_open}"
            );
            assert_eq!(c.file_base.num_captured(), 0, "open fails: {fail_open}");
            let writes = if fail_open { 0 } else { 2 };
            assert_eq!(c.writer.lock().writes, writes, "open fails: {fail_open}");
        }
    }

    /// C ends the capture once the target is met whatever writeFileBase
    /// returned (NDPluginFile.cpp:762-768), so a failed autosave flush does
    /// not leave the next NumCapture frames to be captured as a second run.
    #[test]
    fn a_failed_autosave_capture_flush_ends_the_capture() {
        const CAPTURE: usize = 66;
        for multi in [true, false] {
            let mut c = capture_controller(multi);
            c.params.capture = Some(CAPTURE);
            c.file_base.set_num_capture(2);
            c.writer.lock().fail_write_after = Some(0);
            c.process_array(&Arc::new(array(1)), &mut ());
            let r = c.process_array(&Arc::new(array(2)), &mut ());
            assert_eq!(int_update(&r.param_updates, WRITE_STATUS), Some(1));
            assert_eq!(
                int_update(&r.param_updates, CAPTURE),
                Some(0),
                "multi {multi}"
            );
            assert!(!c.capture_active, "multi {multi}");
            c.process_array(&Arc::new(array(3)), &mut ());
            assert_eq!(c.file_base.num_captured(), 0, "multi {multi}");
        }
    }

    fn num_captured_posts(posts: &[Vec<ParamUpdate>]) -> Vec<Option<i32>> {
        posts.iter().map(|p| int_update(p, NUM_CAPTURED)).collect()
    }

    fn num_captured_sets(updates: &[ParamUpdate]) -> Vec<i32> {
        updates
            .iter()
            .filter_map(|u| match u {
                ParamUpdate::Int32 {
                    reason: NUM_CAPTURED,
                    value,
                    ..
                } => Some(*value),
                _ => None,
            })
            .collect()
    }

    /// C posts what the callback has set with WriteFile 1 in one
    /// callParamCallbacks, then NumCaptured counted down per frame of the
    /// capture flush (NDPluginFile.cpp:278-279, :315-321), so a monitor sees
    /// 3 2 1 0, and nothing after the flush sets it back to 3.
    #[test]
    fn an_autosave_capture_flush_posts_num_captured_per_frame() {
        for multi in [true, false] {
            let mut c = capture_controller(multi);
            c.file_base.set_num_capture(3);
            let mut posts = Vec::new();
            c.process_array(&Arc::new(array(1)), &mut posts);
            c.process_array(&Arc::new(array(2)), &mut posts);
            assert!(posts.is_empty(), "multi {multi}");
            let r = c.process_array(&Arc::new(array(3)), &mut posts);
            assert_eq!(
                num_captured_posts(&posts),
                [Some(3), Some(2), Some(1), Some(0)],
                "multi {multi}"
            );
            assert_eq!(int_update(&posts[0], WRITE_FILE), Some(1), "multi {multi}");
            assert_eq!(num_captured_sets(&r.param_updates), [0], "multi {multi}");
        }
    }

    /// One sequence of what a port sees: a callParamCallbacks, or a frame
    /// handed to endProcessCallbacks.
    #[derive(Debug, PartialEq)]
    enum Step {
        Post,
        Frame(i32),
    }

    #[derive(Debug, Default)]
    struct Log(Vec<Step>);

    impl ParamPost for Log {
        fn post(&mut self, _updates: Vec<ParamUpdate>) {
            self.0.push(Step::Post);
        }
        fn hand_on(&mut self, array: &Arc<NDArray>) {
            self.0.push(Step::Frame(array.unique_id));
        }
    }

    fn frames(log: &Log) -> Vec<i32> {
        log.0
            .iter()
            .filter_map(|s| match s {
                Step::Frame(id) => Some(*id),
                Step::Post => None,
            })
            .collect()
    }

    /// C hands each frame whose write ran to endProcessCallbacks: a
    /// multi-array flush stops after the failed write, a single-image one
    /// tries every frame, and a frame whose file never opened goes nowhere
    /// (NDPluginFile.cpp:288-322).
    #[test]
    fn a_capture_flush_hands_on_the_frames_it_wrote() {
        for (multi, fail_write, fail_open, handed_on) in [
            (true, None, false, vec![1, 2, 3]),
            (true, Some(2), false, vec![1, 2]),
            (false, Some(2), false, vec![1, 2, 3]),
            (true, None, true, vec![]),
            (false, None, true, vec![]),
        ] {
            let case = format!("multi {multi} fail_write {fail_write:?} fail_open {fail_open}");
            let mut c = capture_controller(multi);
            c.writer.lock().fail_write_number = fail_write;
            c.writer.lock().fail_open = fail_open;
            c.file_base.set_num_capture(3);
            let mut log = Log::default();
            for id in 1..3 {
                c.process_array(&Arc::new(array(id)), &mut log);
                assert!(frames(&log).is_empty(), "{case}");
            }
            let r = c.process_array(&Arc::new(array(3)), &mut log);
            assert_eq!(frames(&log), handed_on, "{case}");
            assert!(r.output_arrays.is_empty(), "{case}");
        }
    }

    /// C's capture loop writes frame N, hands it to endProcessCallbacks,
    /// then posts NumCaptured for it (NDPluginFile.cpp:296-321), so a
    /// downstream plugin has frame N before a monitor sees its count, in
    /// the frame callback and in a WriteFile write alike.
    #[test]
    fn a_capture_flush_hands_each_frame_on_before_its_post() {
        let order = vec![
            Step::Post,
            Step::Frame(1),
            Step::Post,
            Step::Frame(2),
            Step::Post,
            Step::Frame(3),
            Step::Post,
        ];
        for autosave in [true, false] {
            let mut c = capture_controller(true);
            c.auto_save = autosave;
            c.file_base.set_num_capture(3);
            let mut log = Log::default();
            for id in 1..=2 {
                c.process_array(&Arc::new(array(id)), &mut log);
            }
            if autosave {
                c.process_array(&Arc::new(array(3)), &mut log);
            } else {
                c.process_array(&Arc::new(array(3)), &mut log);
                let write_file = c.params.write_file.unwrap();
                c.write_int32(write_file, 1, WriteVerdict::Accept(1), &mut log);
            }
            assert_eq!(log.0, order, "autosave {autosave}");
        }
    }

    /// A single image or a streamed frame goes on once its file opened,
    /// whether or not the write then failed (NDPluginFile.cpp:244-252,
    /// :335-349).
    #[test]
    fn a_saved_or_streamed_frame_is_handed_on_once_its_file_opened() {
        for (mode, fail_write, fail_open, handed_on) in [
            (NDFileMode::Single, false, false, vec![5]),
            (NDFileMode::Single, true, false, vec![5]),
            (NDFileMode::Single, false, true, vec![]),
            (NDFileMode::Stream, false, false, vec![5]),
            (NDFileMode::Stream, true, false, vec![5]),
            (NDFileMode::Stream, false, true, vec![]),
        ] {
            let case = format!("{mode:?} fail_write {fail_write} fail_open {fail_open}");
            let mut c = FilePluginController::new(MockWriter::new(true));
            c.set_port_name("F");
            c.file_base.set_mode(mode);
            c.auto_save = true;
            c.capture_active = true;
            c.writer.lock().fail_write_number = fail_write.then_some(1);
            c.writer.lock().fail_open = fail_open;
            let mut log = Log::default();
            let r = c.process_array(&Arc::new(array(5)), &mut log);
            assert_eq!(frames(&log), handed_on, "{case}");
            assert!(r.output_arrays.is_empty(), "{case}");
        }
    }

    /// WriteFile posts WriteFile 1, writes, then hands on what it wrote with
    /// no beginProcessCallbacks to count it, before its final post
    /// (NDPluginFile.cpp:239-252, :801-820).
    #[test]
    fn write_file_hands_on_the_frame_it_wrote() {
        let mut c = write_port(0, 0);
        c.auto_save = false;
        let frame = Arc::new(array(9));
        let mut log = Log::default();
        c.process_array(&frame, &mut log);
        assert!(log.0.is_empty());
        let write_file = c.params.write_file.unwrap();
        let out = c.write_int32(write_file, 1, WriteVerdict::Accept(1), &mut log);
        assert_eq!(log.0, [Step::Post, Step::Frame(9)]);
        assert_eq!(out.publish, crate::plugin::runtime::Published::Nothing);
    }

    /// An empty capture buffer refuses before WriteFile 1 (NDPluginFile.cpp:
    /// 270-276), so nothing is posted until the callback ends.
    #[test]
    fn an_empty_capture_flush_posts_nothing_before_the_callback_ends() {
        let mut c = capture_controller(true);
        c.file_base.set_num_capture(0);
        let mut posts = Vec::new();
        let r = c.process_array(&Arc::new(array(1)), &mut posts);
        assert!(posts.is_empty());
        assert_eq!(int_update(&r.param_updates, WRITE_STATUS), Some(1));
    }

    /// A failed frame still counts: a single-image flush opens per frame and
    /// counts each (NDPluginFile.cpp:292-318); a multi-array flush counts
    /// past a failed write, and counts none when its file never opened.
    #[test]
    fn a_failed_capture_flush_still_counts_num_captured_down() {
        for (multi, fail_open, counted) in [
            (true, false, vec![Some(2), Some(1), Some(0)]),
            (false, false, vec![Some(2), Some(1), Some(0)]),
            (false, true, vec![Some(2), Some(1), Some(0)]),
            (true, true, vec![]),
        ] {
            let mut c = capture_controller(multi);
            c.file_base.set_num_capture(3);
            c.writer.lock().fail_write_after = Some(1);
            c.writer.lock().fail_open = fail_open;
            c.process_array(&Arc::new(array(1)), &mut ());
            c.process_array(&Arc::new(array(2)), &mut ());
            let mut posts = Vec::new();
            let r = c.process_array(&Arc::new(array(3)), &mut posts);
            let case = format!("multi {multi} fail_open {fail_open}");
            assert_eq!(num_captured_posts(&posts[1..]), counted, "{case}");
            assert_eq!(
                int_update(&r.param_updates, NUM_CAPTURED),
                Some(0),
                "{case}"
            );
        }
    }

    /// A flushed frame's failure is set and printed with that frame's
    /// post. A single-image frame sets OK or Error as its open, write and
    /// close go (NDPluginFile.cpp:41-42, :144-145, :308-309), so the flush
    /// ends on the last frame's; a multi-array frame sets only its write's
    /// failure (:302-313).
    #[test]
    fn a_capture_flush_posts_each_frames_write_status() {
        const WRITE_MESSAGE: usize = 66;
        let message = |updates: &[ParamUpdate]| {
            updates.iter().rev().find_map(|u| match u {
                ParamUpdate::Octet {
                    reason: WRITE_MESSAGE,
                    value,
                    ..
                } => Some(value.clone()),
                _ => None,
            })
        };
        let failed = Some("Error writing file, status=3".to_string());
        let ok = Some(String::new());
        for (multi, statuses, messages, last) in [
            (
                false,
                [Some(0), Some(1), Some(0)],
                [ok.clone(), failed.clone(), ok.clone()],
                0,
            ),
            (true, [None, Some(1), None], [None, failed.clone(), None], 1),
        ] {
            let mut c = capture_controller(multi);
            c.params.write_message = Some(WRITE_MESSAGE);
            c.file_base.set_num_capture(3);
            c.writer.lock().fail_write_number = Some(2);
            c.process_array(&Arc::new(array(1)), &mut ());
            c.process_array(&Arc::new(array(2)), &mut ());
            let mut posts = Vec::new();
            let r = c.process_array(&Arc::new(array(3)), &mut posts);
            let frames = &posts[1..];
            assert_eq!(
                frames
                    .iter()
                    .map(|p| int_update(p, WRITE_STATUS))
                    .collect::<Vec<_>>(),
                statuses,
                "multi {multi}"
            );
            assert_eq!(
                frames.iter().map(|p| message(p)).collect::<Vec<_>>(),
                messages,
                "multi {multi}"
            );
            assert_eq!(
                int_update(&r.param_updates, WRITE_STATUS),
                Some(last),
                "multi {multi}"
            );
            let lines: Vec<&str> = r
                .trace
                .iter()
                .map(|(_, l)| l.as_str())
                .filter(|l| l.contains("Error writing file"))
                .collect();
            assert_eq!(
                lines,
                ["NDPluginFile::writeFileBase Error writing file, status=3\n"],
                "multi {multi}: printed once"
            );
        }
    }

    /// Each flushed frame's post carries the FileNumber and FullFileName its
    /// open left (asynNDArrayDriver.cpp:216-219, NDPluginFile.cpp:51): one
    /// name per frame single-image, the file's for every multi-array frame.
    #[test]
    fn a_capture_flush_posts_each_frames_file_name() {
        const FULL_FILE_NAME: usize = 67;
        let name = |updates: &[ParamUpdate]| {
            updates.iter().rev().find_map(|u| match u {
                ParamUpdate::Octet {
                    reason: FULL_FILE_NAME,
                    value,
                    ..
                } => Some(value.clone()),
                _ => None,
            })
        };
        let sep = std::path::MAIN_SEPARATOR;
        for (multi, numbers, names) in [(false, [6, 7, 8], [5, 6, 7]), (true, [6, 6, 6], [5, 5, 5])]
        {
            let mut c = capture_controller(multi);
            c.params.full_file_name = Some(FULL_FILE_NAME);
            c.file_base.file_path = "/p/".into();
            c.file_base.file_name = "f".into();
            c.file_base.file_template = "%s%s_%d".into();
            c.file_base.file_number = 5;
            c.file_base.set_num_capture(3);
            c.process_array(&Arc::new(array(1)), &mut ());
            c.process_array(&Arc::new(array(2)), &mut ());
            let mut posts = Vec::new();
            c.process_array(&Arc::new(array(3)), &mut posts);
            let frames = &posts[1..];
            assert_eq!(
                frames
                    .iter()
                    .map(|p| int_update(p, FILE_NUMBER))
                    .collect::<Vec<_>>(),
                numbers.map(Some),
                "multi {multi}"
            );
            assert_eq!(
                frames.iter().map(|p| name(p)).collect::<Vec<_>>(),
                names.map(|n| Some(format!("/p{sep}f_{n}"))),
                "multi {multi}"
            );
        }
    }

    /// A WriteFile in Capture mode runs the same flush inside the write
    /// (NDPluginFile.cpp:817-819), posting the same countdown.
    #[test]
    fn a_write_file_capture_flush_posts_num_captured_per_frame() {
        for multi in [true, false] {
            let mut c = capture_controller(multi);
            c.auto_save = false;
            c.file_base.set_num_capture(5);
            for id in 1..=3 {
                c.process_array(&Arc::new(array(id)), &mut ());
            }
            let mut posts = Vec::new();
            let out = c.write_int32(WRITE_FILE, 1, WriteVerdict::Accept(1), &mut posts);
            assert_eq!(
                num_captured_posts(&posts),
                [None, Some(2), Some(1), Some(0)],
                "multi {multi}"
            );
            assert_eq!(side_int(&out, NUM_CAPTURED), Some(0), "multi {multi}");
        }
    }

    /// C `writeFileBase` posts WriteStatus OK, an empty WriteMessage and
    /// WriteFile 1 before a Single or Capture write, and sets WriteFile back
    /// to 0 however the write ends (NDPluginFile.cpp:231-241, :265, :278-279,
    /// :329).
    #[test]
    fn a_single_or_capture_write_posts_write_file_around_it() {
        for mode in [NDFileMode::Single, NDFileMode::Capture] {
            for fail in [false, true] {
                let mut c = capture_controller(true);
                c.file_base.set_mode(mode);
                c.file_base.set_num_capture(1);
                c.writer.lock().fail_write_after = fail.then_some(0);
                let mut posts = Vec::new();
                let r = c.process_array(&Arc::new(array(1)), &mut posts);
                let case = format!("{mode:?} fail {fail}");
                let start = posts
                    .iter()
                    .find(|p| int_update(p, WRITE_FILE) == Some(1))
                    .unwrap_or_else(|| panic!("{case}: no WriteFile 1 post"));
                assert_eq!(int_update(start, WRITE_STATUS), Some(0), "{case}");
                assert_eq!(int_update(&r.param_updates, WRITE_FILE), Some(0), "{case}");
                assert_eq!(
                    int_update(&r.param_updates, WRITE_STATUS),
                    Some(fail as i32),
                    "{case}"
                );
            }
        }
    }

    /// Stream mode leaves WriteFile to `doCapture`, which holds it at 1 for
    /// the whole stream (NDPluginFile.cpp:476-487): a streamed frame does
    /// not set it.
    #[test]
    fn a_streamed_frame_leaves_write_file_alone() {
        let mut c = capture_controller(true);
        c.file_base.set_mode(NDFileMode::Stream);
        c.file_base.set_num_capture(3);
        let mut posts = Vec::new();
        let r = c.process_array(&Arc::new(array(1)), &mut posts);
        assert_eq!(c.writer.lock().writes, 1);
        assert!(posts.iter().all(|p| int_update(p, WRITE_FILE).is_none()));
        assert_eq!(int_update(&r.param_updates, WRITE_FILE), None);
    }

    /// C times each frame of a capture flush and posts it as ExecutionTime
    /// with that frame's NumCaptured (NDPluginFile.cpp:289, :319-321), in
    /// the frame callback and in a WriteFile write alike.
    #[test]
    fn a_capture_flush_posts_execution_time_per_frame() {
        const EXECUTION_TIME: usize = 67;
        let write_time = std::time::Duration::from_millis(5);
        for (multi, autosave) in [(true, true), (false, true), (true, false), (false, false)] {
            let mut c = capture_controller(multi);
            c.params.execution_time = Some(EXECUTION_TIME);
            c.auto_save = autosave;
            c.file_base.set_num_capture(3);
            c.writer.lock().write_time = write_time;
            let mut posts = Vec::new();
            for id in 1..=3 {
                c.process_array(&Arc::new(array(id)), &mut posts);
            }
            if !autosave {
                c.write_int32(WRITE_FILE, 1, WriteVerdict::Accept(1), &mut posts);
            }
            let times: Vec<f64> = posts
                .iter()
                .filter(|p| int_update(p, NUM_CAPTURED).is_some_and(|n| n < 3))
                .map(|p| {
                    p.iter()
                        .find_map(|u| match u {
                            ParamUpdate::Float64 {
                                reason: EXECUTION_TIME,
                                value,
                                ..
                            } => Some(*value),
                            _ => None,
                        })
                        .expect("a frame post without ExecutionTime")
                })
                .collect();
            let case = format!("multi {multi} autosave {autosave}");
            assert_eq!(times.len(), 3, "{case}");
            assert!(
                times.iter().all(|&ms| ms >= write_time.as_secs_f64() * 1e3),
                "{case}: {times:?}"
            );
        }
    }

    /// The posts of a port whose ArrayCounter reads `counter`.
    struct CounterPort {
        posts: Vec<Vec<ParamUpdate>>,
        counter: (usize, i32),
    }

    impl ParamPost for CounterPort {
        fn post(&mut self, updates: Vec<ParamUpdate>) {
            self.posts.push(updates);
        }

        fn hand_on(&mut self, _array: &Arc<NDArray>) {}

        fn int32(&self, reason: usize) -> Option<i32> {
            (reason == self.counter.0).then_some(self.counter.1)
        }
    }

    /// C counts ArrayCounter up from its value per flushed frame, posting
    /// each, then sets it back (NDPluginFile.cpp:283-287, :316-323), in the
    /// frame callback and in a WriteFile write alike.
    #[test]
    fn a_capture_flush_counts_array_counter_per_frame_then_restores_it() {
        const ARRAY_COUNTER: usize = 68;
        for (multi, autosave) in [(true, true), (false, true), (true, false), (false, false)] {
            let case = format!("multi {multi} autosave {autosave}");
            let mut c = capture_controller(multi);
            c.params.array_counter = Some(ARRAY_COUNTER);
            c.auto_save = autosave;
            c.file_base.set_num_capture(3);
            let mut port = CounterPort {
                posts: Vec::new(),
                counter: (ARRAY_COUNTER, 10),
            };
            for id in 1..=2 {
                c.process_array(&Arc::new(array(id)), &mut port);
            }
            let restored = if autosave {
                let r = c.process_array(&Arc::new(array(3)), &mut port);
                int_update(&r.param_updates, ARRAY_COUNTER)
            } else {
                c.process_array(&Arc::new(array(3)), &mut port);
                let out = c.write_int32(WRITE_FILE, 1, WriteVerdict::Accept(1), &mut port);
                side_int(&out, ARRAY_COUNTER)
            };
            let counted: Vec<i32> = port
                .posts
                .iter()
                .filter_map(|p| int_update(p, ARRAY_COUNTER))
                .collect();
            assert_eq!(counted, [11, 12, 13], "{case}");
            assert_eq!(restored, Some(10), "{case}");
        }
    }

    /// A writer abort during that flush ends the capture through the abort
    /// owner alone: one Capture 0 and WriteFile 0, as NDFileHDF5.cpp:1549-1552.
    #[test]
    fn an_aborted_autosave_capture_flush_ends_the_capture_once() {
        const CAPTURE: usize = 66;
        let mut c = capture_controller(true);
        c.params.capture = Some(CAPTURE);
        c.file_base.set_num_capture(2);
        c.writer.lock().abort_write = true;
        c.process_array(&Arc::new(array(1)), &mut ());
        let r = c.process_array(&Arc::new(array(2)), &mut ());
        let captures = r
            .param_updates
            .iter()
            .filter(|u| {
                matches!(
                    u,
                    ParamUpdate::Int32 {
                        reason: CAPTURE,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(captures, 2, "the abort owner's and the error surface's");
        assert_eq!(int_update(&r.param_updates, CAPTURE), Some(0));
        assert_eq!(int_update(&r.param_updates, WRITE_FILE), Some(0));
        assert!(!c.capture_active);
    }

    /// C refuses a Capture-mode WriteFile with nothing captured
    /// (NDPluginFile.cpp:270-276).
    #[test]
    fn a_capture_write_with_an_empty_buffer_reports_it() {
        const WRITE_MESSAGE: usize = 66;
        let mut c = capture_controller(true);
        c.params.write_message = Some(WRITE_MESSAGE);
        c.capture_active = false;
        c.process_array(&Arc::new(array(1)), &mut ());
        let out = c.write_int32(WRITE_FILE, 1, WriteVerdict::Accept(1), &mut ());
        assert_eq!(side_int(&out, WRITE_STATUS), Some(1));
        let message = out.side.iter().rev().find_map(|s| match s {
            super::super::runtime::SideParam::Octet {
                reason: WRITE_MESSAGE,
                value,
                ..
            } => Some(value.as_str()),
            _ => None,
        });
        assert_eq!(message, Some("ERROR, no capture buffer present"));
        let lines: Vec<&str> = out.trace.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(
            lines,
            ["NDPluginFile::writeFileBase: ERROR, capture buffer is empty\n"]
        );
        assert_eq!(c.writer.lock().opens, 0);
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
            // Capture buffers short of its target, so the flush has a frame
            // to fail on and no frame reaches a write.
            c.capture_active = mode == NDFileMode::Capture;
            c.file_base.set_num_capture(10);
            c.process_array(&Arc::new(array(1)), &mut ());
            c.writer.lock().fail_write_after = Some(0);
            let out = c.write_int32(WRITE_FILE, 1, WriteVerdict::Accept(1), &mut ());
            assert_eq!(
                side_int(&out, WRITE_STATUS),
                Some(1),
                "{mode:?} write fails"
            );

            let r = c.process_array(&Arc::new(array(2)), &mut ());
            assert_eq!(
                int_update(&r.param_updates, WRITE_STATUS),
                None,
                "{mode:?}: a frame that wrote nothing must not reset WriteStatus"
            );
        }
    }

    /// C NDFileHDF5::writeFile's abort (NDFileHDF5.cpp:1549-1552): the writer
    /// closed the file itself and set Capture and WriteFile to 0, and
    /// NDPluginFile never runs closeFileBase for it, so nothing is closed or
    /// renamed and the next frame is not written.
    #[test]
    fn a_writer_abort_drops_the_stream_without_a_close() {
        const CAPTURE: usize = 66;
        let dir = std::env::temp_dir().join(format!("abort-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut c = capture_controller(true);
        c.params.capture = Some(CAPTURE);
        c.file_base.set_mode(NDFileMode::Stream);
        c.file_base.file_path = format!("{}/", dir.display());
        c.file_base.temp_suffix = ".tmp".into();
        c.file_base.set_num_capture(10);
        c.capture_active = false;
        c.lazy_open = true;
        c.process_array(&Arc::new(array(1)), &mut ());
        start_capture(&mut c, &mut Vec::new()).unwrap();
        c.process_array(&Arc::new(array(2)), &mut ());
        assert!(c.file_base.is_open());

        c.writer.lock().abort_write = true;
        let r = c.process_array(&Arc::new(array(3)), &mut ());
        assert!(!c.capture_active, "the abort ends the capture");
        assert!(!c.file_base.is_open(), "the writer closed the file");
        assert_eq!(int_update(&r.param_updates, CAPTURE), Some(0));
        assert_eq!(int_update(&r.param_updates, WRITE_FILE), Some(0));
        assert_eq!(int_update(&r.param_updates, WRITE_STATUS), Some(1));
        let lines: Vec<&str> = r.trace.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(
            lines,
            [
                "Mock::writeFile Aborting\n",
                "NDPluginFile::writeFileBase Error writing file, status=3\n"
            ]
        );
        assert_eq!(c.writer.lock().closes, 0, "no closeFileBase after an abort");

        c.process_array(&Arc::new(array(4)), &mut ());
        assert_eq!(c.writer.lock().writes, 2, "Capture=0 writes no more frames");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same abort through a WriteFile put.
    #[test]
    fn a_writer_abort_on_write_file_drops_the_capture() {
        const CAPTURE: usize = 66;
        let mut c = capture_controller(true);
        c.params.capture = Some(CAPTURE);
        c.file_base.set_num_capture(10);
        c.process_array(&Arc::new(array(1)), &mut ());
        c.writer.lock().abort_write = true;
        let out = c.write_int32(WRITE_FILE, 1, WriteVerdict::Accept(1), &mut ());
        assert!(!c.capture_active);
        assert_eq!(side_int(&out, CAPTURE), Some(0));
        assert_eq!(side_int(&out, WRITE_FILE), Some(0));
    }

    /// The success boundary: nothing above may turn a clean flush into an
    /// error surface or leave a stale count behind.
    #[test]
    fn a_clean_capture_flush_reports_no_frames_left_and_no_error() {
        let mut c = capture_controller(false);
        c.file_base.set_num_capture(4);

        let mut last = ProcessResult::empty();
        for id in 1..=4 {
            last = c.process_array(&Arc::new(array(id)), &mut ());
        }

        assert_eq!(c.file_base.num_captured(), 0);
        assert_eq!(int_update(&last.param_updates, NUM_CAPTURED), Some(0));
        assert_eq!(int_update(&last.param_updates, WRITE_STATUS), Some(0));
    }

    /// A controller wired to a port with the NDFile params, at the given
    /// LazyOpen and FileWriteMode.
    fn write_port(lazy: i32, mode: i32) -> FilePluginController<MockWriter> {
        let mut base = asyn_rs::port::PortDriverBase::new(
            "F",
            1,
            Default::default(),
            asyn_rs::port::InterfaceMask::UNMIGRATED_ALL,
            asyn_rs::port::InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
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
        c.write_int32(
            c.params.capture.unwrap(),
            1,
            WriteVerdict::Accept(1),
            &mut (),
        )
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

    /// C opens a Single file from WriteFile or AutoSave, a Capture file, or
    /// one to read, through the writer a Stream file is open on, and loses
    /// the stream (NDPluginFile.cpp:244, :280, :160); each is refused.
    #[test]
    fn an_open_stream_file_refuses_every_other_open() {
        const REFUSED: &str = "ERROR, a stream file is open";
        let mut c = write_port(0, NDFileMode::Stream as i32);
        let p = c.params;
        c.latest_array = Some(Arc::new(array(1)));
        capture_on(&mut c);
        assert!(c.file_base.is_open());
        let untouched = |c: &FilePluginController<MockWriter>, case: &str| {
            let w = c.writer.lock();
            assert_eq!((w.opens, w.closes), (1, 0), "{case}");
            assert!(c.file_base.is_open(), "{case}");
        };

        c.file_base.set_mode(NDFileMode::Single);
        let out = write_file(&mut c);
        assert_eq!(message(&c, &out).as_deref(), Some(REFUSED));
        assert_eq!(
            out.trace,
            error_lines(&[
                "NDPluginFile::writeFileBase: ERROR, a stream file is open\n",
                &refused_line(p.write_file.unwrap(), 1),
            ])
        );
        untouched(&c, "Single WriteFile");

        c.auto_save = true;
        let result = c.process_array(&Arc::new(array(2)), &mut ());
        let saved = result.param_updates.iter().rev().find_map(|u| match u {
            ParamUpdate::Octet { reason, value, .. } if Some(*reason) == p.write_message => {
                Some(value.clone())
            }
            _ => None,
        });
        assert_eq!(saved.as_deref(), Some(REFUSED));
        untouched(&c, "AutoSave frame");

        c.file_base.set_mode(NDFileMode::Capture);
        c.file_base.capture_array(Arc::new(array(3)));
        let out = write_file(&mut c);
        assert_eq!(message(&c, &out).as_deref(), Some(REFUSED));
        untouched(&c, "Capture WriteFile");

        let out = c.write_int32(p.read_file.unwrap(), 1, WriteVerdict::Accept(1), &mut ());
        assert_eq!(message(&c, &out).as_deref(), Some(REFUSED));
        assert_eq!(
            out.trace,
            error_lines(&[
                "NDPluginFile::readFileBase: ERROR, a stream file is open\n",
                &refused_line(p.read_file.unwrap(), 1),
            ])
        );
        untouched(&c, "ReadFile");
    }

    /// A Single write refused beside an open stream sets nothing first, so
    /// the stream's NumCaptured stays its own.
    #[test]
    fn a_refused_single_write_keeps_the_streams_num_captured() {
        let mut c = write_port(0, NDFileMode::Stream as i32);
        let p = c.params;
        c.latest_array = Some(Arc::new(array(1)));
        c.file_base.set_num_capture(10);
        capture_on(&mut c);
        c.process_array(&Arc::new(array(1)), &mut ());
        c.process_array(&Arc::new(array(2)), &mut ());
        assert_eq!(c.file_base.num_captured(), 2);
        c.file_base.set_mode(NDFileMode::Single);

        let nc = p.num_captured.unwrap();
        let out = write_file(&mut c);
        assert_eq!(c.file_base.num_captured(), 2, "Single WriteFile");
        assert_ne!(side_int(&out, nc), Some(1), "Single WriteFile");

        c.auto_save = true;
        let result = c.process_array(&Arc::new(array(3)), &mut ());
        assert_eq!(c.file_base.num_captured(), 2, "AutoSave frame");
        assert_ne!(
            int_update(&result.param_updates, nc),
            Some(1),
            "AutoSave frame"
        );
    }

    /// C's Stream arm neither writes nor hands on a frame isFrameValid
    /// refuses, and sets WriteStatus Error / WriteMessage "Invalid frame.
    /// Ignoring." (NDPluginFile.cpp:333-342), from a frame callback and from
    /// a WriteFile write alike.
    #[test]
    fn an_invalid_stream_frame_is_refused_as_c_is_frame_valid() {
        let mut c = write_port(1, 2);
        c.file_base.set_num_capture(10);
        capture_on(&mut c);
        let (status, msg) = (c.params.write_status.unwrap(), c.params.write_message);
        let mut log = Log::default();
        c.process_array(&Arc::new(array(1)), &mut log);
        let mut odd = NDArray::new(vec![NDDimension::new(8)], NDDataType::UInt8);
        odd.unique_id = 2;
        let r = c.process_array(&Arc::new(odd), &mut log);
        assert_eq!(frames(&log), [1]);
        assert_eq!(c.writer.lock().writes, 1);
        assert_eq!(c.file_base.num_captured(), 1);
        assert_eq!(int_update(&r.param_updates, status), Some(1));
        let posted = r.param_updates.iter().rev().find_map(|u| match u {
            ParamUpdate::Octet { reason, value, .. } if Some(*reason) == msg => {
                Some(value.as_str())
            }
            _ => None,
        });
        assert_eq!(posted, Some("Invalid frame. Ignoring."));

        let write_file = c.params.write_file.unwrap();
        let out = c.write_int32(write_file, 1, WriteVerdict::Accept(1), &mut log);
        assert_eq!(frames(&log), [1]);
        assert_eq!(c.writer.lock().writes, 1);
        assert_eq!(side_int(&out, status), Some(1));
        assert_eq!(
            message(&c, &out).as_deref(),
            Some("Invalid frame. Ignoring.")
        );
    }

    fn shaped(id: i32, dims: &[usize], data_type: NDDataType) -> Arc<NDArray> {
        let dims = dims.iter().map(|&n| NDDimension::new(n)).collect();
        let mut a = NDArray::new(dims, data_type);
        a.unique_id = id;
        Arc::new(a)
    }

    /// C `isFrameValid` compares only bytes per element and the X/Y size
    /// (NDPluginFile.cpp:688-701).
    #[test]
    fn a_frame_valid_compares_only_element_bytes_and_x_y_size() {
        for (case, first, next) in [
            (
                "Int16 to UInt16",
                shaped(1, &[4, 4], NDDataType::Int16),
                shaped(2, &[4, 4], NDDataType::UInt16),
            ),
            (
                "third dimension",
                shaped(1, &[4, 4, 2], NDDataType::Int16),
                shaped(2, &[4, 4, 3], NDDataType::Int16),
            ),
        ] {
            let mut c = write_port(1, NDFileMode::Stream as i32);
            c.file_base.set_num_capture(10);
            capture_on(&mut c);
            let mut log = Log::default();
            c.process_array(&first, &mut log);
            let r = c.process_array(&next, &mut log);
            assert_eq!(frames(&log), [1, 2], "{case}");
            assert!(r.trace.is_empty(), "{case}: {:?}", r.trace);
        }
    }

    /// C registers the frame a file opens with (NDPluginFile.cpp:64), so a
    /// non-lazy stream checks its first frame against the array before the
    /// capture, while a lazy one opens with, and registers, the frame.
    #[test]
    fn a_first_stream_frame_is_checked_against_the_array_it_opened_with() {
        for (lazy, handed_on) in [(0, vec![]), (1, vec![2])] {
            let mut c = write_port(lazy, NDFileMode::Stream as i32);
            c.file_base.set_num_capture(10);
            c.latest_array = Some(Arc::new(array(1)));
            capture_on(&mut c);
            let mut log = Log::default();
            let r = c.process_array(&shaped(2, &[8], NDDataType::UInt8), &mut log);
            assert_eq!(frames(&log), handed_on, "lazy {lazy}");
            let refused = error_lines(&[
                "NDPluginFile::isFrameValid: WARNING: Frame dimensions have changed X:4,8 Y:0,0]\n",
            ]);
            assert_eq!(r.trace == refused, lazy == 0, "lazy {lazy}: {:?}", r.trace);
        }
    }

    /// C doCapture(1) registers the array before the capture
    /// (NDPluginFile.cpp:470), and buffers only frames that match it (:756).
    #[test]
    fn a_capture_buffers_only_frames_matching_the_array_before_it() {
        let mut c = write_port(0, NDFileMode::Capture as i32);
        c.file_base.set_num_capture(10);
        c.latest_array = Some(Arc::new(array(1)));
        capture_on(&mut c);
        let r = c.process_array(&shaped(2, &[8], NDDataType::UInt8), &mut ());
        assert_eq!(c.file_base.num_captured(), 0);
        assert_eq!(
            r.trace,
            error_lines(&[
                "NDPluginFile::isFrameValid: WARNING: Frame dimensions have changed X:4,8 Y:0,0]\n",
            ])
        );
        c.process_array(&Arc::new(array(3)), &mut ());
        assert_eq!(c.file_base.num_captured(), 1);
    }

    /// `"%s::%s: ERROR, must collect an array to get dimensions first\n"`
    /// (NDPluginFile.cpp:435): Capture and Stream alike, Capture left 0.
    #[test]
    fn capture_without_an_array_fails_unless_lazy_as_c_do_capture_does() {
        for mode in [1, 2] {
            let mut c = write_port(0, mode);
            let out = capture_on(&mut c);
            assert_eq!(out.verdict, zeroed(false));
            assert!(out.settled);
            assert_eq!(
                out.trace,
                error_lines(&[
                    "NDPluginFile::doCapture: ERROR, must collect an array to get dimensions first\n",
                    &refused_line(c.params.capture.unwrap(), 1),
                ])
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
        assert_eq!(out.verdict, zeroed(false));
        assert!(out.settled);
        assert_eq!(
            out.trace,
            error_lines(&[
                "NDPluginFile::doCapture ERROR: No arrays collected: cannot allocate capture buffer\n",
                &refused_line(c.params.capture.unwrap(), 1),
            ])
        );
    }

    /// NDPluginFile.cpp:452 makes only capture=1 an error in Single mode; a
    /// stop succeeds with no line and no WriteMessage, and a start still fails.
    #[test]
    fn a_stop_in_single_mode_succeeds() {
        let mut c = write_port(0, NDFileMode::Single as i32);
        c.latest_array = Some(Arc::new(array(1)));
        let capture = c.params.capture.unwrap();
        let out = c.write_int32(capture, 0, WriteVerdict::Accept(0), &mut ());
        assert_eq!(out.verdict, WriteVerdict::Accept(0));
        assert!(out.trace.is_empty(), "{:?}", out.trace);
        assert_eq!(message(&c, &out), None);
        assert!(!c.capture_active);

        let out = capture_on(&mut c);
        assert_eq!(out.verdict, zeroed(false));
        assert_eq!(
            message(&c, &out).as_deref(),
            Some("ERROR: capture not supported in Single mode")
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
    /// fails.
    #[test]
    fn write_file_without_an_array_prints_as_c_write_int32_does() {
        let mut c = write_port(0, 0);
        let write_file = c.params.write_file.unwrap();
        let out = c.write_int32(write_file, 1, WriteVerdict::Accept(1), &mut ());
        assert_eq!(out.verdict, zeroed(false));
        assert!(out.settled);
        assert!(out.side.is_empty());
        assert_eq!(
            out.trace,
            error_lines(&[
                "NDPluginFile::writeInt32: ERROR, no valid array to write",
                &refused_line(write_file, 1),
            ])
        );
    }

    /// C's `if (status)` line (NDPluginFile.cpp:867).
    fn refused_line(reason: usize, value: i32) -> String {
        format!("NDPluginFile::writeInt32 error, status=3 function={reason}, value={value}\n")
    }

    /// The status C drops at NDPluginFile.cpp:865 fails the write: the base
    /// class's, readFileBase's and writeFileBase's. A Capture flush of an
    /// empty buffer sets WriteStatus but leaves writeFileBase's status at
    /// success (:270-276), and succeeds.
    #[test]
    fn a_write_fails_with_the_status_c_drops() {
        let mut c = write_port(0, 0);
        let clamp = WriteVerdict::Refuse {
            keep: 1,
            message: String::new(),
        };
        let out = c.write_int32(999, 98, clamp.clone(), &mut ());
        assert_eq!(out.verdict, clamp);
        assert_eq!(out.trace, error_lines(&[&refused_line(999, 98)]));

        let read_file = c.params.read_file.unwrap();
        let out = c.write_int32(read_file, 1, WriteVerdict::Accept(1), &mut ());
        assert_eq!(out.verdict, zeroed(false));
        assert_eq!(
            out.trace.last(),
            error_lines(&[&refused_line(read_file, 1)]).last()
        );

        let p = c.params;
        let dir = std::env::temp_dir().to_string_lossy().into_owned() + "/";
        c.store_octet(p.file_path.unwrap(), &dir);
        c.store_octet(p.file_template.unwrap(), "%s%s_%d.x");
        c.latest_array = Some(Arc::new(array(1)));
        c.writer.lock().fail_open = true;
        let write_file = p.write_file.unwrap();
        let out = c.write_int32(write_file, 1, WriteVerdict::Accept(1), &mut ());
        assert_eq!(out.verdict, zeroed(false));
        assert_eq!(
            out.trace.last(),
            error_lines(&[&refused_line(write_file, 1)]).last()
        );

        c.store_int32(p.write_mode.unwrap(), WriteVerdict::Accept(1).into());
        let out = c.write_int32(write_file, 1, WriteVerdict::Accept(1), &mut ());
        assert_eq!(out.verdict, WriteVerdict::Accept(0));
        assert_eq!(side_int(&out, p.write_status.unwrap()), Some(1));
    }

    /// writeFileBase's DeleteDriverFile step fails its status once the
    /// write has landed (NDPluginFile.cpp:376-391): WriteFile fails, C says
    /// why only when `remove` does, and WriteStatus stays the write's. An
    /// empty Capture buffer leaves the status clean, so the step still runs.
    #[test]
    fn a_driver_file_left_undeleted_fails_write_file() {
        let missing = std::env::temp_dir()
            .join(format!("nodel-{}", std::process::id()))
            .join("raw");
        let missing = missing.to_str().unwrap().to_string();
        let unremoved = format!(
            "NDPluginFile::writeFileBase: error deleting file {missing}, error=No such file or directory\n"
        );
        let empty = "NDPluginFile::writeFileBase: ERROR, capture buffer is empty\n";
        for (mode, frame, lines, status) in [
            (
                0,
                with_str_attr(array(1), "DriverFileName", &missing),
                vec![unremoved.as_str()],
                0,
            ),
            (0, with_i32_attr(array(1), "DriverFileName", 7), vec![], 0),
            (
                1,
                with_str_attr(array(1), "DriverFileName", &missing),
                vec![empty, unremoved.as_str()],
                1,
            ),
        ] {
            let mut c = base_port();
            let p = c.params;
            c.store_int32(p.write_mode.unwrap(), WriteVerdict::Accept(mode).into());
            c.file_base.delete_driver_file = true;
            c.latest_array = Some(Arc::new(frame));
            let out = write_file(&mut c);
            assert_eq!(out.verdict, zeroed(false), "{lines:?}");
            let refused = refused_line(p.write_file.unwrap(), 1);
            let mut want = lines.clone();
            want.push(&refused);
            assert_eq!(out.trace, error_lines(&want));
            assert_eq!(side_int(&out, p.write_status.unwrap()), Some(status));
        }
    }

    /// A DeleteDriverFile write takes effect at the next write, with no
    /// capture started in between (NDPluginFile.cpp:378).
    #[test]
    fn delete_driver_file_is_read_at_each_write() {
        let missing = std::env::temp_dir()
            .join(format!("nolatch-{}", std::process::id()))
            .join("raw");
        let missing = missing.to_str().unwrap().to_string();
        let mut c = base_port();
        let p = c.params;
        c.latest_array = Some(Arc::new(with_str_attr(
            array(1),
            "DriverFileName",
            &missing,
        )));
        for (delete, ok) in [(1, false), (0, true)] {
            c.store_int32(
                p.delete_driver_file.unwrap(),
                WriteVerdict::Accept(delete).into(),
            );
            assert_eq!(write_file(&mut c).verdict, zeroed(ok), "{delete}");
        }
    }

    /// processCallbacks drops writeFileBase's status (NDPluginFile.cpp:751),
    /// not the line its DeleteDriverFile step prints.
    #[test]
    fn a_saved_frame_prints_why_its_driver_file_stays() {
        let missing = std::env::temp_dir()
            .join(format!("nosave-{}", std::process::id()))
            .join("raw");
        let missing = missing.to_str().unwrap().to_string();
        let mut c = base_port();
        c.auto_save = true;
        c.file_base.delete_driver_file = true;
        let frame = with_str_attr(array(2), "DriverFileName", &missing);
        let result = c.process_array(&Arc::new(frame), &mut ());
        assert_eq!(
            result.trace,
            error_lines(&[&format!(
                "NDPluginFile::writeFileBase: error deleting file {missing}, error=No such file or directory\n"
            )])
        );
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
        let out = c.write_int32(p.write_file.unwrap(), 1, WriteVerdict::Accept(1), &mut ());
        assert_eq!(out.verdict, WriteVerdict::Accept(0));
        assert_eq!(c.writer.lock().writes, 1);
        assert_eq!(side_int(&out, p.write_status.unwrap()), Some(0));
        assert_eq!(side_int(&out, p.file_number.unwrap()), Some(8));
    }

    /// A writer whose `write_file` holds until the test lets it go.
    struct GateWriter(Arc<std::sync::Barrier>);

    impl NDFileWriter for GateWriter {
        fn open_file(&mut self, _p: &Path, _m: NDFileOpenMode, _a: &NDArray) -> ADResult<()> {
            Ok(())
        }
        fn write_file(&mut self, _a: &Arc<NDArray>) -> ADResult<()> {
            self.0.wait(); // entered
            self.0.wait(); // released
            Ok(())
        }
        fn read_file(&mut self, _pool: &crate::ndarray_pool::NDArrayPool) -> ADResult<NDArray> {
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
        let mut base = asyn_rs::port::PortDriverBase::new(
            "F",
            1,
            Default::default(),
            asyn_rs::port::InterfaceMask::UNMIGRATED_ALL,
            asyn_rs::port::InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
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
                    .write_int32(p.write_file.unwrap(), 1, WriteVerdict::Accept(1), &mut ())
            })
        };
        gate.wait(); // the write is inside write_file
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let frame = {
            let ctrl = Arc::clone(&ctrl);
            std::thread::spawn(move || {
                ctrl.lock().process_array(&Arc::new(array(2)), &mut ());
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
        let out = c.write_int32(read_file, 1, WriteVerdict::Accept(1), &mut ());
        assert_eq!(out.verdict, WriteVerdict::Accept(0));
        let crate::plugin::runtime::Published::Read(read) = &out.publish else {
            panic!("ReadFile publishes its array: {:?}", out.publish);
        };
        assert_eq!(read.unique_id, 77);
        assert!(Arc::ptr_eq(read, c.latest_array.as_ref().unwrap()));
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
            let out = c.write_int32(p.read_file.unwrap(), 1, WriteVerdict::Accept(1), &mut ());
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
        c.write_int32(p.write_mode.unwrap(), 2, WriteVerdict::Accept(2), &mut ());
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
        c.file_base.set_num_capture(10); // short of it: buffered, never written
        c.process_array(&Arc::new(array(1)), &mut ());
        assert_eq!(c.file_base.num_captured(), 1);
        let out = c.write_int32(66, 0, WriteVerdict::Accept(0), &mut ());
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

    /// `base_port`'s first name: checkPath terminates `/d/` with the OS
    /// separator, as C on Windows does.
    fn d_f0() -> String {
        format!("/d{}f_0", std::path::MAIN_SEPARATOR)
    }

    fn write_file(c: &mut FilePluginController<MockWriter>) -> WriteOutcome<i32> {
        let reason = c.params.write_file.unwrap();
        c.write_int32(reason, 1, WriteVerdict::Accept(1), &mut ())
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
                "Mock::openFile cannot open\n",
                &format!(
                    "NDPluginFile::openFileBase Error opening file {}, status=3\n",
                    d_f0()
                ),
                &refused_line(c.params.write_file.unwrap(), 1),
            ])
        );
        assert_eq!(
            message(&c, &out),
            Some(format!("Error opening file {}, status=3", d_f0()))
        );
        assert_eq!(side_int(&out, c.params.write_status.unwrap()), Some(1));
    }

    /// NDPluginFile.cpp:254-260, from a frame (AutoSave) as from WriteFile.
    #[test]
    fn a_failed_frame_write_prints_write_file_base() {
        let mut c = base_port();
        c.auto_save = true;
        c.writer.lock().fail_write_after = Some(0);
        let result = c.process_array(&Arc::new(array(2)), &mut ());
        assert_eq!(
            result.trace,
            error_lines(&["NDPluginFile::writeFileBase Error writing file, status=3\n"])
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
            error_lines(&[
                "NDPluginFile::closeFileBase Error closing file, status=3\n",
                &refused_line(c.params.write_file.unwrap(), 1),
            ])
        );
        assert_eq!(
            message(&c, &out).as_deref(),
            Some("Error closing file, status=3")
        );

        let mut c = base_port();
        c.file_base.temp_suffix = ".tmp".into();
        let out = write_file(&mut c);
        let text = format!("Error renaming temporary file {0}.tmp to {0}", d_f0());
        assert_eq!(
            out.trace,
            error_lines(&[
                &format!("NDPluginFile::closeFileBase {text}\n"),
                &refused_line(c.params.write_file.unwrap(), 1),
            ])
        );
        assert_eq!(message(&c, &out), Some(text));
    }

    /// C `createFileName` runs `checkPath` first (asynNDArrayDriver.cpp:203),
    /// so a WriteFile or ReadFile open terminates FilePath and refreshes
    /// FilePathExists, whatever FilePath was holding.
    #[test]
    fn write_file_and_read_file_check_the_path_as_create_file_name_does() {
        let dir = std::env::temp_dir().join(format!("fc-checkpath-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for op in ["WriteFile", "ReadFile"] {
            let mut c = base_port();
            c.file_base.file_path = dir.display().to_string();
            let reason = match op {
                "WriteFile" => c.params.write_file,
                _ => c.params.read_file,
            };
            let out = c.write_int32(reason.unwrap(), 1, WriteVerdict::Accept(1), &mut ());
            assert_eq!(
                c.file_base.full_file_name(),
                format!("{}{}f_0", dir.display(), std::path::MAIN_SEPARATOR),
                "{op}"
            );
            assert_eq!(
                side_int(&out, c.params.file_path_exists.unwrap()),
                Some(1),
                "{op}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// NDPluginFile.cpp:160-180, the double space included.
    #[test]
    fn a_failed_read_prints_read_file_base() {
        let mut c = base_port();
        let reason = c.params.read_file.unwrap();
        let out = c.write_int32(reason, 1, WriteVerdict::Accept(1), &mut ());
        assert_eq!(
            out.trace,
            error_lines(&[
                &format!(
                    "NDPluginFile::readFileBase Error  file {}, status=3\n",
                    d_f0()
                ),
                &refused_line(reason, 1),
            ])
        );
        assert_eq!(
            message(&c, &out),
            Some(format!("Error  file {}, status=3", d_f0()))
        );

        let mut c = base_port();
        c.writer.lock().fail_open = true;
        let out = c.write_int32(reason, 1, WriteVerdict::Accept(1), &mut ());
        assert_eq!(
            out.trace,
            error_lines(&[
                "Mock::openFile cannot open\n",
                &format!(
                    "NDPluginFile::readFileBase Error opening file {}, status=3\n",
                    d_f0()
                ),
                &refused_line(reason, 1),
            ])
        );
    }
}
