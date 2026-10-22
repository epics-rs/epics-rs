//! File-writing plugin base: file-name construction, the capture buffer, and
//! the flush that drains it.
//!
//! A capture flush frees the buffer however it ends, as C does at
//! `NDPluginFile.cpp:325` (`freeCaptureBuffer()`), so a frame whose write
//! fails is lost.
//!
//! File numbering follows C: `asynNDArrayDriver::createFileName` advances
//! `NDFileNumber` as it makes the name (`asynNDArrayDriver.cpp:216-219`), and
//! [`NDPluginFileBase::open_file_name`] is the one place that does it here, so
//! a failed open or ReadFile consumes its number.
//!
//! C++ line numbers here resolve against ADCore `R3-14-111-g6c53844e`, the
//! revision this port was written from; `NDPluginFile.cpp` is byte-identical
//! between it and the checkout at `R3-14-173-g926bb4c8`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::attributes::{NDAttribute, NDAttributeList};
use crate::error::{ADError, ADResult};
use crate::finalize::Finalize;
use crate::ndarray::NDArray;

/// File write modes matching C++ NDFileMode_t.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NDFileMode {
    Single = 0,
    Capture = 1,
    Stream = 2,
}

impl NDFileMode {
    pub fn from_i32(v: i32) -> Self {
        match v {
            0 => Self::Single,
            1 => Self::Capture,
            _ => Self::Stream,
        }
    }
}

/// C `NDFileOpenMode_t` (NDPluginFile.h:10-16): the access bits `openFile`
/// is called with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NDFileOpenMode(u8);

impl NDFileOpenMode {
    pub const READ: Self = Self(0x01);
    pub const WRITE: Self = Self(0x02);
    pub const APPEND: Self = Self(0x04);
    pub const MULTIPLE: Self = Self(0x08);

    pub fn contains(self, bits: Self) -> bool {
        self.0 & bits.0 == bits.0
    }

    /// The `openFile` refusal of C's write-only formats, which return
    /// asynError for a read or an append and print nothing.
    pub fn refuse_read_or_append(self, writer: &str) -> ADResult<()> {
        if self.contains(Self::READ) || self.contains(Self::APPEND) {
            return Err(ADError::UnsupportedConversion(format!(
                "{writer} opens files only to write"
            )));
        }
        Ok(())
    }
}

impl std::ops::BitOr for NDFileOpenMode {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// Trait for file format writers.
pub trait NDFileWriter: Send + Sync {
    fn open_file(&mut self, path: &Path, mode: NDFileOpenMode, array: &NDArray) -> ADResult<()>;
    /// Write one frame. The frame arrives as its `Arc` (C `NDArray*` with
    /// `reserve()` available) so a writer that materialises the file later
    /// can hold it without copying.
    fn write_file(&mut self, array: &Arc<NDArray>) -> ADResult<()>;
    fn read_file(&mut self) -> ADResult<NDArray>;
    fn close_file(&mut self) -> ADResult<()>;
    fn supports_multiple_arrays(&self) -> bool {
        true
    }
    /// Inform the writer of the configured capture count (`NDFileNumCapture`)
    /// before the next `open_file`, signed as C's `int`. A writer whose
    /// on-disk layout depends on the capture target (e.g. HDF5 chunk sizing,
    /// C `calculateAttributeChunking`) uses it; the default ignores it.
    fn set_num_capture(&mut self, _n: i32) {}
    /// The plugin's own NDAttributesFile set as C `getAttributes` returns it
    /// to `openFile`/`writeFile`, evaluated by the processor ahead of the
    /// call because the evaluation cannot run under the writer lock. A
    /// writer whose C counterpart never calls `getAttributes` ignores it.
    fn set_plugin_attributes(&mut self, _attributes: NDAttributeList) {}
    /// Whether [`write_file`](NDFileWriter::write_file) has put the frame on
    /// disk by the time it returns.
    ///
    /// A writer that answers `false` only materialises the file in
    /// `close_file`; see [`NDPluginFileBase::open_stream`] for what that rules
    /// out. The default is `true`.
    fn writes_incrementally(&self) -> bool {
        true
    }
}

/// A multi-frame writer's own NDAttribute list — C's `pFileAttributes`
/// (`NDFileHDF5.h`, `NDFileNetCDF.h`, `NDFileNexus.h`), which is deliberately
/// separate from the list on any one frame.
///
/// It is **sticky for the life of the file**. `NDAttributeList::copy`
/// (`NDAttributeList.cpp:185-205`) walks the input list and either overwrites
/// the same-named output entry or appends a new one — it never removes — and
/// the writers clear it in `openFile` alone (`NDFileHDF5.cpp:243`,
/// `NDFileNetCDF.cpp:72`) and merge each subsequent frame into it
/// (`NDFileHDF5.cpp:1437`, `NDFileNetCDF.cpp:362`). So an attribute that
/// appears in one frame and is absent from the next keeps its previous value
/// for the rest of the file, and that value is what the later frame's record
/// carries. Reading the frame's own list instead makes a drop-out look like a
/// missing value, which each writer then has to invent a rule for — a zero, or
/// the first frame's value.
///
/// Only [`open`](FileAttributes::open) may reset the contents, so the mid-file
/// clear that would reintroduce that miss cannot be written; and because the
/// list only ever grows, the order of the names is a stable prefix of every
/// later state of it.
#[derive(Debug, Clone, Default)]
pub struct FileAttributes {
    list: NDAttributeList,
}

impl FileAttributes {
    /// Start a new file from the plugin's set and `array`'s attributes on
    /// top (C `openFile`: `clear()`, `getAttributes`, then `copy`).
    pub fn open(&mut self, plugin: &NDAttributeList, array: &NDArray) {
        self.list.clear();
        self.frame(plugin, array);
    }

    /// Merge the plugin's set and one frame's attributes in (C `writeFile`:
    /// `getAttributes` and `copy` with no clear).
    pub fn frame(&mut self, plugin: &NDAttributeList, array: &NDArray) {
        self.list.copy_from(plugin);
        self.list.copy_from(&array.attributes);
    }

    /// The current value for `name`, which is the most recent frame that
    /// carried it, not necessarily the frame being written.
    pub fn get(&self, name: &str) -> Option<&NDAttribute> {
        self.list.get(name)
    }

    /// Every attribute seen since [`open`](FileAttributes::open), in the order
    /// the names first appeared.
    pub fn iter(&self) -> impl Iterator<Item = &NDAttribute> {
        self.list.iter()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
}

/// asynError, the status C's base messages quote for a failed writer call.
const WRITER_ERROR: i32 = 3;

/// A writer call that failed under NDPluginFile's `base` step: the lines the
/// writer printed, then C's own, with C's `message` for `NDFileWriteMessage`.
fn base_failure(writer_lines: &[String], base: &str, message: String) -> ADError {
    let mut lines = writer_lines.to_vec();
    lines.push(format!("NDPluginFile::{base} {message}\n"));
    ADError::Reported { lines, message }
}

/// The writer's `openFile` as C `openFileBase` (NDPluginFile.cpp:62-73) or
/// `readFileBase` (:158-168) reports it.
fn open_file_base<W: NDFileWriter + ?Sized>(
    w: &mut W,
    base: &str,
    path: &Path,
    mode: NDFileOpenMode,
    layout: &NDArray,
) -> ADResult<()> {
    w.open_file(path, mode, layout).map_err(|e| {
        let message = format!(
            "Error opening file {}, status={WRITER_ERROR}",
            path.display()
        );
        let failure = base_failure(e.lines(), base, message);
        if e.is_aborted() {
            ADError::Aborted(Box::new(failure))
        } else {
            failure
        }
    })
}

/// The writer's `writeFile` as C `writeFileBase` reports it
/// (NDPluginFile.cpp:253-260, :302-309, :350-357).
fn write_file_base<W: NDFileWriter + ?Sized>(w: &mut W, array: &Arc<NDArray>) -> ADResult<()> {
    w.write_file(array).map_err(|e| {
        let message = format!("Error writing file, status={WRITER_ERROR}");
        let failure = base_failure(e.lines(), "writeFileBase", message);
        if e.is_aborted() {
            ADError::Aborted(Box::new(failure))
        } else {
            failure
        }
    })
}

/// C `closeFileBase` (NDPluginFile.cpp:83-130): the writer's `closeFile`,
/// then the temp file renamed to its final name when `rename` names both —
/// whether or not the close succeeded. A failed rename's message replaces the
/// close's, and C prints only the one left.
fn close_file_base<W: NDFileWriter + ?Sized>(
    w: &mut W,
    rename: Option<(&Path, &Path)>,
) -> ADResult<()> {
    let closed = w.close_file();
    let renamed = rename.map_or(Ok(()), |(temp, final_path)| {
        std::fs::rename(temp, final_path).map_err(|_| {
            format!(
                "Error renaming temporary file {} to {}",
                temp.display(),
                final_path.display()
            )
        })
    });
    let message = match (&closed, renamed) {
        (_, Err(message)) => message,
        (Err(_), Ok(())) => format!("Error closing file, status={WRITER_ERROR}"),
        (Ok(()), Ok(())) => return Ok(()),
    };
    let writer_lines = closed.as_ref().err().map_or(&[][..], ADError::lines);
    Err(base_failure(writer_lines, "closeFileBase", message))
}

/// Open `path` as the `base` step names it, run `body` against the open
/// writer, and close — exactly once, on every exit path out of `body`.
///
/// The close is the finalizer, so a failing `write_file` or `read_file` cannot
/// leave the writer holding an open file (for HDF5 an H5 file id, plus a stale
/// `current_path`). C skips `closeFileBase` after a failed `writeFile` and
/// leaks the open file (NDPluginFile.cpp:253-263; a capture flush at
/// :302-313 and :326-327); that defect is not copied. Every [`NDFileWriter::close_file`] is a no-op when nothing
/// is open, so an `open_file` that itself fails is safe to close as well, and
/// the guard can cover the open too rather than starting just after it.
///
/// `close` runs only when `body` succeeded; when it failed, its error is the
/// one worth returning and the guard's close reports nothing.
fn with_open_file<W: NDFileWriter + ?Sized, R>(
    writer: &mut W,
    base: &str,
    path: &Path,
    mode: NDFileOpenMode,
    layout: &NDArray,
    body: impl FnOnce(&mut W) -> ADResult<R>,
    close: impl FnOnce(&mut W) -> ADResult<()>,
) -> ADResult<R> {
    let mut open = Finalize::new(writer, |w: &mut W| {
        let _ = w.close_file();
    });
    let value = open.run(|w| {
        open_file_base(w, base, path, mode, layout)?;
        body(w)
    })?;
    let closed = open.run(close);
    open.disarm();
    closed.map(|()| value)
}

/// C `readFileBase`'s file I/O (NDPluginFile.cpp:156-182): open, read, and a
/// close whose status C ignores.
pub(crate) fn read_file_base<W: NDFileWriter + ?Sized>(
    writer: &mut W,
    path: &Path,
    layout: &NDArray,
) -> ADResult<NDArray> {
    const BASE: &str = "readFileBase";
    let read = |w: &mut W| {
        w.read_file().map_err(|e| {
            let message = format!("Error  file {}, status={WRITER_ERROR}", path.display());
            base_failure(e.lines(), BASE, message)
        })
    };
    let close = |w: &mut W| {
        let _ = w.close_file();
        Ok(())
    };
    with_open_file(
        writer,
        BASE,
        path,
        NDFileOpenMode::READ,
        layout,
        read,
        close,
    )
}

/// The file I/O half of NDPluginFile's locking. C runs `openFile`,
/// `writeFile`, `readFile` and `closeFile` with the port lock released and
/// `fileMutex` held (NDPluginFile.cpp:62-76, :247-251), so a frame callback
/// can queue while a file is written. `io` is that window; `base` is the
/// state the port lock guards, and is never reached from inside `io`.
pub trait FileIo {
    fn base(&mut self) -> &mut NDPluginFileBase;
    fn io<R>(&mut self, f: impl FnOnce(&mut dyn NDFileWriter) -> R) -> R;
}

/// A base and writer with no port lock between them.
struct InPlace<'a> {
    base: &'a mut NDPluginFileBase,
    writer: &'a mut dyn NDFileWriter,
}

impl FileIo for InPlace<'_> {
    fn base(&mut self) -> &mut NDPluginFileBase {
        self.base
    }

    fn io<R>(&mut self, f: impl FnOnce(&mut dyn NDFileWriter) -> R) -> R {
        f(self.writer)
    }
}

/// Called after each frame of a write: the time it took, the frame if its
/// write ran, and the failure if it failed.
type FrameWritten<'a> = dyn FnMut(Duration, Option<&Arc<NDArray>>, Option<&ADError>) + 'a;

/// Write `frames` into one file at `path` and rename it to `final_path`,
/// calling `written` after each frame with the time it took, the frame if
/// its write ran, and, for the frame whose write failed, the failure. As C
/// (NDPluginFile.cpp:290-322), a failed frame stops the writes, not the
/// calls; a frame whose write ran is the one C hands to endProcessCallbacks.
fn write_one_file(
    writer: &mut dyn NDFileWriter,
    num_capture: i32,
    mode: NDFileOpenMode,
    path: &Path,
    final_path: Option<&Path>,
    frames: &[Arc<NDArray>],
    written: &mut FrameWritten<'_>,
) -> ADResult<()> {
    writer.set_num_capture(num_capture);
    with_open_file(
        writer,
        "openFileBase",
        path,
        mode,
        &frames[0],
        |w| {
            let mut result = Ok(());
            for arr in frames {
                let start = Instant::now();
                let fresh = result.is_ok();
                if fresh {
                    result = write_file_base(w, arr);
                }
                written(
                    start.elapsed(),
                    fresh.then_some(arr),
                    result.as_ref().err().filter(|_| fresh),
                );
            }
            result
        },
        |w| close_file_base(w, final_path.map(|f| (path, f))),
    )
}

/// A `callParamCallbacks` C makes inside a capture flush.
#[derive(Debug, Clone, Copy)]
pub enum FlushStep<'a> {
    /// The buffer holds frames and the files are about to be opened
    /// (NDPluginFile.cpp:278-279).
    Start,
    /// One buffered frame is done, failed or not, with `num_captured` left,
    /// `elapsed` after it started; a multi-array file that never opened has
    /// none (:288-321).
    Frame {
        num_captured: usize,
        elapsed: Duration,
        status: FrameStatus<'a>,
        /// The frame, when its write ran: C hands it to endProcessCallbacks
        /// (NDPluginFile.cpp:296-301).
        handed_on: Option<&'a Arc<NDArray>>,
        /// FileNumber, FullFileName and FilePathExists once the frame's file
        /// was named (asynNDArrayDriver.cpp:203-219, NDPluginFile.cpp:51).
        file_number: i32,
        full_file_name: &'a str,
        file_path_exists: Option<bool>,
    },
}

/// What a flushed frame left in WriteStatus/WriteMessage.
#[derive(Debug, Clone, Copy)]
pub enum FrameStatus<'a> {
    /// A multi-array frame that wrote, or came after the failed one: C sets
    /// neither (NDPluginFile.cpp:294-313).
    Untouched,
    /// A single-image frame whose open, write and close succeeded, each
    /// setting OK (:41-42, :144-145).
    Ok,
    /// The frame's failure, which C prints and sets as it happens.
    Failed(&'a ADError),
}

/// File path/name management and capture buffering for file plugins.
pub struct NDPluginFileBase {
    pub file_path: String,
    pub file_name: String,
    pub file_number: i32,
    pub file_template: String,
    pub auto_increment: bool,
    pub temp_suffix: String,
    pub create_dir: i32,
    pub lazy_open: bool,
    pub delete_driver_file: bool,
    capture_buffer: Vec<Arc<NDArray>>,
    num_capture: i32,
    num_captured: usize,
    is_open: bool,
    mode: NDFileMode,
    full_file_name: String,
    /// `NDFilePathExists` as the last `checkPath` left it; `None` until a
    /// non-empty FilePath is checked (asynNDArrayDriver.cpp:104).
    file_path_exists: Option<bool>,
    /// C `ndArrayInfoInit`: what `isFrameValid` compares a frame against.
    init_frame: Option<InitFrame>,
}

/// The part of C's `NDArrayInfo_t` that `isFrameValid` compares.
#[derive(Debug, Clone, Copy, PartialEq)]
struct InitFrame {
    bytes_per_element: usize,
    x_size: usize,
    y_size: usize,
}

impl InitFrame {
    fn of(array: &NDArray) -> Self {
        let info = array.info();
        Self {
            bytes_per_element: info.bytes_per_element,
            x_size: info.x_size,
            y_size: info.y_size,
        }
    }
}

impl NDPluginFileBase {
    pub fn new() -> Self {
        Self {
            file_path: String::new(),
            file_name: String::new(),
            file_number: 0,
            file_template: String::new(),
            auto_increment: false,
            temp_suffix: String::new(),
            create_dir: 0,
            lazy_open: false,
            delete_driver_file: false,
            capture_buffer: Vec::new(),
            num_capture: 1,
            num_captured: 0,
            is_open: false,
            mode: NDFileMode::Single,
            full_file_name: String::new(),
            file_path_exists: None,
            init_frame: None,
        }
    }

    /// Construct the full file path from template/path/name/number.
    ///
    /// Mimics C `epicsSnprintf(buf, ..., template, filePath, fileName, fileNumber)`.
    /// Template uses printf-style: first `%s` → filePath, second `%s` → fileName,
    /// `%d` (with optional width/precision like `%3.3d`) → fileNumber.
    pub fn create_file_name(&self) -> String {
        let mut result = String::new();
        let mut chars = self.file_template.chars().peekable();
        let mut s_count = 0;
        while let Some(c) = chars.next() {
            if c == '%' {
                // Collect printf flags and the width/precision spec:
                // optional `-` (left-justify) / `0` (zero-pad) flags,
                // digits (width), `.digits` (precision). C++ uses real
                // epicsSnprintf.
                let mut left_justify = false;
                let mut zero_pad = false;
                loop {
                    match chars.peek() {
                        Some('-') => {
                            left_justify = true;
                            chars.next();
                        }
                        Some('0') => {
                            zero_pad = true;
                            chars.next();
                        }
                        _ => break,
                    }
                }
                let mut spec = String::new();
                while let Some(&nc) = chars.peek() {
                    if nc.is_ascii_digit() || nc == '.' {
                        spec.push(nc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                match chars.next() {
                    // `%%` → literal percent.
                    Some('%') if spec.is_empty() && !left_justify => result.push('%'),
                    Some('s') => {
                        s_count += 1;
                        match s_count {
                            1 => result.push_str(&self.file_path),
                            2 => result.push_str(&self.file_name),
                            _ => {}
                        }
                    }
                    Some('d') => {
                        // `width.precision`: precision = minimum digits
                        // (zero-pad), width = minimum field width
                        // (space-pad unless precision provided).
                        let (width, precision) = match spec.split_once('.') {
                            Some((w, p)) => (
                                w.parse::<usize>().unwrap_or(0),
                                p.parse::<usize>().unwrap_or(0),
                            ),
                            None => (spec.parse::<usize>().unwrap_or(0), 0),
                        };
                        // Apply precision first (zero-pad the number).
                        let digits = format!("{:0>prec$}", self.file_number, prec = precision);
                        // Then pad to the field width. The `0` flag
                        // zero-pads (ignored when left-justified or when an
                        // explicit precision is given, per C printf).
                        if digits.len() >= width {
                            result.push_str(&digits);
                        } else if zero_pad && !left_justify && precision == 0 {
                            result.push_str(&format!(
                                "{:0>width$}",
                                self.file_number,
                                width = width
                            ));
                        } else {
                            let pad = " ".repeat(width - digits.len());
                            if left_justify {
                                result.push_str(&digits);
                                result.push_str(&pad);
                            } else {
                                result.push_str(&pad);
                                result.push_str(&digits);
                            }
                        }
                    }
                    Some(other) => {
                        result.push('%');
                        if left_justify {
                            result.push('-');
                        }
                        if zero_pad {
                            result.push('0');
                        }
                        result.push_str(&spec);
                        result.push(other);
                    }
                    None => result.push('%'),
                }
            } else {
                result.push(c);
            }
        }
        result
    }

    /// Get the temp file path (if temp_suffix is set).
    pub fn temp_file_path(&self) -> Option<PathBuf> {
        if self.temp_suffix.is_empty() {
            None
        } else {
            let name = self.create_file_name();
            Some(PathBuf::from(format!("{}{}", name, self.temp_suffix)))
        }
    }

    /// `NDFullFileName`: the name the last open made.
    pub fn full_file_name(&self) -> &str {
        &self.full_file_name
    }

    pub fn clear_full_file_name(&mut self) {
        self.full_file_name.clear();
    }

    /// C `checkPath()` (asynNDArrayDriver.cpp:98-109) on FilePath: normalize
    /// it and record whether the directory exists.
    fn check_path(&mut self) {
        if !self.file_path.is_empty() {
            let exists = crate::driver::ndarray_driver::check_path_str(&mut self.file_path);
            self.file_path_exists = Some(exists);
        }
    }

    pub(crate) fn file_path_exists(&self) -> Option<bool> {
        self.file_path_exists
    }

    /// C `createFileName` at an open (asynNDArrayDriver.cpp:192-219): the
    /// path is checked, the name is made and AutoIncrement moves FileNumber
    /// past it there, whether or not the open then succeeds.
    pub fn open_file_name(&mut self) -> String {
        self.check_path();
        self.full_file_name = self.create_file_name();
        if self.auto_increment {
            self.file_number += 1;
        }
        self.full_file_name.clone()
    }

    /// Create directory if needed.
    /// C ADCore behavior: createDir != 0 → create directories.
    /// Positive or negative values both trigger creation (negative = depth hint in C,
    /// but in practice create_dir_all handles any depth).
    pub fn ensure_directory(&self) -> ADResult<()> {
        if self.create_dir != 0 && !self.file_path.is_empty() {
            std::fs::create_dir_all(&self.file_path)?;
        }
        Ok(())
    }

    /// Open the next file name, under its temp suffix when one is set; the
    /// second path is the final name to rename to. `opening` is the frame
    /// the file opens with, which C `openFileBase` registers for
    /// `isFrameValid` (NDPluginFile.cpp:64).
    fn write_path(&mut self, opening: &NDArray) -> (PathBuf, Option<PathBuf>) {
        self.register_init_frame(Some(opening));
        let final_path = PathBuf::from(self.open_file_name());
        if self.temp_suffix.is_empty() {
            (final_path, None)
        } else {
            let temp = PathBuf::from(format!("{}{}", final_path.display(), self.temp_suffix));
            (temp, Some(final_path))
        }
    }

    /// Delete the driver's original file for `array` when `delete_driver_file`
    /// is set, keyed off the `DriverFileName` attribute. C deletes it for
    /// `pArrays[0]` once per successful writeFileBase, in every write mode
    /// (NDPluginFile.cpp:376-391).
    fn maybe_delete_driver_file(&self, array: &NDArray) {
        if !self.delete_driver_file {
            return;
        }
        // C reads DriverFileName via `getValue(NDAttrString, …)` and only
        // deletes on `asynSuccess`; a numeric attribute returns `ND_ERROR`, so
        // no file is removed (the decimal rendering is never used as a path).
        if let Some(driver_file) = array
            .attributes
            .get("DriverFileName")
            .and_then(|attr| attr.value.as_string_typed())
        {
            if !driver_file.is_empty() {
                let _ = std::fs::remove_file(driver_file);
            }
        }
    }

    /// Process an incoming array according to the current file mode, with
    /// the writer called in place. [`process_array_io`](Self::process_array_io)
    /// is the same step for a caller that has a port lock to release.
    pub fn process_array(
        &mut self,
        array: Arc<NDArray>,
        writer: &mut dyn NDFileWriter,
    ) -> ADResult<()> {
        Self::process_array_io(&mut InPlace { base: self, writer }, array, &mut |_| {})
    }

    /// [`flush_capture_io`](Self::flush_capture_io) with the writer in place
    /// and no port to post to.
    pub fn flush_capture(&mut self, writer: &mut dyn NDFileWriter, out: &NDArray) -> ADResult<()> {
        Self::flush_capture_io(&mut InPlace { base: self, writer }, out, &mut |_| {})
    }

    /// [`open_stream_io`](Self::open_stream_io) with the writer in place.
    pub fn open_stream(&mut self, writer: &mut dyn NDFileWriter, array: &NDArray) -> ADResult<()> {
        Self::open_stream_io(&mut InPlace { base: self, writer }, array)
    }

    /// [`force_close_io`](Self::force_close_io) with the writer in place.
    pub fn force_close(&mut self, writer: &mut dyn NDFileWriter) -> ADResult<()> {
        Self::force_close_io(&mut InPlace { base: self, writer })
    }

    /// [`close_stream_io`](Self::close_stream_io) with the writer in place.
    pub fn close_stream(&mut self, writer: &mut dyn NDFileWriter) -> ADResult<()> {
        Self::close_stream_io(&mut InPlace { base: self, writer })
    }

    /// Process an incoming array according to the current file mode,
    /// passing `handed_on` each frame whose write ran, as C hands it to
    /// endProcessCallbacks (NDPluginFile.cpp:252, :301, :349).
    pub fn process_array_io(
        lock: &mut impl FileIo,
        array: Arc<NDArray>,
        handed_on: &mut dyn FnMut(&Arc<NDArray>),
    ) -> ADResult<()> {
        let base = lock.base();
        let num_capture = base.num_capture;
        match base.mode {
            NDFileMode::Single => {
                base.refuse_beside_stream("writeFileBase")?;
                let (write_path, final_path) = base.write_path(&array);
                lock.io(|w| {
                    write_one_file(
                        w,
                        num_capture,
                        NDFileOpenMode::WRITE,
                        &write_path,
                        final_path.as_deref(),
                        std::slice::from_ref(&array),
                        &mut |_, frame, _| {
                            if let Some(frame) = frame {
                                handed_on(frame);
                            }
                        },
                    )
                })?;
                lock.base().maybe_delete_driver_file(&array);
            }
            NDFileMode::Capture => {
                // C buffers while NumCaptured < NumCapture and writes once they
                // are equal (NDPluginFile.cpp:756-765), so NumCapture 0
                // buffers nothing and finds the buffer empty.
                if (base.num_captured as i64) < i64::from(base.num_capture) {
                    base.capture_buffer.push(Arc::clone(&array));
                    base.num_captured = base.capture_buffer.len();
                }
                if base.num_captured as i64 >= i64::from(base.num_capture) {
                    Self::flush_capture_io(lock, &array, &mut |step| {
                        if let FlushStep::Frame {
                            handed_on: Some(frame),
                            ..
                        } = step
                        {
                            handed_on(frame);
                        }
                    })?;
                }
            }
            NDFileMode::Stream => {
                Self::open_stream_io(lock, &array)?;
                // C checks the frame after the open, then neither writes it
                // nor hands it on (NDPluginFile.cpp:333-342).
                if let Err(lines) = lock.base().frame_valid(&array) {
                    return Err(ADError::Reported {
                        lines,
                        message: "Invalid frame. Ignoring.".into(),
                    });
                }
                let written = lock.io(|w| {
                    w.set_num_capture(num_capture);
                    write_file_base(w, &array)
                });
                handed_on(&array);
                written?;
                let base = lock.base();
                base.maybe_delete_driver_file(&array);
                base.num_captured += 1;
            }
        }
        Ok(())
    }

    /// Flush capture buffer: open file, write all buffered arrays, close.
    ///
    /// For writers that support multiple arrays (HDF5, NeXus), we open once,
    /// write all frames, and close once.
    /// For single-image writers (JPEG, TIFF), we open/write/close for each
    /// frame individually, auto-incrementing the filename between each.
    ///
    /// The flush frees the buffer however it ends, as C does
    /// (NDPluginFile.cpp:325, `freeCaptureBuffer`), so a failed frame is lost.
    /// A multi-array flush writes none past the failing one (:295); a
    /// single-image flush tries every frame and ends on the last one's
    /// outcome (:288-322). A frame's failure goes out with its
    /// [`FlushStep::Frame`], so the error returned for it has no lines left
    /// to print.
    ///
    /// `out` is C's `pArrayOut`, the newest frame (`pArrays[0]`): its driver
    /// file alone is deleted, once, when writeFileBase ends in success
    /// (:376-391).
    ///
    /// `posted` is each `callParamCallbacks` C makes inside the flush, in
    /// order (see [`FlushStep`]).
    pub fn flush_capture_io(
        lock: &mut impl FileIo,
        out: &NDArray,
        posted: &mut dyn FnMut(FlushStep<'_>),
    ) -> ADResult<()> {
        let (result, status_ok) = Self::write_capture(lock, out, posted);
        if status_ok {
            lock.base().maybe_delete_driver_file(out);
        }
        result
    }

    /// The Capture arm of C `writeFileBase` (NDPluginFile.cpp:268-330): the
    /// error to report, and whether C's `status` ends in success. They differ
    /// where C sets WriteStatus but not `status`: the empty-buffer refusal,
    /// and a single-image flush whose last frame landed after an earlier one
    /// failed.
    fn write_capture(
        lock: &mut impl FileIo,
        out: &NDArray,
        posted: &mut dyn FnMut(FlushStep<'_>),
    ) -> (ADResult<()>, bool) {
        if lock.base().capture_buffer.is_empty() {
            // NDPluginFile.cpp:270-276.
            let err = ADError::Reported {
                lines: vec!["NDPluginFile::writeFileBase: ERROR, capture buffer is empty\n".into()],
                message: "ERROR, no capture buffer present".into(),
            };
            return (Err(err), true);
        }
        if let Err(err) = lock.base().refuse_beside_stream("writeFileBase") {
            return (Err(err), false);
        }
        let flushed = Self::write_frames(lock, out, posted);
        // NDPluginFile.cpp:328.
        lock.base().register_init_frame(None);
        flushed
    }

    /// The buffered frames of [`Self::write_capture`], opening with `out` as
    /// C's multi-array open does (NDPluginFile.cpp:280).
    fn write_frames(
        lock: &mut impl FileIo,
        out: &NDArray,
        posted: &mut dyn FnMut(FlushStep<'_>),
    ) -> (ADResult<()>, bool) {
        posted(FlushStep::Start);
        let num_capture = lock.base().num_capture;
        // Frames a capture adds while the files are written start a new
        // buffer.
        let base = lock.base();
        let frames = std::mem::take(&mut base.capture_buffer);
        let mut left = base.num_captured;
        base.num_captured = 0;
        let mut count_down =
            |elapsed,
             status: FrameStatus<'_>,
             handed_on: Option<&Arc<NDArray>>,
             (file_number, name, exists): (i32, &str, Option<bool>)| {
                left = left.saturating_sub(1);
                posted(FlushStep::Frame {
                    num_captured: left,
                    elapsed,
                    status,
                    handed_on,
                    file_number,
                    full_file_name: name,
                    file_path_exists: exists,
                });
            };
        let named = |base: &mut Self| {
            (
                base.file_number,
                base.full_file_name.clone(),
                base.file_path_exists,
            )
        };
        if lock.io(|w| w.supports_multiple_arrays()) {
            // Multi-array format: open once, write all, close once.
            let (write_path, final_path) = lock.base().write_path(out);
            let (number, name, exists) = named(lock.base());
            let mut frame_failed = false;
            let written = lock.io(|w| {
                write_one_file(
                    w,
                    num_capture,
                    NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                    &write_path,
                    final_path.as_deref(),
                    &frames,
                    &mut |elapsed, frame, failed| {
                        // The write's failure is the one returned.
                        frame_failed |= failed.is_some();
                        count_down(
                            elapsed,
                            failed.map_or(FrameStatus::Untouched, FrameStatus::Failed),
                            frame,
                            (number, &name, exists),
                        )
                    },
                )
            });
            let ok = written.is_ok();
            return (
                written.map_err(|e| if frame_failed { e.without_lines() } else { e }),
                ok,
            );
        }
        // Single-image format: open/write/close per frame with auto-increment.
        let mut last = Ok(());
        for arr in &frames {
            let start = Instant::now();
            let (write_path, final_path) = lock.base().write_path(arr);
            let (number, name, exists) = named(lock.base());
            let mut ran = false;
            let written = lock.io(|w| {
                write_one_file(
                    w,
                    num_capture,
                    NDFileOpenMode::WRITE,
                    &write_path,
                    final_path.as_deref(),
                    std::slice::from_ref(arr),
                    &mut |_, frame, _| ran = frame.is_some(),
                )
            });
            // Counted whether or not its file opened (:292-318).
            let status = written
                .as_ref()
                .map_or_else(FrameStatus::Failed, |()| FrameStatus::Ok);
            count_down(
                start.elapsed(),
                status,
                ran.then_some(arr),
                (number, &name, exists),
            );
            last = written;
        }
        let ok = last.is_ok();
        (last.map_err(ADError::without_lines), ok)
    }

    /// Open the Stream-mode file — the single owner of a stream open.
    ///
    /// The eager open at capture start (C++ `doCapture` opens the file then so
    /// a bad path is reported at capture-start rather than on the first frame,
    /// NDPluginFile.cpp:478-479, B9) and the lazy open on the first frame both
    /// come through here, so Stream mode's precondition is stated once: every
    /// frame the mode accepts is on disk when `process_array` returns, which is
    /// what lets the controller publish `NDFileNumCaptured` per frame. A writer
    /// that holds frames until `close_file` cannot keep that promise, and an
    /// unbounded stream (`NumCapture=0`) never reaches a close, so the open is
    /// refused here instead of growing in RAM behind a readback that reports
    /// those frames as captured.
    ///
    /// `array` supplies the layout the writer needs at open time. A no-op when
    /// a file is already open; `is_open` is claimed before the writer opens,
    /// so a frame arriving while the open runs queues behind it on the writer
    /// instead of opening a second file.
    pub fn open_stream_io(lock: &mut impl FileIo, array: &NDArray) -> ADResult<()> {
        if lock.base().is_open {
            return Ok(());
        }
        if !lock.io(|w| w.writes_incrementally()) {
            return Err(ADError::UnsupportedConversion(
                "this file format writes the whole file when it is closed and \
                 cannot stream; use FileWriteMode=Capture"
                    .into(),
            ));
        }
        let base = lock.base();
        let num_capture = base.num_capture;
        let (write_path, _) = base.write_path(array);
        base.is_open = true;
        let opened = lock.io(|w| {
            w.set_num_capture(num_capture);
            open_file_base(
                w,
                "openFileBase",
                &write_path,
                NDFileOpenMode::WRITE | NDFileOpenMode::MULTIPLE,
                array,
            )
        });
        if opened.is_err() {
            lock.base().is_open = false;
        }
        opened
    }

    /// Force a file close (used by the FilePluginClose attribute, G9).
    /// Safe to call when no file is open.
    pub fn force_close_io(lock: &mut impl FileIo) -> ADResult<()> {
        Self::close_stream_io(lock)
    }

    /// Close stream mode.
    ///
    /// The close owns `is_open` and clears it before the writer runs, so a
    /// failing `close_file` or temp rename ends the open state and reports the
    /// error instead of latching the plugin open against a file it can no
    /// longer write.
    pub fn close_stream_io(lock: &mut impl FileIo) -> ADResult<()> {
        let base = lock.base();
        if !base.is_open {
            return Ok(());
        }
        base.is_open = false;
        // C renames the name the open made, NDFullFileName (:96-117).
        let final_name = base.full_file_name.clone();
        let temp_name =
            (!base.temp_suffix.is_empty()).then(|| format!("{}{}", final_name, base.temp_suffix));
        let rename = temp_name
            .as_deref()
            .map(|t| (Path::new(t), Path::new(&final_name)));
        lock.io(|w| close_file_base(w, rename))
    }

    /// The writer closed the stream file itself ([`ADError::Aborted`]): C
    /// runs no `closeFileBase` for it, so there is no close and no rename.
    pub fn writer_aborted(&mut self) {
        self.is_open = false;
    }

    pub fn is_open(&self) -> bool {
        self.is_open
    }

    pub fn set_mode(&mut self, mode: NDFileMode) {
        self.mode = mode;
    }

    pub fn set_num_capture(&mut self, n: i32) {
        self.num_capture = n;
    }

    pub fn num_captured(&self) -> usize {
        self.num_captured
    }

    /// C opens a Single or Capture file, or one to read, through the writer
    /// an open Stream file is on, and the stream is lost (NDPluginFile.cpp:244,
    /// :280, :290 and :160). That is a C defect, not copied: such an open is
    /// refused while a stream file is open.
    pub(crate) fn refuse_beside_stream(&self, caller: &str) -> ADResult<()> {
        if !self.is_open {
            return Ok(());
        }
        Err(ADError::Reported {
            lines: vec![format!(
                "NDPluginFile::{caller}: ERROR, a stream file is open\n"
            )],
            message: "ERROR, a stream file is open".into(),
        })
    }

    pub fn mode(&self) -> NDFileMode {
        self.mode
    }

    pub fn num_capture_target(&self) -> i32 {
        self.num_capture
    }

    pub fn capture_array(&mut self, array: Arc<NDArray>) {
        self.capture_buffer.push(array);
        self.num_captured = self.capture_buffer.len();
    }

    /// Single mode's NumCaptured, which C sets to 1 before the open for
    /// writers that read it there (NDPluginFile.cpp:242-244). Refused first
    /// beside a stream file, whose NumCaptured it would reset.
    pub fn count_single(&mut self) -> ADResult<()> {
        self.refuse_beside_stream("writeFileBase")?;
        self.num_captured = 1;
        Ok(())
    }

    pub fn clear_capture(&mut self) {
        self.capture_buffer.clear();
        self.num_captured = 0;
    }

    /// C `isFrameValid` (NDPluginFile.cpp:665-705): the frame's bytes per
    /// element and X/Y size against the registered frame's, or the lines C
    /// prints refusing it.
    pub fn frame_valid(&self, array: &NDArray) -> Result<(), Vec<String>> {
        let Some(init) = self.init_frame else {
            return Err(vec![
                "NDPluginFile::isFrameValid: no init array info was registered\n".into(),
            ]);
        };
        let frame = InitFrame::of(array);
        let mut lines = Vec::new();
        if init.bytes_per_element != frame.bytes_per_element {
            lines.push(format!(
                "NDPluginFile::isFrameValid: WARNING: Datatype bit-width has changed. \
                 Was: {}bytes now: {}bytes\n",
                init.bytes_per_element, frame.bytes_per_element
            ));
        }
        if (init.x_size, init.y_size) != (frame.x_size, frame.y_size) {
            lines.push(format!(
                "NDPluginFile::isFrameValid: WARNING: Frame dimensions have changed \
                 X:{},{} Y:{},{}]\n",
                init.x_size, frame.x_size, init.y_size, frame.y_size
            ));
        }
        if lines.is_empty() { Ok(()) } else { Err(lines) }
    }

    /// C `registerInitFrameInfo`: the frame later frames are checked
    /// against, or none.
    pub fn register_init_frame(&mut self, array: Option<&NDArray>) {
        self.init_frame = array.map(InitFrame::of);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndarray::{NDDataType, NDDimension};

    /// Test file writer that records operations.
    struct MockWriter {
        opens: Vec<PathBuf>,
        writes: usize,
        closes: usize,
        multi: bool,
        /// Make `close_file` fail, standing in for the ENOSPC / read-only-mount
        /// close that leaves a real writer unable to finalise.
        fail_close: bool,
        /// Make `write_file` fail, standing in for the ENOSPC write.
        fail_write: bool,
        /// Make `write_file` fail for the frame with this `unique_id` only.
        fail_id: Option<i32>,
        /// Whether `write_file` is durable, as netCDF's is not.
        incremental: bool,
    }

    impl MockWriter {
        fn new(multi: bool) -> Self {
            Self {
                opens: Vec::new(),
                writes: 0,
                closes: 0,
                multi,
                fail_close: false,
                fail_write: false,
                fail_id: None,
                incremental: true,
            }
        }
    }

    impl NDFileWriter for MockWriter {
        fn open_file(
            &mut self,
            path: &Path,
            _mode: NDFileOpenMode,
            _array: &NDArray,
        ) -> ADResult<()> {
            self.opens.push(path.to_path_buf());
            Ok(())
        }
        fn write_file(&mut self, array: &Arc<NDArray>) -> ADResult<()> {
            if self.fail_write || self.fail_id == Some(array.unique_id) {
                return Err(crate::error::ADError::UnsupportedConversion(
                    "disk full".into(),
                ));
            }
            self.writes += 1;
            Ok(())
        }
        fn read_file(&mut self) -> ADResult<NDArray> {
            Err(crate::error::ADError::UnsupportedConversion(
                "not implemented".into(),
            ))
        }
        fn close_file(&mut self) -> ADResult<()> {
            self.closes += 1;
            if self.fail_close {
                return Err(crate::error::ADError::UnsupportedConversion(
                    "disk full".into(),
                ));
            }
            Ok(())
        }
        fn supports_multiple_arrays(&self) -> bool {
            self.multi
        }
        fn writes_incrementally(&self) -> bool {
            self.incremental
        }
    }

    fn make_array(id: i32) -> Arc<NDArray> {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = id;
        Arc::new(arr)
    }

    #[test]
    fn test_single_mode() {
        let mut fb = NDPluginFileBase::new();
        fb.file_path = "/tmp/".into();
        fb.file_name = "test_".into();
        fb.file_number = 1;
        fb.auto_increment = true;
        fb.set_mode(NDFileMode::Single);

        let mut writer = MockWriter::new(false);
        fb.process_array(make_array(1), &mut writer).unwrap();

        assert_eq!(writer.opens.len(), 1);
        assert_eq!(writer.writes, 1);
        assert_eq!(writer.closes, 1);
        assert_eq!(fb.file_number, 2); // auto-incremented
    }

    #[test]
    fn test_capture_mode() {
        let mut fb = NDPluginFileBase::new();
        fb.file_path = "/tmp/".into();
        fb.file_name = "cap_".into();
        fb.set_mode(NDFileMode::Capture);
        fb.set_num_capture(3);

        let mut writer = MockWriter::new(true);

        // Buffer 3 arrays
        fb.process_array(make_array(1), &mut writer).unwrap();
        assert_eq!(writer.writes, 0); // not flushed yet
        fb.process_array(make_array(2), &mut writer).unwrap();
        assert_eq!(writer.writes, 0);
        fb.process_array(make_array(3), &mut writer).unwrap();
        // Should have flushed
        assert_eq!(writer.opens.len(), 1);
        assert_eq!(writer.writes, 3);
        assert_eq!(writer.closes, 1);
    }

    #[test]
    fn test_capture_mode_single_image_format() {
        let mut fb = NDPluginFileBase::new();
        fb.file_path = "/tmp/".into();
        fb.file_name = "jpeg_".into();
        fb.file_number = 0;
        fb.auto_increment = true;
        fb.set_mode(NDFileMode::Capture);
        fb.set_num_capture(3);

        let mut writer = MockWriter::new(false); // single-image format

        fb.process_array(make_array(1), &mut writer).unwrap();
        fb.process_array(make_array(2), &mut writer).unwrap();
        fb.process_array(make_array(3), &mut writer).unwrap();
        // Should have flushed with open/write/close per frame
        assert_eq!(writer.opens.len(), 3);
        assert_eq!(writer.writes, 3);
        assert_eq!(writer.closes, 3);
        assert_eq!(fb.file_number, 3); // auto-incremented 3 times
    }

    #[test]
    fn test_stream_mode() {
        let mut fb = NDPluginFileBase::new();
        fb.file_path = "/tmp/".into();
        fb.file_name = "stream_".into();
        fb.set_mode(NDFileMode::Stream);

        let mut writer = MockWriter::new(true);

        fb.process_array(make_array(1), &mut writer).unwrap();
        fb.process_array(make_array(2), &mut writer).unwrap();
        fb.process_array(make_array(3), &mut writer).unwrap();

        assert_eq!(writer.opens.len(), 1); // opened once
        assert_eq!(writer.writes, 3);
        assert_eq!(writer.closes, 0); // not closed yet

        fb.close_stream(&mut writer).unwrap();
        assert_eq!(writer.closes, 1);
    }

    #[test]
    /// `epicsSnprintf` with an empty FileTemplate makes an empty name
    /// (asynNDArrayDriver.cpp:210-211); nothing stands in for the template.
    fn an_empty_template_makes_an_empty_name() {
        let mut fb = NDPluginFileBase::new();
        fb.file_path = "/data/".into();
        fb.file_name = "img_".into();
        fb.file_number = 42;
        assert_eq!(fb.create_file_name(), "");
    }

    #[test]
    fn test_create_file_name_template() {
        let mut fb = NDPluginFileBase::new();
        fb.file_path = "/data/".into();
        fb.file_name = "img_".into();
        fb.file_number = 5;
        fb.file_template = "%s%s%d.tif".into();
        assert_eq!(fb.create_file_name(), "/data/img_5.tif");
    }

    #[test]
    fn test_create_file_name_printf_specs() {
        // B10: %-, width-only space pad, %0Nd zero pad, %%.
        let mut fb = NDPluginFileBase::new();
        fb.file_path = "/d/".into();
        fb.file_name = "f".into();
        fb.file_number = 7;

        fb.file_template = "%s%s_%3.3d.dat".into();
        assert_eq!(fb.create_file_name(), "/d/f_007.dat");

        // Width-only → space pad, right-justified.
        fb.file_template = "%s%s_%5d".into();
        assert_eq!(fb.create_file_name(), "/d/f_    7");

        // Left-justify.
        fb.file_template = "%s%s_%-5d".into();
        assert_eq!(fb.create_file_name(), "/d/f_7    ");

        // Zero-pad flag.
        fb.file_template = "%s%s_%05d".into();
        assert_eq!(fb.create_file_name(), "/d/f_00007");

        // Literal percent.
        fb.file_template = "%s%s_%d%%".into();
        assert_eq!(fb.create_file_name(), "/d/f_7%");
    }

    #[test]
    fn test_auto_increment() {
        let mut fb = NDPluginFileBase::new();
        fb.file_path = "/tmp/".into();
        fb.file_name = "t_".into();
        fb.file_number = 0;
        fb.auto_increment = true;
        fb.set_mode(NDFileMode::Single);

        let mut writer = MockWriter::new(false);
        fb.process_array(make_array(1), &mut writer).unwrap();
        assert_eq!(fb.file_number, 1);
        fb.process_array(make_array(2), &mut writer).unwrap();
        assert_eq!(fb.file_number, 2);
    }

    #[test]
    fn test_temp_suffix() {
        let mut fb = NDPluginFileBase::new();
        fb.file_path = "/data/".into();
        fb.file_name = "img_".into();
        fb.file_template = "%s%s%4.4d".into();
        fb.file_number = 1;
        fb.temp_suffix = ".tmp".into();

        let temp = fb.temp_file_path().unwrap();
        assert_eq!(temp.to_str().unwrap(), "/data/img_0001.tmp");
    }

    /// C closeFileBase renames NDFullFileName (NDPluginFile.cpp:96-117),
    /// the name the open made, not one made again after FileNumber moved.
    #[test]
    fn a_stream_close_renames_the_temp_file_it_opened() {
        let dir = std::env::temp_dir().join(format!("fb-tmp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut fb = NDPluginFileBase::new();
        fb.file_path = format!("{}/", dir.display());
        fb.file_name = "s".into();
        fb.file_template = "%s%s_%d.h5".into();
        fb.file_number = 3;
        fb.auto_increment = true;
        fb.temp_suffix = ".tmp".into();
        fb.set_mode(NDFileMode::Stream);
        let mut writer = Touch(MockWriter::new(true));
        fb.open_stream(&mut writer, &make_array(1)).unwrap();
        assert_eq!(fb.file_number, 4);
        fb.close_stream(&mut writer).unwrap();
        assert!(dir.join("s_3.h5").is_file());
        assert_eq!(fb.file_number, 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A writer whose open creates the file, so a rename has one to move.
    struct Touch(MockWriter);
    impl NDFileWriter for Touch {
        fn open_file(&mut self, p: &Path, m: NDFileOpenMode, a: &NDArray) -> ADResult<()> {
            std::fs::write(p, b"")?;
            self.0.open_file(p, m, a)
        }
        fn write_file(&mut self, a: &Arc<NDArray>) -> ADResult<()> {
            self.0.write_file(a)
        }
        fn read_file(&mut self) -> ADResult<NDArray> {
            self.0.read_file()
        }
        fn close_file(&mut self) -> ADResult<()> {
            self.0.close_file()
        }
        fn supports_multiple_arrays(&self) -> bool {
            self.0.supports_multiple_arrays()
        }
        fn writes_incrementally(&self) -> bool {
            true
        }
    }

    /// C closeFileBase renames the temp file after a failed closeFile as
    /// after a clean one (NDPluginFile.cpp:103-117), in Stream mode and for
    /// a Single file alike; the error is still the close's.
    #[test]
    fn a_failed_close_still_renames_the_temp_file() {
        let dir = std::env::temp_dir().join(format!("fb-closefail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (mode, name) in [
            (NDFileMode::Stream, "s_0.h5"),
            (NDFileMode::Single, "s_1.h5"),
        ] {
            let mut fb = NDPluginFileBase::new();
            fb.file_path = format!("{}/", dir.display());
            fb.file_name = "s".into();
            fb.file_template = "%s%s_%d.h5".into();
            fb.file_number = if mode == NDFileMode::Stream { 0 } else { 1 };
            fb.temp_suffix = ".tmp".into();
            fb.set_mode(mode);
            let mut writer = Touch(MockWriter::new(true));
            writer.0.fail_close = true;
            let err = if mode == NDFileMode::Stream {
                fb.open_stream(&mut writer, &make_array(1)).unwrap();
                fb.close_stream(&mut writer).unwrap_err()
            } else {
                fb.process_array(make_array(1), &mut writer).unwrap_err()
            };
            assert_eq!(err.to_string(), "Error closing file, status=3");
            assert!(dir.join(name).is_file(), "{mode:?}: temp file not renamed");
            assert!(!dir.join(format!("{name}.tmp")).exists());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn make_array_with_driver_file(id: i32, driver_file: &str) -> Arc<NDArray> {
        use crate::attributes::{NDAttrSource, NDAttrValue, NDAttribute};
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = id;
        arr.attributes.add(NDAttribute::new_static(
            "DriverFileName",
            "",
            NDAttrSource::Driver,
            NDAttrValue::String(driver_file.to_string()),
        ));
        Arc::new(arr)
    }

    /// Driver files named `<tag>_<id>` for ids 1..=n, created on disk.
    fn driver_files(tag: &str, n: i32) -> Vec<PathBuf> {
        (1..=n)
            .map(|id| {
                let f = std::env::temp_dir()
                    .join(format!("adcore_{tag}_{id}_{}.raw", std::process::id()));
                std::fs::write(&f, b"x").unwrap();
                f
            })
            .collect()
    }

    fn capture_deleting(tag: &str, num_capture: i32) -> NDPluginFileBase {
        let mut fb = NDPluginFileBase::new();
        fb.file_path = format!("{}/", std::env::temp_dir().display());
        fb.file_name = format!("{tag}_");
        fb.delete_driver_file = true;
        fb.set_mode(NDFileMode::Capture);
        fb.set_num_capture(num_capture);
        fb
    }

    /// C deletes the DriverFileName of `pArrays[0]` once, after the write,
    /// when writeFileBase ends in success (NDPluginFile.cpp:376-391): a
    /// capture flush deletes the newest frame's file, not each frame's.
    #[test]
    fn a_capture_flush_deletes_only_the_newest_driver_file() {
        for multi in [true, false] {
            let files = driver_files(&format!("capdel{multi}"), 2);
            let mut fb = capture_deleting("capdel", 2);
            let mut writer = MockWriter::new(multi);
            for (id, f) in (1..).zip(&files) {
                fb.process_array(
                    make_array_with_driver_file(id, f.to_str().unwrap()),
                    &mut writer,
                )
                .unwrap();
            }
            assert!(files[0].exists(), "multi {multi}: frame 1's file is kept");
            assert!(
                !files[1].exists(),
                "multi {multi}: pArrays[0]'s file is deleted"
            );
            std::fs::remove_file(&files[0]).ok();
        }
    }

    /// C's single-image flush ends on the last frame's status (:288-322), so
    /// a failing last frame keeps the file and fails the flush, and a failing
    /// earlier one does neither.
    #[test]
    fn a_single_image_flush_deletes_on_the_last_frames_status() {
        for (fail_id, deleted) in [(2, false), (1, true)] {
            let files = driver_files(&format!("capsi{fail_id}"), 2);
            let mut fb = capture_deleting("capsi", 2);
            let mut writer = MockWriter::new(false);
            writer.fail_id = Some(fail_id);
            fb.process_array(
                make_array_with_driver_file(1, files[0].to_str().unwrap()),
                &mut writer,
            )
            .unwrap();
            let flushed = fb.process_array(
                make_array_with_driver_file(2, files[1].to_str().unwrap()),
                &mut writer,
            );
            assert_eq!(flushed.is_ok(), deleted, "frame {fail_id} fails");
            assert!(
                files[0].exists(),
                "frame {fail_id} fails: frame 1's file is kept"
            );
            assert_eq!(!files[1].exists(), deleted, "frame {fail_id} fails");
            for f in &files {
                std::fs::remove_file(f).ok();
            }
        }
    }

    /// The empty-buffer refusal leaves C's status at success (:270-276), so
    /// the newest frame's driver file is still deleted.
    #[test]
    fn an_empty_capture_buffer_still_deletes_the_driver_file() {
        let files = driver_files("capempty", 1);
        let mut fb = capture_deleting("capempty", 0);
        let mut writer = MockWriter::new(true);
        let err = fb
            .process_array(
                make_array_with_driver_file(1, files[0].to_str().unwrap()),
                &mut writer,
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "ERROR, no capture buffer present");
        assert!(!files[0].exists());
    }

    #[test]
    fn test_ensure_directory() {
        let fb = NDPluginFileBase::new();
        // With create_dir=0 and empty path, should be a no-op
        fb.ensure_directory().unwrap();
    }

    /// D6: a Single-mode write that fails must still close the file it opened,
    /// or the writer keeps the handle (for HDF5 an H5 file id) until some later
    /// open happens to displace it.
    #[test]
    fn single_write_closes_the_file_when_the_write_fails() {
        let mut fb = NDPluginFileBase::new();
        fb.file_path = "/tmp/".into();
        fb.file_name = "adcore_single_fail_".into();
        fb.set_mode(NDFileMode::Single);

        let mut writer = MockWriter::new(false);
        writer.fail_write = true;
        assert!(fb.process_array(make_array(1), &mut writer).is_err());
        assert_eq!(writer.opens.len(), 1);
        assert_eq!(
            writer.closes, 1,
            "a failed write must still close the file it opened"
        );
    }

    /// D6: the same open/close pairing inside `flush_capture`, for every
    /// frame C tries past a failure (NDPluginFile.cpp:288-322).
    #[test]
    fn flush_capture_closes_the_file_when_a_frame_write_fails() {
        let (mut fb, mut writer) = partial_flush_fixture();
        writer.fail_write = true;
        assert!(fb.flush_capture(&mut writer, &make_array(2)).is_err());
        assert_eq!(writer.opens.len(), 2, "C tries every frame");
        assert_eq!(
            writer.closes, 2,
            "each file opened for a failing frame is still closed"
        );
    }

    /// Two frames buffered for a single-image (open/write/close per frame)
    /// writer, ready to flush.
    fn partial_flush_fixture() -> (NDPluginFileBase, MockWriter) {
        let mut fb = NDPluginFileBase::new();
        fb.file_path = "/tmp/".into();
        fb.file_name = "adcore_partial_".into();
        fb.set_mode(NDFileMode::Capture);
        fb.set_num_capture(2);
        fb.capture_array(make_array(1));
        fb.capture_array(make_array(2));
        (fb, MockWriter::new(false))
    }

    /// D1: a stream close whose `close_file` fails must still leave the base
    /// closed, so the plugin can open a new file once the disk is freed.
    #[test]
    fn close_stream_clears_is_open_when_close_file_fails() {
        let mut fb = NDPluginFileBase::new();
        fb.file_path = "/tmp/".into();
        fb.file_name = "adcore_wedge_".into();
        fb.set_mode(NDFileMode::Stream);

        let mut writer = MockWriter::new(true);
        fb.process_array(make_array(1), &mut writer).unwrap();
        assert!(fb.is_open());

        writer.fail_close = true;
        assert!(
            fb.close_stream(&mut writer).is_err(),
            "the close failure is still reported"
        );
        assert!(
            !fb.is_open(),
            "a failed close must still end the open state"
        );

        writer.fail_close = false;
        fb.process_array(make_array(2), &mut writer).unwrap();
        assert_eq!(
            writer.opens.len(),
            2,
            "the next frame opens a new stream file instead of appending to the stale handle"
        );
    }

    /// D1, second exit path: the temp-to-final rename is the other `?` between
    /// the close and the marker clear.
    #[test]
    fn close_stream_clears_is_open_when_temp_rename_fails() {
        let mut fb = NDPluginFileBase::new();
        // A directory that does not exist, so the rename fails with ENOENT.
        fb.file_path = "/tmp/adcore-no-such-dir-for-close-stream/".into();
        fb.file_name = "norename_".into();
        fb.temp_suffix = ".tmp".into();
        fb.set_mode(NDFileMode::Stream);

        let mut writer = MockWriter::new(true);
        fb.process_array(make_array(1), &mut writer).unwrap();
        assert!(fb.is_open());

        assert!(fb.close_stream(&mut writer).is_err());
        assert!(
            !fb.is_open(),
            "a failed temp rename must still end the open state"
        );
    }

    /// F4: Stream mode's contract is that a frame `process_array` accepts is on
    /// disk when it returns — that is what `NDFileNumCaptured` reports. A
    /// writer that only materialises the file in `close_file` cannot keep it,
    /// and an unbounded stream never reaches a close, so the open is refused
    /// rather than accumulating the whole stream in RAM.
    #[test]
    fn stream_open_is_refused_for_a_writer_that_writes_only_on_close() {
        let mut fb = NDPluginFileBase::new();
        fb.file_path = "/tmp/".into();
        fb.file_name = "stream_".into();
        fb.set_mode(NDFileMode::Stream);
        fb.set_num_capture(0);

        let mut writer = MockWriter::new(true);
        writer.incremental = false;

        assert!(fb.process_array(make_array(1), &mut writer).is_err());
        assert!(writer.opens.is_empty(), "no file may be opened");
        assert_eq!(writer.writes, 0, "no frame may be handed to the writer");
        assert!(!fb.is_open());
        assert_eq!(
            fb.num_captured(),
            0,
            "NumCaptured must not count a frame that is not on disk"
        );
    }
}
