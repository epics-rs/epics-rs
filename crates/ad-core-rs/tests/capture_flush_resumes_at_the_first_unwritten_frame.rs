//! A capture flush may only be re-entered on frames that never reached disk.
//!
//! `NDPluginFileBase::flush_capture` writes one file per buffered frame in the
//! single-image branch and advances `file_number` between them. Nothing
//! recorded how far it had got, so a flush that failed on frame 2 left all
//! three frames queued with `file_number` already past the file it had
//! written, and the retry the code invited produced extra files holding
//! byte-identical copies of frames that were already saved.
//!
//! The boundaries are where the failure falls — first frame, middle frame,
//! last frame — plus a clean flush as the negative control, and the same three
//! against the multi-array branch so both branches answer to one rule: the
//! capture buffer holds exactly the frames that have not landed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ad_core_rs::error::{ADError, ADResult};
use ad_core_rs::ndarray::{NDArray, NDDataType, NDDimension};
use ad_core_rs::plugin::file_base::{NDFileMode, NDFileWriter, NDPluginFileBase};

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

    /// Distinct file names written, in first-write order.
    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for (name, _) in &self.landed {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    }
}

impl NDFileWriter for RecordingWriter {
    fn open_file(&mut self, path: &Path, _mode: NDFileMode, _array: &NDArray) -> ADResult<()> {
        self.current = Some(path.to_path_buf());
        Ok(())
    }

    fn write_file(&mut self, array: &NDArray) -> ADResult<()> {
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
    fb.file_number = 1;
    fb.auto_increment = true;
    fb.set_mode(NDFileMode::Capture);
    fb.set_num_capture(3);
    for id in 1..=3 {
        fb.capture_array(frame(id));
    }
    fb
}

/// What three frames must look like on disk once they are all saved, whatever
/// happened on the way: one file per frame, in order, and `file_number` three
/// past where it started.
fn assert_single_image_outcome(fb: &NDPluginFileBase, writer: &RecordingWriter) {
    assert_eq!(
        writer.landed,
        vec![
            ("img_0001".to_string(), 1),
            ("img_0002".to_string(), 2),
            ("img_0003".to_string(), 3),
        ],
        "three frames must produce three files, each written once"
    );
    assert_eq!(fb.file_number, 4, "one increment per file written");
    assert_eq!(fb.num_captured(), 0, "nothing is still owed");
}

/// The observable the regression produced: three captured frames, a failure on
/// frame 2, the operator frees the disk and issues WriteFile again — and the
/// files that were already saved are written a second time under the numbers
/// `file_number` had moved on to.
#[test]
fn a_retry_after_a_middle_failure_produces_one_file_per_frame() {
    let mut fb = captured_three();
    let mut writer = RecordingWriter::new(false);
    writer.fail_write_number = Some(2);

    assert!(fb.flush_capture(&mut writer).is_err());

    writer.fail_write_number = None;
    let _ = fb.flush_capture(&mut writer);

    assert_single_image_outcome(&fb, &writer);
}

/// Negative control.
#[test]
fn a_clean_single_image_flush_writes_one_file_per_frame() {
    let mut fb = captured_three();
    let mut writer = RecordingWriter::new(false);

    fb.flush_capture(&mut writer).expect("no writer failure");

    assert_single_image_outcome(&fb, &writer);
}

#[test]
fn a_failure_on_the_first_frame_leaves_all_three_queued() {
    let mut fb = captured_three();
    let mut writer = RecordingWriter::new(false);
    writer.fail_write_number = Some(1);

    assert!(fb.flush_capture(&mut writer).is_err());
    assert!(writer.landed.is_empty(), "nothing reached disk");
    assert_eq!(fb.num_captured(), 3, "all three frames are still owed");
    assert_eq!(fb.file_number, 1, "no file was written, no number consumed");

    writer.fail_write_number = None;
    fb.flush_capture(&mut writer)
        .expect("the disk is free again");
    assert_single_image_outcome(&fb, &writer);
}

#[test]
fn a_failure_on_a_middle_frame_leaves_the_unwritten_tail_queued() {
    let mut fb = captured_three();
    let mut writer = RecordingWriter::new(false);
    writer.fail_write_number = Some(2);

    assert!(fb.flush_capture(&mut writer).is_err());
    assert_eq!(writer.files(), vec!["img_0001"], "only frame 1 landed");
    assert_eq!(fb.num_captured(), 2, "frames 2 and 3 are still owed");
    assert_eq!(fb.file_number, 2);

    writer.fail_write_number = None;
    fb.flush_capture(&mut writer)
        .expect("the disk is free again");
    assert_single_image_outcome(&fb, &writer);
}

#[test]
fn a_failure_on_the_last_frame_leaves_only_that_frame_queued() {
    let mut fb = captured_three();
    let mut writer = RecordingWriter::new(false);
    writer.fail_write_number = Some(3);

    assert!(fb.flush_capture(&mut writer).is_err());
    assert_eq!(
        writer.files(),
        vec!["img_0001", "img_0002"],
        "frames 1 and 2 landed"
    );
    assert_eq!(fb.num_captured(), 1, "frame 3 is still owed");
    assert_eq!(fb.file_number, 3);

    writer.fail_write_number = None;
    fb.flush_capture(&mut writer)
        .expect("the disk is free again");
    assert_single_image_outcome(&fb, &writer);
}

/// The multi-array branch under the same rule: the frames share one file, so
/// they land together or stay queued together, and the retry reuses the file
/// number rather than consuming a second one.
fn multi_array_flush_failing_on_write(fail_write_number: usize) {
    let mut fb = captured_three();
    let mut writer = RecordingWriter::new(true);
    writer.fail_write_number = Some(fail_write_number);

    assert!(fb.flush_capture(&mut writer).is_err());
    assert_eq!(
        fb.num_captured(),
        3,
        "the file never completed, so every frame is still owed"
    );
    assert_eq!(fb.file_number, 1, "no completed file, no number consumed");

    writer.fail_write_number = None;
    writer.landed.clear();
    fb.flush_capture(&mut writer)
        .expect("the disk is free again");

    assert_eq!(
        writer.landed,
        vec![
            ("img_0001".to_string(), 1),
            ("img_0001".to_string(), 2),
            ("img_0001".to_string(), 3),
        ],
        "all three frames in the one file the failed attempt had claimed"
    );
    assert_eq!(fb.file_number, 2, "one completed file, one increment");
    assert_eq!(fb.num_captured(), 0);
}

#[test]
fn a_multi_array_failure_on_the_first_frame_keeps_the_whole_buffer() {
    multi_array_flush_failing_on_write(1);
}

#[test]
fn a_multi_array_failure_on_a_middle_frame_keeps_the_whole_buffer() {
    multi_array_flush_failing_on_write(2);
}

#[test]
fn a_multi_array_failure_on_the_last_frame_keeps_the_whole_buffer() {
    multi_array_flush_failing_on_write(3);
}

#[test]
fn a_clean_multi_array_flush_writes_every_frame_into_one_file() {
    let mut fb = captured_three();
    let mut writer = RecordingWriter::new(true);

    fb.flush_capture(&mut writer).expect("no writer failure");

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
