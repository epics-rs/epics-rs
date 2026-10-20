//! C's save cycle — `write_save_file` (save_restore.c:2003-2135), `do_seq`
//! (:2145-2216) and `doPeriodicDatedBackup` (:2218-2262) — says on the
//! console what went wrong with a save, with `fGetDateStr`'s
//! `[yymmdd-HHMMSS]` stamp:
//!
//! ```text
//! save_restore:write_save_file: Backup file (/dir/set.savB) bad or not found.  Writing a new one. [261018-101500]
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;

const CHILD: &str = "AUTOSAVE_SAVE_CONSOLE_DIR";
/// Which backups the child's set keeps: `savb`, `seq` or `dated`.
const BACKUPS: &str = "AUTOSAVE_SAVE_CONSOLE_BACKUPS";

const GOOD: &str = "# autosave-rs V1.0\nTEMP 1.0\n<END>\n";

/// Run the `child_*` test `name` in a fresh process on `dir` and return its
/// stdout's autosave lines, with each `[yymmdd-HHMMSS]` stamp checked and
/// replaced by `[DATE]`.
fn console_of(name: &str, dir: &Path, backups: &str) -> Vec<String> {
    run_child(name, dir, backups).0
}

/// The child's stdout lines as [`console_of`] reads them, and its errlog
/// (stderr) lines that name an autosave source file.
fn run_child(name: &str, dir: &Path, backups: &str) -> (Vec<String>, Vec<String>) {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, dir)
        .env(BACKUPS, backups)
        .output()
        .unwrap();
    assert!(out.status.success(), "{name}: {out:?}");
    let prefix = format!("test {name} ... ");
    let dir = dir.display().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.strip_prefix(prefix.as_str()).unwrap_or(l))
        .filter(|l| l.contains("save_restore") || l.starts_with("***"))
        .map(|l| stamp(&l.replace(&dir, "DIR")))
        .collect();
    let errlog = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.contains("save_restore.c") || l.contains("dbrestore.c"))
        .map(|l| stamp(&l.replace(&dir, "DIR")))
        .collect();
    (stdout, errlog)
}

fn stamp(line: &str) -> String {
    let digits = |s: &str| s.len() == 6 && s.bytes().all(|b| b.is_ascii_digit());
    let date = line
        .strip_suffix(']')
        .and_then(|head| head.rsplit_once('['))
        .filter(|(_, date)| {
            date.split_once('-')
                .is_some_and(|(d, t)| digits(d) && digits(t))
        });
    match date {
        Some((head, _)) => format!("{head}[DATE]"),
        None => line.to_string(),
    }
}

/// One manual save of `TEMP` into `$CHILD/set.sav`, in the child only.
async fn save_once() -> Option<()> {
    let dir = PathBuf::from(std::env::var_os(CHILD)?);
    let backups = std::env::var(BACKUPS).unwrap();
    // `seq`: a save, then the task's `do_seq`; `seq-only`: `do_seq` alone.
    let seq = backups.starts_with("seq");
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEMP", Box::new(AoRecord::new(25.5)))
        .await
        .unwrap();
    let mgr = AutosaveBuilder::new()
        .add_set(
            SaveSetConfig {
                name: "set".into(),
                save_path: dir.join("set.sav"),
                request_file: None,
                request_pvs: vec!["TEMP".into()],
                backup: BackupConfig {
                    enable_savb: backups == "savb",
                },
                macros: HashMap::new(),
                macro_string: None,
                search_paths: Vec::new(),
            },
            SaveStrategy::Manual,
        )
        .build()
        .await;
    if backups == "dated" {
        mgr.sets()[0].0.do_periodic_dated_backup().await;
    } else if backups != "seq-only" {
        let _ = mgr.manual_save("set", &db).await;
    }
    if seq {
        mgr.sets()[0].0.do_seq(&db).await;
    }
    epics_base_rs::runtime::log::errlog_flush();
    Some(())
}

#[epics_macros_rs::epics_test]
async fn child_save() {
    save_once().await;
}

const BAD_BACKUP: &str = "save_restore:write_save_file: Backup file (DIR/set.savB) bad or not \
                          found.  Writing a new one. [DATE]";

#[test]
fn a_bad_backup_is_cs_line() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("set.sav"), GOOD).unwrap();
    std::fs::write(dir.path().join("set.savB"), "TEMP 1.0\n").unwrap();
    assert_eq!(console_of("child_save", dir.path(), "savb"), [BAD_BACKUP]);
    let kept = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| {
            let name = e.as_ref().unwrap().file_name();
            name.to_str().unwrap().starts_with("set.savB_SBAD_")
        })
        .count();
    assert_eq!(kept, 1, "the corrupt backup must be kept for diagnosis");
}

#[test]
fn an_unwritable_backup_is_cs_banner() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("set.sav"), GOOD).unwrap();
    // No `.savB`, and the temp file it is written through is a non-empty
    // directory.
    let tmp = dir.path().join("set.tmp");
    std::fs::create_dir(&tmp).unwrap();
    std::fs::write(tmp.join("x"), "").unwrap();
    let stars = "*** ".repeat(18) + "***";
    assert_eq!(
        console_of("child_save", dir.path(), "savb"),
        [
            BAD_BACKUP,
            "save_restore:write_it - unable to open file 'DIR/set.savB' [DATE]",
            &stars,
            "save_restore:write_save_file: Can't write new backup file. [DATE]",
            &stars,
        ]
    );
}

#[test]
fn an_unwritable_save_file_is_cs_banner() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("set.sav")).unwrap();
    let stars = "*** ".repeat(16) + "***";
    let (stdout, errlog) = run_child("child_save", dir.path(), "savb");
    assert_eq!(
        stdout,
        [
            BAD_BACKUP,
            "save_restore:write_it - unable to open file 'DIR/set.sav' [DATE]",
            &stars,
            "save_restore:write_save_file: Can't write save file. [DATE]",
            &stars,
        ]
    );
    assert_eq!(
        errlog,
        ["../save_restore.c(1774): [0x15]=write_it:Is a directory"]
    );
}

#[test]
fn a_failed_backup_copy_is_cs_line() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("set.sav"), GOOD).unwrap();
    std::fs::write(dir.path().join("set.savB"), GOOD).unwrap();
    // `.savB` is published through `set.savB.tmp`; a non-empty directory
    // there makes the copy fail.
    let tmp = dir.path().join("set.savB.tmp");
    std::fs::create_dir(&tmp).unwrap();
    std::fs::write(tmp.join("x"), "").unwrap();
    let lines = console_of("child_save", dir.path(), "savb");
    assert!(
        lines.contains(
            &"save_restore:write_save_file - Couldn't make backup 'DIR/set.savB' [DATE]".into()
        ),
        "{lines:?}"
    );
}

const SEQ_COPY: &str = "save_restore:do_seq - Can't copy save file to 'DIR/set.sav0' [DATE]";

/// The copy fails and the slot is written from the PVs (save_restore.c:2193-2203).
#[test]
fn a_failed_sequence_copy_is_cs_line() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().join("set.sav0.tmp");
    std::fs::create_dir(&tmp).unwrap();
    std::fs::write(tmp.join("x"), "").unwrap();
    assert_eq!(
        console_of("child_save", dir.path(), "seq"),
        [
            SEQ_COPY,
            "save_restore:do_seq: Wrote seq. file from PV list. [DATE]"
        ]
    );
}

/// Neither the copy nor the write lands (save_restore.c:2196).
#[test]
fn an_unwritable_sequence_file_is_cs_line() {
    let dir = tempfile::tempdir().unwrap();
    // Every slot taken, `.sav0` the oldest.
    for i in 0..3 {
        let slot = dir.path().join(format!("set.sav{i}"));
        std::fs::create_dir(&slot).unwrap();
        std::fs::write(slot.join("x"), "").unwrap();
    }
    let lines = console_of("child_save", dir.path(), "seq");
    assert_eq!(lines[0], SEQ_COPY, "{lines:?}");
    assert_eq!(
        lines.last().unwrap(),
        "save_restore:do_seq - Can't write seq. file from PV list. [DATE]",
        "{lines:?}"
    );
}

/// No `.sav` to copy: C writes one first (save_restore.c:2183-2190).
#[test]
fn a_missing_sav_is_written_for_the_sequence_file() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        console_of("child_save", dir.path(), "seq-only"),
        ["save_restore:do_seq - 'DIR/set.sav' not found.  Writing a new one. [DATE]"]
    );
    assert!(dir.path().join("set.sav0").exists());
}

#[cfg(unix)]
#[test]
fn a_failed_dated_backup_is_cs_banner() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("set.sav"), GOOD).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(dir.path().join("probe"), "").is_ok() {
        // Permissions do not bind this user (root).
        return;
    }
    let lines = console_of("child_save", dir.path(), "dated");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let stars = "*** ".repeat(16) + "***";
    assert_eq!(lines.len(), 4, "{lines:?}");
    // write_it's own line, then doPeriodicDatedBackup's (save_restore.c:1773,
    // :2253-2255), both naming `<save_file>_b_<date>`.
    let name = lines[0]
        .strip_prefix("save_restore:write_it - unable to open file 'DIR/set.sav_b_")
        .and_then(|rest| rest.strip_suffix("' [DATE]"))
        .unwrap_or_else(|| panic!("{lines:?}"));
    assert_eq!(lines[1], stars);
    assert_eq!(
        lines[2],
        format!("save_restore:doPeriodicDatedBackup: Can't write file. [DIR/set.sav_b_{name}]")
    );
    assert_eq!(lines[3], stars);
}

#[cfg(unix)]
#[test]
fn a_failed_bad_backup_copy_is_cs_line() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("set.sav"), GOOD).unwrap();
    std::fs::write(dir.path().join("set.savB"), "TEMP 1.0\n").unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(dir.path().join("probe"), "").is_ok() {
        // Permissions do not bind this user (root).
        return;
    }
    let lines = console_of("child_save", dir.path(), "savb");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(lines.len(), 6, "{lines:?}");
    assert_eq!(lines[0], BAD_BACKUP);
    let copy = lines[1]
        .strip_prefix(
            "save_restore:write_save_file: myFileCopy() failed for 'DIR/set.savB' -> \
             'DIR/set.savB_SBAD_",
        )
        .and_then(|rest| rest.strip_suffix('\''));
    assert!(copy.is_some(), "{lines:?}");
    assert_eq!(
        lines[2],
        "save_restore:write_it - unable to open file 'DIR/set.savB' [DATE]"
    );
    assert_eq!(
        lines[4],
        "save_restore:write_save_file: Can't write new backup file. [DATE]"
    );
}

/// C `check_file` (save_restore.c:1642-1690) cannot seek 7 bytes back in a
/// shorter `.savB`.
#[test]
fn a_short_backup_fails_check_files_seek() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("set.sav"), GOOD).unwrap();
    std::fs::write(dir.path().join("set.savB"), "").unwrap();
    assert_eq!(
        console_of("child_save", dir.path(), "savb"),
        ["save_restore:check_file: seek failed", BAD_BACKUP]
    );
}
