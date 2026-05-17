use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::ADResult;
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
    fn write_file(&mut self, array: &NDArray) -> ADResult<()>;
    fn read_file(&mut self) -> ADResult<NDArray>;
    fn close_file(&mut self) -> ADResult<()>;
    fn supports_multiple_arrays(&self) -> bool {
        true
    }
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
    last_written_name: String,
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
            last_written_name: String::new(),
        }
    }

    /// Construct the full file path from template/path/name/number.
    ///
    /// Mimics C `epicsSnprintf(buf, ..., template, filePath, fileName, fileNumber)`.
    /// Template uses printf-style: first `%s` → filePath, second `%s` → fileName,
    /// `%d` (with optional width/precision like `%3.3d`) → fileNumber.
    pub fn create_file_name(&self) -> String {
        if self.file_template.is_empty() {
            format!(
                "{}{}{:04}",
                self.file_path, self.file_name, self.file_number
            )
        } else {
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

    /// Return the full file name that was last written.
    pub fn last_written_name(&self) -> &str {
        &self.last_written_name
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

    /// Write to temp path if temp_suffix is set, then rename to final path.
    fn write_path(&self) -> (PathBuf, Option<PathBuf>) {
        let final_path = PathBuf::from(self.create_file_name());
        if self.temp_suffix.is_empty() {
            (final_path, None)
        } else {
            let temp = PathBuf::from(format!("{}{}", final_path.display(), self.temp_suffix));
            (temp, Some(final_path))
        }
    }

    /// Rename temp file to final path if applicable.
    fn rename_temp(temp_path: &Path, final_path: &Path) -> ADResult<()> {
        std::fs::rename(temp_path, final_path)?;
        Ok(())
    }

    /// Delete the driver's original file for `array` when `delete_driver_file`
    /// is set. C++ NDPluginFile deletes the driver file in every write mode
    /// (Single, Stream, and Capture), keyed off the `DriverFileName` attribute.
    fn maybe_delete_driver_file(&self, array: &NDArray) {
        if !self.delete_driver_file {
            return;
        }
        if let Some(attr) = array.attributes.get("DriverFileName") {
            let driver_file = attr.value.as_string();
            if !driver_file.is_empty() {
                let _ = std::fs::remove_file(&driver_file);
            }
        }
    }

    /// Process an incoming array according to the current file mode.
    pub fn process_array(
        &mut self,
        array: Arc<NDArray>,
        writer: &mut dyn NDFileWriter,
    ) -> ADResult<()> {
        match self.mode {
            NDFileMode::Single => {
                self.last_written_name = self.create_file_name();
                let (write_path, final_path) = self.write_path();
                writer.open_file(&write_path, NDFileMode::Single, &array)?;
                writer.write_file(&array)?;
                writer.close_file()?;
                if let Some(final_path) = final_path {
                    Self::rename_temp(&write_path, &final_path)?;
                }
                self.maybe_delete_driver_file(&array);
                if self.auto_increment {
                    self.file_number += 1;
                }
            }
            NDFileMode::Capture => {
                self.capture_buffer.push(array);
                self.num_captured = self.capture_buffer.len();
                // B7: num_capture==0 → buffer forever, never auto-flush.
                if self.num_capture > 0 && self.num_captured >= self.num_capture {
                    self.flush_capture(writer)?;
                }
            }
            NDFileMode::Stream => {
                if !self.is_open && !self.lazy_open {
                    self.last_written_name = self.create_file_name();
                    let (write_path, _) = self.write_path();
                    writer.open_file(&write_path, NDFileMode::Stream, &array)?;
                    self.is_open = true;
                }
                if self.lazy_open && !self.is_open {
                    self.last_written_name = self.create_file_name();
                    let (write_path, _) = self.write_path();
                    writer.open_file(&write_path, NDFileMode::Stream, &array)?;
                    self.is_open = true;
                }
                writer.write_file(&array)?;
                self.maybe_delete_driver_file(&array);
                self.num_captured += 1;
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
    pub fn flush_capture(&mut self, writer: &mut dyn NDFileWriter) -> ADResult<()> {
        if self.capture_buffer.is_empty() {
            return Ok(());
        }

        if writer.supports_multiple_arrays() {
            // Multi-array format: open once, write all, close once.
            self.last_written_name = self.create_file_name();
            let (write_path, final_path) = self.write_path();
            writer.open_file(&write_path, NDFileMode::Capture, &self.capture_buffer[0])?;
            for arr in &self.capture_buffer {
                writer.write_file(arr)?;
            }
            writer.close_file()?;
            if let Some(final_path) = final_path {
                Self::rename_temp(&write_path, &final_path)?;
            }
            // C++ deletes the driver file per frame in Capture mode too.
            let buffer = std::mem::take(&mut self.capture_buffer);
            for arr in &buffer {
                self.maybe_delete_driver_file(arr);
            }
            self.capture_buffer = buffer;
            if self.auto_increment {
                self.file_number += 1;
            }
        } else {
            // Single-image format: open/write/close per frame with auto-increment.
            let buffer = std::mem::take(&mut self.capture_buffer);
            for arr in &buffer {
                self.last_written_name = self.create_file_name();
                let (write_path, final_path) = self.write_path();
                writer.open_file(&write_path, NDFileMode::Single, arr)?;
                writer.write_file(arr)?;
                writer.close_file()?;
                if let Some(final_path) = final_path {
                    Self::rename_temp(&write_path, &final_path)?;
                }
                self.maybe_delete_driver_file(arr);
                if self.auto_increment {
                    self.file_number += 1;
                }
            }
            self.capture_buffer = buffer;
        }

        self.capture_buffer.clear();
        self.num_captured = 0;
        Ok(())
    }

    /// Eagerly open a stream file before the first frame arrives.
    ///
    /// C++ `doCapture` opens the file at capture-start for non-lazy stream
    /// plugins (NDPluginFile.cpp:478-479) so a bad path is reported then,
    /// not on the first frame (B9). `array` supplies the layout the writer
    /// needs at open time.
    pub fn open_stream_eager(
        &mut self,
        writer: &mut dyn NDFileWriter,
        array: &NDArray,
    ) -> ADResult<()> {
        if self.is_open {
            return Ok(());
        }
        self.last_written_name = self.create_file_name();
        let (write_path, _) = self.write_path();
        writer.open_file(&write_path, NDFileMode::Stream, array)?;
        self.is_open = true;
        Ok(())
    }

    /// Force a file close (used by the FilePluginClose attribute, G9).
    /// Safe to call when no file is open.
    pub fn force_close(&mut self, writer: &mut dyn NDFileWriter) -> ADResult<()> {
        if self.is_open {
            self.close_stream(writer)?;
        }
        Ok(())
    }

    /// Close stream mode.
    pub fn close_stream(&mut self, writer: &mut dyn NDFileWriter) -> ADResult<()> {
        if self.is_open {
            writer.close_file()?;
            // Rename temp to final if temp_suffix was set
            if !self.temp_suffix.is_empty() {
                let final_name = self.create_file_name();
                let temp_name = format!("{}{}", final_name, self.temp_suffix);
                Self::rename_temp(Path::new(&temp_name), Path::new(&final_name))?;
            }
            self.is_open = false;
            if self.auto_increment {
                self.file_number += 1;
            }
        }
        Ok(())
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
    }

    impl MockWriter {
        fn new(multi: bool) -> Self {
            Self {
                opens: Vec::new(),
                writes: 0,
                closes: 0,
                multi,
            }
        }
    }

    impl NDFileWriter for MockWriter {
        fn open_file(&mut self, path: &Path, _mode: NDFileMode, _array: &NDArray) -> ADResult<()> {
            self.opens.push(path.to_path_buf());
            Ok(())
        }
        fn write_file(&mut self, _array: &NDArray) -> ADResult<()> {
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
            Ok(())
        }
        fn supports_multiple_arrays(&self) -> bool {
            self.multi
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
    fn test_create_file_name_default() {
        let mut fb = NDPluginFileBase::new();
        fb.file_path = "/data/".into();
        fb.file_name = "img_".into();
        fb.file_number = 42;
        assert_eq!(fb.create_file_name(), "/data/img_0042");
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
        fb.file_number = 1;
        fb.temp_suffix = ".tmp".into();

        let temp = fb.temp_file_path().unwrap();
        assert_eq!(temp.to_str().unwrap(), "/data/img_0001.tmp");
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
        let f1 = dir.join("adcore_capture_driver_1.raw");
        let f2 = dir.join("adcore_capture_driver_2.raw");
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
        let f1 = dir.join("adcore_capture_si_driver_1.raw");
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
}
