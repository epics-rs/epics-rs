//! C `write_save_file` with a `configName` (save_restore.c:2081-2124): no
//! `.savB` check; a file already there is copied to `<file>_<datetime>`,
//! which then gets the new file too; with no file there `backup_file` stays
//! "" and the copy fails, so the save warns and fails. The expected lines
//! and files are the C oracle IOC's for two `manual_save`s, the first with
//! no `.sav` on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

const CHILD: &str = "AUTOSAVE_SAVE_TO_CONFIG_NAME_DIR";

/// Two saves of `A` to `m.sav`, `A` 3 then 4, each status printed, in the
/// child only.
#[epics_macros_rs::epics_test]
async fn child_save_to() {
    let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) else {
        return;
    };
    let db = Arc::new(PvDatabase::new());
    db.add_record("A", Box::new(AoRecord::new(3.0)))
        .await
        .unwrap();
    std::fs::write(dir.join("s.req"), "A\n").unwrap();
    let mgr = AutosaveBuilder::new()
        .add_set(
            SaveSetConfig {
                name: "s".into(),
                save_path: dir.join("s.sav"),
                request_file: Some("s.req".into()),
                request_pvs: Vec::new(),
                backup: BackupConfig { enable_savb: true },
                macros: HashMap::new(),
                macro_string: None,
                search_paths: vec![dir.clone()],
            },
            SaveStrategy::Manual,
        )
        .build()
        .await;
    let path = dir.join("m.sav");
    let first = mgr.save_file_op(&db, "s.req", "m.sav", &path).await;
    println!("status={first}");
    db.put_pv("A", EpicsValue::Double(4.0)).await.unwrap();
    // A second later, so the dated name is not the first save's.
    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(1100)).await;
    let second = mgr.save_file_op(&db, "s.req", "m.sav", &path).await;
    println!("status={second}");
    epics_base_rs::runtime::log::errlog_flush();
}

fn run_child(dir: &Path) -> (Vec<String>, Vec<String>) {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_save_to",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let lines = |bytes: &[u8], keep: fn(&str) -> bool| -> Vec<String> {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(|l| l.strip_prefix("test child_save_to ... ").unwrap_or(l))
            .filter(|l| keep(l))
            .map(|l| {
                let l = l.replace(&dir.display().to_string(), "DIR");
                match l.strip_suffix(']').and_then(|h| h.rsplit_once('[')) {
                    Some((head, _)) => format!("{head}[DATE]"),
                    None => l,
                }
            })
            .collect()
    };
    (
        lines(&out.stdout, |l| {
            l.contains("save_restore") || l.starts_with("status=")
        }),
        lines(&out.stderr, |l| {
            l.contains("dbrestore.c") || l.contains("save_restore.c")
        }),
    )
}

#[test]
fn a_config_name_save_keeps_a_dated_copy_and_fails_with_none_before() {
    let dir = tempfile::tempdir().unwrap();
    let (stdout, errlog) = run_child(dir.path());
    assert_eq!(
        stdout,
        [
            "save_restore:write_save_file - Couldn't make backup '' [DATE]",
            "save_restore: write_save_file() failed for s.sav",
            "status=-1",
            "status=0",
        ]
    );
    assert_eq!(errlog, ["../dbrestore.c:myFileCopy Can't open file ''"]);
    let mut names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "s.req")
        .collect();
    names.sort();
    assert_eq!(names.len(), 2, "{names:?}");
    assert_eq!(names[0], "m.sav");
    assert!(names[1].starts_with("m.sav_"), "{names:?}");
    // The dated copy is the second save's file, as C's second copy leaves it.
    for name in &names {
        let text = std::fs::read_to_string(dir.path().join(name)).unwrap();
        assert_eq!(text.split_once('\n').unwrap().1, "A 4\n<END>\n", "{name}");
    }
}
