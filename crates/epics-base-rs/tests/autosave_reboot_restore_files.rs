//! Which file C `reboot_restore` reads, and what it leaves behind: the item's
//! filename under `set_savefile_path` (dbrestore.c:819-823).

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

/// Boot `record(ao, "A")` with `files` in the save directory and `line` in
/// st.cmd; A's value once iocInit is done.
async fn booted(files: &[(&str, &str)], line: &str) -> (f64, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("t.db"), "record(ao, \"A\") { }\n").unwrap();
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    let found = Arc::new(Mutex::new(None));
    let out = found.clone();
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_line(&format!("set_savefile_path(\"{}\")", dir.path().display()))
        .startup_line(line)
        .startup_line(&format!(
            "dbLoadRecords(\"{}\")",
            dir.path().join("t.db").display()
        ))
        .run(move |config| async move {
            *out.lock().unwrap() = Some(config.db.get_pv("A").unwrap().to_f64().unwrap());
            Ok(())
        })
        .await
        .unwrap();
    let value = found.lock().unwrap().take().unwrap();
    (value, dir)
}

/// C opens the name it was given; no `.sav` is added.
#[epics_macros_rs::epics_test]
async fn the_file_is_the_name_given() {
    let (value, _dir) = booted(
        &[("s", "# h\nA 5\n<END>\n"), ("s.sav", "# h\nA 6\n<END>\n")],
        "set_pass1_restoreFile(\"s\")",
    )
    .await;
    assert_eq!(value, 5.0);
}

const GOOD: &str = "# save/restore V5.10\nA 5\n<END>\n";
const TORN: &str = "# save/restore V5.10\nA 6\n";

/// A `.sav` without its `<END>` is not trusted, and is kept as
/// `<file>_RBAD_<date>`; the `.savB` is read (dbrestore.c:1252-1256).
#[epics_macros_rs::epics_test]
async fn a_bad_sav_falls_back_to_the_savb() {
    let (value, dir) = booted(
        &[("s.sav", TORN), ("s.savB", GOOD)],
        "set_pass1_restoreFile(\"s.sav\")",
    )
    .await;
    assert_eq!(value, 5.0);
    let rbad: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("s.sav_RBAD_"))
        .collect();
    assert_eq!(rbad.len(), 1, "{rbad:?}");
}

/// Then the sequence files (:1258-1305).
#[epics_macros_rs::epics_test]
async fn a_bad_savb_falls_back_to_a_sequence_file() {
    let (value, _dir) = booted(
        &[("s.sav", TORN), ("s.savB", TORN), ("s.sav1", GOOD)],
        "set_pass1_restoreFile(\"s.sav\")",
    )
    .await;
    assert_eq!(value, 5.0);
}

/// The item's status when a sequence file was read (dbrestore.c:833-836);
/// it stands over the error count (:1063-1065).
#[epics_macros_rs::epics_test]
async fn a_sequence_file_restore_warns() {
    use epics_base_rs::server::autosave::save_set::SrStatus;
    use epics_base_rs::server::autosave::startup::RestoreDef;
    use epics_base_rs::server::autosave::{BootRestore, RestorePass, reboot_restore};
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.sav0"), GOOD).unwrap();
    let db = epics_base_rs::server::database::PvDatabase::new();
    db.add_record(
        "A",
        Box::new(epics_base_rs::server::records::ao::AoRecord::new(0.0)),
    )
    .await
    .unwrap();
    let item = BootRestore {
        path: dir.path().join("s.sav"),
        status: RestoreDef::new("s.sav".into(), String::new()).status,
        settings: Default::default(),
        filename: "s.sav".into(),
        macrostring: String::new(),
        write_backup: false,
    };
    assert_eq!(
        reboot_restore(&db, &item, RestorePass::One).await.unwrap(),
        1
    );
    let status = item.status.lock().unwrap().clone();
    assert_eq!(
        (status.code, status.text.as_str()),
        (SrStatus::Warn, "Bad .sav(B) files; used seq. backup")
    );
}

/// The `_YYMMDD-HHMMSS` names in `dir` C's boot backup of `stem` gives.
fn boot_backups(dir: &std::path::Path, stem: &str) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix(&format!("{stem}_")))
                .is_some_and(|ts| ts.len() == 13 && ts.as_bytes()[6] == b'-')
        })
        .collect()
}

/// A restored file is copied to `<file>_<date>` (dbrestore.c:1031-1051).
#[epics_macros_rs::epics_test]
async fn a_restore_writes_a_boot_backup() {
    let (_, dir) = booted(&[("s.sav", GOOD)], "set_pass1_restoreFile(\"s.sav\")").await;
    let backups = boot_backups(dir.path(), "s.sav");
    assert_eq!(backups.len(), 1, "{:?}", std::fs::read_dir(dir.path()));
    assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), GOOD);
}

/// "don't write boot-time backups for files specified with full path" (:1028).
#[epics_macros_rs::epics_test]
async fn an_absolute_name_gets_no_boot_backup() {
    let other = tempfile::tempdir().unwrap();
    let sav = other.path().join("s.sav");
    std::fs::write(&sav, GOOD).unwrap();
    let (value, _dir) = booted(
        &[],
        &format!("set_pass1_restoreFile(\"{}\")", sav.display()),
    )
    .await;
    assert_eq!(value, 5.0);
    assert!(boot_backups(other.path(), "s.sav").is_empty());
}

/// The backup copies `fname`, not the file read, so restoring from the
/// `.savB` of a missing `.sav` cannot write it (:1044-1050).
#[epics_macros_rs::epics_test]
async fn a_boot_backup_that_cannot_be_written_warns() {
    use epics_base_rs::server::autosave::save_set::SrStatus;
    use epics_base_rs::server::autosave::startup::RestoreDef;
    use epics_base_rs::server::autosave::{BootRestore, RestorePass, reboot_restore};
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.savB"), GOOD).unwrap();
    let db = epics_base_rs::server::database::PvDatabase::new();
    db.add_record(
        "A",
        Box::new(epics_base_rs::server::records::ao::AoRecord::new(0.0)),
    )
    .await
    .unwrap();
    let item = BootRestore {
        path: dir.path().join("s.sav"),
        status: RestoreDef::new("s.sav".into(), String::new()).status,
        settings: Default::default(),
        filename: "s.sav".into(),
        macrostring: String::new(),
        write_backup: true,
    };
    assert_eq!(
        reboot_restore(&db, &item, RestorePass::One).await.unwrap(),
        1
    );
    let status = item.status.lock().unwrap().clone();
    assert_eq!(
        (status.code, status.text.as_str()),
        (SrStatus::Warn, "Can't write backup file")
    );
}

/// Each line is expanded with the item's macro string (dbrestore.c:840-853,
/// :857-866).
#[epics_macros_rs::epics_test]
async fn the_restore_file_macros_expand_each_line() {
    let (value, _dir) = booted(
        &[("s.sav", "# h\n$(P) 5\n<END>\n")],
        "set_pass1_restoreFile(\"s.sav\", \"P=A\")",
    )
    .await;
    assert_eq!(value, 5.0);
}
