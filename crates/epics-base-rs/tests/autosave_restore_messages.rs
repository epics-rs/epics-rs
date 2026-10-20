//! C `reboot_restore` (dbrestore.c:760-1092) and its `scalar_restore`
//! (:222-327) say on the errlog which PV a boot restore could not restore,
//! and why.

use std::path::{Path, PathBuf};

use epics_base_rs::server::autosave::startup::RestoreDef;
use epics_base_rs::server::autosave::{BootRestore, RestorePass, reboot_restore};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;

const CHILD: &str = "AUTOSAVE_RESTORE_MESSAGES_DIR";
/// `save_restoreIncompleteSetsOk` for the child, 1 when unset.
const OK: &str = "AUTOSAVE_RESTORE_MESSAGES_OK";

/// The errlog lines naming an autosave source file of the child test `name`
/// run on `dir`, with `dir` written `DIR`.
fn errlog_of(name: &str, dir: &Path) -> Vec<String> {
    errlog_with(name, dir, "1")
}

fn errlog_with(name: &str, dir: &Path, ok: &str) -> Vec<String> {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, dir)
        .env(OK, ok)
        .output()
        .unwrap();
    assert!(out.status.success(), "{name}: {out:?}");
    let dir = dir.display().to_string();
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.contains("dbrestore.c"))
        .map(|l| l.replace(&dir, "DIR"))
        .collect()
}

/// A pass-1 boot restore of `$CHILD/s.sav` into `record(ao, "A")`, in the
/// child only.
#[epics_macros_rs::epics_test]
async fn child_restore() {
    let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) else {
        return;
    };
    let db = PvDatabase::new();
    db.add_record("A", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let item = BootRestore {
        path: dir.join("s.sav"),
        status: RestoreDef::new("s.sav".into(), String::new()).status,
        filename: "s.sav".into(),
        macrostring: String::new(),
        write_backup: false,
        settings: Default::default(),
    };
    let ok = std::env::var(OK).map_or(1, |v| v.parse().unwrap());
    item.settings.set_incomplete_sets_ok(ok);
    let _ = reboot_restore(&db, &item, RestorePass::One).await;
    epics_base_rs::runtime::log::errlog_flush();
}

fn restore_of(sav: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.sav"), sav).unwrap();
    errlog_of("child_restore", dir.path())
}

/// :966-974, with `.VAL` added to a bare name (:912). A missing field is
/// `dbFindField`'s `S_dbLib_fieldNotFound` (dbStaticLib.c:1847-1853), the
/// same line as a missing record, as the C oracle IOC prints it.
#[test]
fn a_missing_record_or_field_is_named() {
    assert_eq!(
        restore_of("# h\nB 1\nA.NOPE 2\nA 3\n<END>\n"),
        [
            "../dbrestore.c:reboot_restore dbFindRecord for 'B.VAL' failed",
            "../dbrestore.c:reboot_restore dbFindRecord for 'A.NOPE' failed",
        ]
    );
}

/// `scalar_restore`'s line and its `errMessage` (:316-319), then
/// `reboot_restore`'s (:981-984).
#[test]
fn a_refused_value_is_named() {
    assert_eq!(
        restore_of("# h\nA.SCAN 10\n<END>\n"),
        [
            "../dbrestore.c:scalar_restore restore of '10' for 'A.SCAN' failed",
            "Bad Field value filename=\"../dbrestore.c\" line number=318   ",
            "../dbrestore.c:reboot_restore restore for PV 'A.SCAN' failed",
        ]
    );
}

/// :994-998.
#[test]
fn a_saved_error_message_is_counted() {
    assert_eq!(
        restore_of("# h\n! 2 channel(s) not connected or fetch failed\nA 3\n<END>\n"),
        ["../dbrestore.c:reboot_restore 2 PVs had no saved value."]
    );
}

/// :1003-1005, with `save_restoreIncompleteSetsOk` cleared.
#[test]
fn an_incomplete_file_is_refused_when_not_ok() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("s.sav"),
        "# h\n! 2 channel(s) not connected or fetch failed\nA 3\n<END>\n",
    )
    .unwrap();
    assert_eq!(
        errlog_with("child_restore", dir.path(), "0"),
        [
            "../dbrestore.c:reboot_restore 2 PVs had no saved value.",
            "../dbrestore.c:reboot_restore aborting restore",
        ]
    );
}

/// :827-828, after `fopen_and_check`'s own lines over C's default three
/// sequence files (:1265-1301).
#[test]
fn a_file_that_cannot_be_opened_is_named() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        errlog_of("child_restore", dir.path()),
        [
            "../dbrestore.c:checkFile Can't open file 'DIR/s.sav'.",
            "../dbrestore.c:fopen_and_check Trying backup file 'DIR/s.savB'",
            "../dbrestore.c:checkFile Can't open file 'DIR/s.savB'.",
            "../dbrestore.c:fopen_and_check Can't figure out which seq file is most recent,",
            "../dbrestore.c:fopen_and_check so I'm just going to start with 'DIR/s.sav0'.",
            "../dbrestore.c:fopen_and_check Trying backup file 'DIR/s.sav0'",
            "../dbrestore.c:checkFile Can't open file 'DIR/s.sav0'.",
            "../dbrestore.c:fopen_and_check Trying backup file 'DIR/s.sav1'",
            "../dbrestore.c:checkFile Can't open file 'DIR/s.sav1'.",
            "../dbrestore.c:fopen_and_check Trying backup file 'DIR/s.sav2'",
            "../dbrestore.c:checkFile Can't open file 'DIR/s.sav2'.",
            "../dbrestore.c:fopen_and_check Can't find a file to restore from...",
            "../dbrestore.c:fopen_and_check ...last tried 'DIR/s.sav2'. I give up.",
            "../dbrestore.c:reboot_restore Can't open save file.",
        ]
    );
}

/// dbStatic finds no field with a filter after it (dbStaticLib.c:1855-1858),
/// so the C oracle IOC restores nothing to `A.DESC{"x":1}$`.
#[test]
fn a_filtered_name_is_not_restored_at_boot() {
    assert_eq!(
        restore_of("# h\nA.DESC{\"x\":1}$ hi\n<END>\n"),
        ["../dbrestore.c:reboot_restore dbFindRecord for 'A.DESC{\"x\":1}$' failed"]
    );
}
