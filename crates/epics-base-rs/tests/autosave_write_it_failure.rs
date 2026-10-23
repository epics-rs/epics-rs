//! C `write_it`'s steps on a full disk (save_restore.c:1800-1981): an
//! `fprintf` whose buffer flush fails gives up, naming the set's own file;
//! a failed `fflush` or `fsync` is only printed, and the file-check after
//! the close fails the write. The expected lines are the C oracle IOC's for
//! a monitor set whose `s.sav` is a symlink to `/dev/full`; rs writes
//! through `s.tmp`, so that is the symlink here.

// `/dev/full` is Linux's.
#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const CHILD: &str = "AUTOSAVE_WRITE_IT_FAILURE_DIR";
const RECORDS: &str = "AUTOSAVE_WRITE_IT_FAILURE_RECORDS";
const ELEMENTS: &str = "AUTOSAVE_WRITE_IT_FAILURE_ELEMENTS";

/// One manual save into `$CHILD/s.sav` of a DOUBLE waveform `W` of
/// `ELEMENTS` elements when set, then `RECORDS` ao records, in the child
/// only.
#[epics_macros_rs::epics_test]
async fn child_save() {
    let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) else {
        return;
    };
    let n: usize = std::env::var(RECORDS).unwrap().parse().unwrap();
    let db = Arc::new(PvDatabase::new());
    let mut names: Vec<String> = (0..n)
        .map(|i| format!("PADDING_RECORD_NAME_{i:04}"))
        .collect();
    for name in &names {
        db.add_record(name, Box::new(AoRecord::new(1.2345678901234e-100)))
            .await
            .unwrap();
    }
    if let Ok(elements) = std::env::var(ELEMENTS) {
        let elements: usize = elements.parse().unwrap();
        let mut wf = WaveformRecord::new(elements as i32, DbFieldType::Double);
        wf.put_field(
            "VAL",
            EpicsValue::DoubleArray(vec![1.2345678901234e-100; elements].into()),
        )
        .unwrap();
        db.add_record("W", Box::new(wf)).await.unwrap();
        names.insert(0, "W".into());
    }
    let mgr = AutosaveBuilder::new()
        .add_set(
            SaveSetConfig {
                name: "s".into(),
                save_path: dir.join("s.sav"),
                request_file: None,
                request_pvs: names,
                backup: BackupConfig { enable_savb: true },
                macros: HashMap::new(),
                macro_string: None,
                search_paths: Vec::new(),
            },
            SaveStrategy::Manual,
        )
        .build()
        .await;
    let _ = mgr.manual_save("s", &db).await;
    epics_base_rs::runtime::log::errlog_flush();
}

/// The child's stdout lines from autosave and its errlog lines, each
/// `[yymmdd-HHMMSS]` written `[DATE]`.
fn run_child(dir: &Path, records: usize) -> (Vec<String>, Vec<String>) {
    run_child_with(dir, records, None)
}

fn run_child_with(
    dir: &Path,
    records: usize,
    elements: Option<usize>,
) -> (Vec<String>, Vec<String>) {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", "child_save", "--nocapture", "--test-threads=1"])
        .env(CHILD, dir)
        .env(RECORDS, records.to_string());
    if let Some(elements) = elements {
        child.env(ELEMENTS, elements.to_string());
    }
    let out = child.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let lines = |bytes: &[u8], keep: fn(&str) -> bool| -> Vec<String> {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(|l| l.strip_prefix("test child_save ... ").unwrap_or(l))
            .filter(|l| keep(l))
            .map(stamp)
            .collect()
    };
    (
        lines(&out.stdout, |l| {
            l.contains("save_restore") || l.starts_with("***")
        }),
        lines(&out.stderr, |l| l.contains("save_restore.c")),
    )
}

fn stamp(line: &str) -> String {
    let mut out = line.to_string();
    let bytes = line.as_bytes();
    for at in (0..bytes.len().saturating_sub(14)).rev() {
        let s = &bytes[at..at + 15];
        let digits = |r: std::ops::Range<usize>| s[r].iter().all(u8::is_ascii_digit);
        if s[0] == b'[' && digits(1..7) && s[7] == b'-' && digits(8..14) && s[14] == b']' {
            out.replace_range(at..at + 15, "[DATE]");
        }
    }
    out
}

/// A directory whose `s.tmp` is `/dev/full`, with a good `s.savB` when
/// `savb`.
fn full_disk(savb: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    if savb {
        std::fs::write(dir.path().join("s.savB"), "# x\n<END>\n").unwrap();
    }
    std::os::unix::fs::symlink("/dev/full", dir.path().join("s.tmp")).unwrap();
    dir
}

fn banner(line: &str) -> [String; 3] {
    let stars = "*** ".repeat(16) + "***";
    [stars.clone(), line.to_string(), stars]
}

#[test]
fn a_failed_fflush_is_printed_and_the_file_check_fails() {
    let dir = full_disk(true);
    let (stdout, errlog) = run_child(dir.path(), 3);
    let mut expected = vec!["save_restore:write_it: fflush returned -1 [DATE]".to_string()];
    expected.extend(banner(
        "save_restore:write_save_file: Can't write save file. [DATE]",
    ));
    assert_eq!(stdout, expected);
    assert_eq!(
        errlog,
        [
            "../save_restore.c(1899): [0x1c]=write_it:No space left on device",
            "../save_restore.c:write_it fsync returned -1 [DATE]",
            "../save_restore.c(1925): [0x16]=write_it:Invalid argument",
            "../save_restore.c:write_it file-check failure [DATE], check_file=1",
        ]
    );
    assert!(!dir.path().join("s.sav").exists());
}

#[test]
fn a_failed_fprintf_gives_up() {
    let dir = full_disk(true);
    let (stdout, errlog) = run_child(dir.path(), 295);
    let mut expected = vec!["save_restore:write_it: fprintf returned -1 [DATE].".to_string()];
    expected.extend(banner(
        "save_restore:write_save_file: Can't write save file. [DATE]",
    ));
    assert_eq!(stdout, expected);
    assert_eq!(
        errlog,
        [
            "../save_restore.c(1870): [0x1c]=write_it:No space left on device",
            "../save_restore.c:write_it Giving up on this attempt to write 's.sav'. [DATE]",
        ]
    );
}

/// C names `plist->save_file` however `write_it` was reached
/// (save_restore.c:1981), here for the `.savB` it writes first.
#[test]
fn giving_up_on_the_backup_names_the_save_file() {
    let dir = full_disk(false);
    let (_, errlog) = run_child(dir.path(), 295);
    assert_eq!(
        errlog,
        [
            "../save_restore.c(1870): [0x1c]=write_it:No space left on device",
            "../save_restore.c:write_it Giving up on this attempt to write 's.sav'. [DATE]",
        ]
    );
}

/// The oracle's 1000-element DOUBLE waveform and one ao: the array's
/// failed flushes only add -1 to `SR_write_array_data`'s sum, so the
/// write goes on to the `fflush` (dbrestore.c:1385-1448).
#[test]
fn a_failed_flush_inside_an_array_goes_on_to_the_fflush() {
    let dir = full_disk(true);
    let (stdout, errlog) = run_child_with(dir.path(), 1, Some(1000));
    let mut expected = vec!["save_restore:write_it: fflush returned -1 [DATE]".to_string()];
    expected.extend(banner(
        "save_restore:write_save_file: Can't write save file. [DATE]",
    ));
    assert_eq!(stdout, expected);
    assert_eq!(
        errlog,
        [
            "../save_restore.c(1899): [0x1c]=write_it:No space left on device",
            "../save_restore.c:write_it fsync returned -1 [DATE]",
            "../save_restore.c(1925): [0x16]=write_it:Invalid argument",
            "../save_restore.c:write_it file-check failure [DATE], check_file=1",
        ]
    );
}
