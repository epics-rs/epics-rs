//! File-writing plugin base: file-name construction, the capture buffer, and
//! the flush that drains it.
//!
//! One deliberate deviation from C++ `NDPluginFile` lives here. C frees the
//! capture buffer unconditionally once a flush ends, at `NDPluginFile.cpp:325`
//! (`freeCaptureBuffer()`), so a flush that fails on one frame discards every
//! frame including the ones never written, and a second `WriteFile` finds an
//! empty buffer and reports "no capture buffer present".
//! [`NDPluginFileBase::flush_capture`] instead keeps exactly the frames that
//! have not reached disk, so the operator can free the disk and resume where
//! the flush stopped.
//!
//! File numbering follows C: `asynNDArrayDriver::createFileName` advances
//! `NDFileNumber` as it makes the name (`asynNDArrayDriver.cpp:216-219`), and
//! [`NDPluginFileBase::open_file_name`] is the one place that does it here, so
//! a failed open or ReadFile consumes its number and a resumed flush leaves a
//! gap at the frame that failed, as C would have had there.
//!
//! C++ line numbers here resolve against ADCore `R3-14-111-g6c53844e`, the
//! revision this port was written from; `NDPluginFile.cpp` is byte-identical
//! between it and the checkout at `R3-14-173-g926bb4c8`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// Trait for file format writers.
pub trait NDFileWriter: Send + Sync {
    fn open_file(&mut self, path: &Path, mode: NDFileMode, array: &NDArray) -> ADResult<()>;
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
    /// before the next `open_file`. A writer whose on-disk layout depends on
    /// the capture target (e.g. HDF5 chunk sizing, C `calculateAttributeChunking`)
    /// uses it; the default ignores it.
    fn set_num_capture(&mut self, _n: usize) {}
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
    /// Start a new file from `array`'s attributes (C `openFile`: `clear()`
    /// then `copy`).
    pub fn open(&mut self, array: &NDArray) {
        self.list.clear();
        self.list.copy_from(&array.attributes);
    }

    /// Merge one frame's attributes in (C `writeFile`: `copy` with no clear).
    pub fn frame(&mut self, array: &NDArray) {
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
    lines.push(format!("NDPluginFile::{base} {message}"));
    ADError::Reported { lines, message }
}

/// The writer's `openFile` as C `openFileBase` (NDPluginFile.cpp:62-73) or
/// `readFileBase` (:158-168) reports it.
fn open_file_base<W: NDFileWriter + ?Sized>(
    w: &mut W,
    base: &str,
    path: &Path,
    mode: NDFileMode,
    layout: &NDArray,
) -> ADResult<()> {
    w.open_file(path, mode, layout).map_err(|e| {
        let message = format!(
            "Error opening file {}, status={WRITER_ERROR}",
            path.display()
        );
        base_failure(e.lines(), base, message)
    })
}

/// The writer's `writeFile` as C `writeFileBase` reports it
/// (NDPluginFile.cpp:253-260, :302-309, :350-357).
fn write_file_base<W: NDFileWriter + ?Sized>(w: &mut W, array: &Arc<NDArray>) -> ADResult<()> {
    w.write_file(array).map_err(|e| {
        let message = format!("Error writing file, status={WRITER_ERROR}");
        base_failure(e.lines(), "writeFileBase", message)
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
/// `current_path`). Every [`NDFileWriter::close_file`] is a no-op when nothing
/// is open, so an `open_file` that itself fails is safe to close as well, and
/// the guard can cover the open too rather than starting just after it.
///
/// `close` runs only when `body` succeeded; when it failed, its error is the
/// one worth returning and the guard's close reports nothing.
fn with_open_file<W: NDFileWriter + ?Sized, R>(
    writer: &mut W,
    base: &str,
    path: &Path,
    mode: NDFileMode,
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
    with_open_file(writer, BASE, path, NDFileMode::Single, layout, read, close)
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

/// Write `frames` into one file at `path` and rename it to `final_path`.
fn write_one_file(
    writer: &mut dyn NDFileWriter,
    num_capture: usize,
    mode: NDFileMode,
    path: &Path,
    final_path: Option<&Path>,
    frames: &[Arc<NDArray>],
) -> ADResult<()> {
    writer.set_num_capture(num_capture);
    with_open_file(
        writer,
        "openFileBase",
        path,
        mode,
        &frames[0],
        |w| frames.iter().try_for_each(|arr| write_file_base(w, arr)),
        |w| close_file_base(w, final_path.map(|f| (path, f))),
    )
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
    num_capture: usize,
    num_captured: usize,
    is_open: bool,
    mode: NDFileMode,
    full_file_name: String,
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

    /// C `createFileName` at an open (asynNDArrayDriver.cpp:192-219): the
    /// name is made and AutoIncrement moves FileNumber past it there, whether
    /// or not the open then succeeds.
    pub fn open_file_name(&mut self) -> String {
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
    /// second path is the final name to rename to.
    fn write_path(&mut self) -> (PathBuf, Option<PathBuf>) {
        let final_path = PathBuf::from(self.open_file_name());
        if self.temp_suffix.is_empty() {
            (final_path, None)
        } else {
            let temp = PathBuf::from(format!("{}{}", final_path.display(), self.temp_suffix));
            (temp, Some(final_path))
        }
    }

    /// Delete the driver's original file for `array` when `delete_driver_file`
    /// is set. C++ NDPluginFile deletes the driver file in every write mode
    /// (Single, Stream, and Capture), keyed off the `DriverFileName` attribute.
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
        Self::process_array_io(&mut InPlace { base: self, writer }, array)
    }

    /// [`flush_capture_io`](Self::flush_capture_io) with the writer in place.
    pub fn flush_capture(&mut self, writer: &mut dyn NDFileWriter) -> ADResult<()> {
        Self::flush_capture_io(&mut InPlace { base: self, writer })
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

    /// Process an incoming array according to the current file mode.
    pub fn process_array_io(lock: &mut impl FileIo, array: Arc<NDArray>) -> ADResult<()> {
        let base = lock.base();
        let num_capture = base.num_capture;
        match base.mode {
            NDFileMode::Single => {
                let (write_path, final_path) = base.write_path();
                lock.io(|w| {
                    write_one_file(
                        w,
                        num_capture,
                        NDFileMode::Single,
                        &write_path,
                        final_path.as_deref(),
                        std::slice::from_ref(&array),
                    )
                })?;
                lock.base().maybe_delete_driver_file(&array);
            }
            NDFileMode::Capture => {
                base.capture_buffer.push(array);
                base.num_captured = base.capture_buffer.len();
                // B7: num_capture==0 → buffer forever, never auto-flush.
                if base.num_capture > 0 && base.num_captured >= base.num_capture {
                    Self::flush_capture_io(lock)?;
                }
            }
            NDFileMode::Stream => {
                Self::open_stream_io(lock, &array)?;
                lock.io(|w| {
                    w.set_num_capture(num_capture);
                    write_file_base(w, &array)
                })?;
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
    /// `capture_buffer` means one thing throughout: the frames that have not
    /// reached disk. A frame leaves it only once its own file is complete, so
    /// a flush that fails part way keeps exactly the frames still owed and a
    /// second `WriteFile` resumes at the first of them instead of starting
    /// over on files that are already written under numbers `file_number` has
    /// moved past.
    pub fn flush_capture_io(lock: &mut impl FileIo) -> ADResult<()> {
        if lock.base().capture_buffer.is_empty() {
            return Ok(());
        }
        let num_capture = lock.base().num_capture;
        if lock.io(|w| w.supports_multiple_arrays()) {
            // Multi-array format: open once, write all, close once. The frames
            // share one file, so they land together or not at all.
            let base = lock.base();
            let (write_path, final_path) = base.write_path();
            let frames = base.capture_buffer.clone();
            lock.io(|w| {
                write_one_file(
                    w,
                    num_capture,
                    NDFileMode::Capture,
                    &write_path,
                    final_path.as_deref(),
                    &frames,
                )
            })?;
            let base = lock.base();
            // C++ deletes the driver file per frame in Capture mode too.
            for arr in base.take_landed(&frames) {
                base.maybe_delete_driver_file(&arr);
            }
        } else {
            // Single-image format: open/write/close per frame with
            // auto-increment. The head of the buffer is the resume point: it
            // is dropped once its file is complete, so the `?` below leaves
            // the failing frame and everything behind it queued.
            while let Some(arr) = lock.base().capture_buffer.first().cloned() {
                let (write_path, final_path) = lock.base().write_path();
                lock.io(|w| {
                    write_one_file(
                        w,
                        num_capture,
                        NDFileMode::Single,
                        &write_path,
                        final_path.as_deref(),
                        std::slice::from_ref(&arr),
                    )
                })?;
                let base = lock.base();
                base.take_landed(std::slice::from_ref(&arr));
                base.maybe_delete_driver_file(&arr);
            }
        }
        Ok(())
    }

    /// Drop the frames of `written` that still head the capture buffer — the
    /// only place a frame leaves it, and the only place `num_captured` is set
    /// while frames are queued, so the readback cannot claim frames the buffer
    /// no longer holds or hide ones it still does. A capture restarted or
    /// freed while the file was written replaced the head; those frames stay.
    fn take_landed(&mut self, written: &[Arc<NDArray>]) -> Vec<Arc<NDArray>> {
        let n = self
            .capture_buffer
            .iter()
            .zip(written)
            .take_while(|(queued, done)| Arc::ptr_eq(queued, done))
            .count();
        let landed: Vec<Arc<NDArray>> = self.capture_buffer.drain(..n).collect();
        self.num_captured = self.capture_buffer.len();
        landed
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
        let (write_path, _) = base.write_path();
        base.is_open = true;
        let opened = lock.io(|w| {
            w.set_num_capture(num_capture);
            open_file_base(w, "openFileBase", &write_path, NDFileMode::Stream, array)
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

    pub fn is_open(&self) -> bool {
        self.is_open
    }

    pub fn set_mode(&mut self, mode: NDFileMode) {
        self.mode = mode;
    }

    pub fn set_num_capture(&mut self, n: usize) {
        self.num_capture = n;
    }

    pub fn num_captured(&self) -> usize {
        self.num_captured
    }

    pub fn mode(&self) -> NDFileMode {
        self.mode
    }

    pub fn num_capture_target(&self) -> usize {
        self.num_capture
    }

    pub fn capture_array(&mut self, array: Arc<NDArray>) {
        self.capture_buffer.push(array);
        self.num_captured = self.capture_buffer.len();
    }

    pub fn clear_capture(&mut self) {
        self.capture_buffer.clear();
        self.num_captured = 0;
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
                incremental: true,
            }
        }
    }

    impl NDFileWriter for MockWriter {
        fn open_file(&mut self, path: &Path, _mode: NDFileMode, _array: &NDArray) -> ADResult<()> {
            self.opens.push(path.to_path_buf());
            Ok(())
        }
        fn write_file(&mut self, _array: &Arc<NDArray>) -> ADResult<()> {
            if self.fail_write {
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
        fn open_file(&mut self, p: &Path, m: NDFileMode, a: &NDArray) -> ADResult<()> {
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

    #[test]
    fn test_delete_driver_file_in_capture_mode() {
        // C++ NDPluginFile deletes the driver file per frame in Capture mode
        // too, not only Single/Stream.
        let dir = std::env::temp_dir();
        let f1 = dir.join(format!(
            "adcore_capture_driver_1_{}.raw",
            std::process::id()
        ));
        let f2 = dir.join(format!(
            "adcore_capture_driver_2_{}.raw",
            std::process::id()
        ));
        std::fs::write(&f1, b"x").unwrap();
        std::fs::write(&f2, b"y").unwrap();

        let mut fb = NDPluginFileBase::new();
        fb.file_path = format!("{}/", dir.display());
        fb.file_name = "capdel_".into();
        fb.delete_driver_file = true;
        fb.set_mode(NDFileMode::Capture);
        fb.set_num_capture(2);

        let mut writer = MockWriter::new(true);
        fb.process_array(
            make_array_with_driver_file(1, f1.to_str().unwrap()),
            &mut writer,
        )
        .unwrap();
        fb.process_array(
            make_array_with_driver_file(2, f2.to_str().unwrap()),
            &mut writer,
        )
        .unwrap();

        // Capture buffer flushed at num_capture=2; both driver files deleted.
        assert!(
            !f1.exists(),
            "driver file 1 should be deleted in capture mode"
        );
        assert!(
            !f2.exists(),
            "driver file 2 should be deleted in capture mode"
        );
    }

    #[test]
    fn test_delete_driver_file_capture_single_image_format() {
        let dir = std::env::temp_dir();
        let f1 = dir.join(format!(
            "adcore_capture_si_driver_1_{}.raw",
            std::process::id()
        ));
        std::fs::write(&f1, b"x").unwrap();

        let mut fb = NDPluginFileBase::new();
        fb.file_path = format!("{}/", dir.display());
        fb.file_name = "capdelsi_".into();
        fb.delete_driver_file = true;
        fb.set_mode(NDFileMode::Capture);
        fb.set_num_capture(1);

        let mut writer = MockWriter::new(false); // single-image format
        fb.process_array(
            make_array_with_driver_file(1, f1.to_str().unwrap()),
            &mut writer,
        )
        .unwrap();

        assert!(
            !f1.exists(),
            "driver file should be deleted in capture mode"
        );
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

    /// D6: the same open/close pairing inside `flush_capture`.
    #[test]
    fn flush_capture_closes_the_file_when_a_frame_write_fails() {
        let (mut fb, mut writer) = partial_flush_fixture();
        writer.fail_write = true;
        assert!(fb.flush_capture(&mut writer).is_err());
        assert_eq!(
            writer.closes, 1,
            "the file opened for the failing frame is still closed"
        );
    }

    /// D6, the second resource the failing frame used to take with it: the
    /// buffer was moved out of `self` and only put back below the `?`, so a
    /// failed flush silently discarded every queued frame and the retry then
    /// reported success having written nothing.
    #[test]
    fn flush_capture_keeps_the_buffer_when_a_frame_write_fails() {
        let (mut fb, mut writer) = partial_flush_fixture();
        writer.fail_write = true;
        assert!(fb.flush_capture(&mut writer).is_err());

        writer.fail_write = false;
        fb.flush_capture(&mut writer).unwrap();
        assert_eq!(
            writer.writes, 2,
            "both buffered frames survive the failed flush and are written on retry"
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
