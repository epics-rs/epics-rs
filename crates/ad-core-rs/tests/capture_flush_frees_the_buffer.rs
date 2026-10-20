//! A capture flush frees the buffer however it ends, as C does
//! (NDPluginFile.cpp:325, `freeCaptureBuffer`).
//!
//! The single-image branch tries every frame, each under its own name, and a
//! failed frame is lost (:288-322). The multi-array branch writes nothing past
//! the failing frame (:295). The boundaries are where the failure falls —
//! first frame, middle frame, last frame — plus a clean flush as the negative
//! control.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ad_core_rs::error::{ADError, ADResult};
use ad_core_rs::ndarray::{NDArray, NDDataType, NDDimension};
use ad_core_rs::plugin::file_base::{NDFileMode, NDFileOpenMode, NDFileWriter, NDPluginFileBase};

/// Records which frame landed in which file, and fails a chosen write.
struct RecordingWriter {
    multi: bool,
    /// `(file name, frame unique_id)` for every write that succeeded.
    landed: Vec<(String, i32)>,
    current: Option<PathBuf>,
    /// 1-based index of the `write_file` call that returns Err, if any.
    fail_write_number: Option<usize>,
    writes_seen: usize,
}

impl RecordingWriter {
    fn new(multi: bool) -> Self {
        Self {
            multi,
            landed: Vec::new(),
            current: None,
            fail_write_number: None,
            writes_seen: 0,
        }
    }
}

impl NDFileWriter for RecordingWriter {
    fn open_file(&mut self, path: &Path, _mode: NDFileOpenMode, _array: &NDArray) -> ADResult<()> {
        self.current = Some(path.to_path_buf());
        Ok(())
    }

    fn write_file(&mut self, array: &Arc<NDArray>) -> ADResult<()> {
        self.writes_seen += 1;
        if self.fail_write_number == Some(self.writes_seen) {
            return Err(ADError::UnsupportedConversion("disk full".into()));
        }
        let path = self.current.clone().expect("write without open");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        self.landed.push((name, array.unique_id));
        Ok(())
    }

    fn read_file(&mut self) -> ADResult<NDArray> {
        Err(ADError::UnsupportedConversion("not implemented".into()))
    }

    fn close_file(&mut self) -> ADResult<()> {
        self.current = None;
        Ok(())
    }

    fn supports_multiple_arrays(&self) -> bool {
        self.multi
    }
}

fn frame(id: i32) -> Arc<NDArray> {
    let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
    arr.unique_id = id;
    Arc::new(arr)
}

/// FileWriteMode Capture, AutoIncrement 1, FileNumber 1, NumCapture 3, three
/// frames captured and not yet flushed.
fn captured_three() -> NDPluginFileBase {
    let mut fb = NDPluginFileBase::new();
    fb.file_path = "/tmp/".into();
    fb.file_name = "img_".into();
    fb.file_template = "%s%s%4.4d".into();
    fb.file_number = 1;
    fb.auto_increment = true;
    fb.set_mode(NDFileMode::Capture);
    fb.set_num_capture(3);
    for id in 1..=3 {
        fb.capture_array(frame(id));
    }
    fb
}

/// The single-image branch: every frame but the failing one is in its own
/// file, under the number its name consumed (asynNDArrayDriver.cpp:216-218),
/// and the buffer is freed, so a retry has nothing to write. The flush ends
/// on the last frame's outcome (NDPluginFile.cpp:288-322).
fn single_image_flush_failing_on_write(fail_write_number: usize) {
    let mut fb = captured_three();
    let mut writer = RecordingWriter::new(false);
    writer.fail_write_number = Some(fail_write_number);

    let flushed = fb.flush_capture(&mut writer, &frame(3));
    if fail_write_number == 3 {
        assert_eq!(
            flushed.unwrap_err().to_string(),
            "Error writing file, status=3"
        );
    } else {
        flushed.expect("the last frame landed");
    }
    let landed: Vec<(String, i32)> = (1..=3)
        .filter(|&id| id != fail_write_number as i32)
        .map(|id| (format!("img_{id:04}"), id))
        .collect();
    assert_eq!(writer.landed, landed);
    assert_eq!(writer.writes_seen, 3, "C writes past the failure");
    assert_eq!(fb.num_captured(), 0, "C frees the buffer");
    assert_eq!(fb.file_number, 4, "every frame's name consumed one");

    let retry = fb.flush_capture(&mut writer, &frame(3)).unwrap_err();
    assert_eq!(retry.to_string(), "ERROR, no capture buffer present");
    assert_eq!(writer.landed, landed, "nothing left to retry");
}

#[test]
fn a_single_image_failure_on_the_first_frame_frees_the_buffer() {
    single_image_flush_failing_on_write(1);
}

#[test]
fn a_single_image_failure_on_a_middle_frame_frees_the_buffer() {
    single_image_flush_failing_on_write(2);
}

#[test]
fn a_single_image_failure_on_the_last_frame_frees_the_buffer() {
    single_image_flush_failing_on_write(3);
}

/// Negative control.
#[test]
fn a_clean_single_image_flush_writes_one_file_per_frame() {
    let mut fb = captured_three();
    let mut writer = RecordingWriter::new(false);

    fb.flush_capture(&mut writer, &frame(3))
        .expect("no writer failure");

    let landed: Vec<(String, i32)> = (1..=3).map(|id| (format!("img_{id:04}"), id)).collect();
    assert_eq!(writer.landed, landed);
    assert_eq!(fb.file_number, 4);
    assert_eq!(fb.num_captured(), 0);
}

/// The multi-array branch: the frames before the failing one are in the
/// file, none after it is written, and the buffer is freed, so a retry has
/// nothing to write.
fn multi_array_flush_failing_on_write(fail_write_number: usize) {
    let mut fb = captured_three();
    let mut writer = RecordingWriter::new(true);
    writer.fail_write_number = Some(fail_write_number);

    assert!(fb.flush_capture(&mut writer, &frame(3)).is_err());
    let before: Vec<(String, i32)> = (1..fail_write_number as i32)
        .map(|id| ("img_0001".to_string(), id))
        .collect();
    assert_eq!(writer.landed, before);
    assert_eq!(
        writer.writes_seen, fail_write_number,
        "no write past the failure"
    );
    assert_eq!(fb.num_captured(), 0, "C frees the buffer");
    assert_eq!(fb.file_number, 2, "the open's name consumed 1");

    let retry = fb.flush_capture(&mut writer, &frame(3)).unwrap_err();
    assert_eq!(retry.to_string(), "ERROR, no capture buffer present");
    assert_eq!(writer.landed, before, "nothing left to retry");
}

#[test]
fn a_multi_array_failure_on_the_first_frame_frees_the_buffer() {
    multi_array_flush_failing_on_write(1);
}

#[test]
fn a_multi_array_failure_on_a_middle_frame_frees_the_buffer() {
    multi_array_flush_failing_on_write(2);
}

#[test]
fn a_multi_array_failure_on_the_last_frame_frees_the_buffer() {
    multi_array_flush_failing_on_write(3);
}

#[test]
fn a_clean_multi_array_flush_writes_every_frame_into_one_file() {
    let mut fb = captured_three();
    let mut writer = RecordingWriter::new(true);

    fb.flush_capture(&mut writer, &frame(3))
        .expect("no writer failure");

    assert_eq!(
        writer.landed,
        vec![
            ("img_0001".to_string(), 1),
            ("img_0001".to_string(), 2),
            ("img_0001".to_string(), 3),
        ]
    );
    assert_eq!(fb.file_number, 2);
    assert_eq!(fb.num_captured(), 0);
}
