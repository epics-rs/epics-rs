// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — its one hand-built tokio runtime ran in a suite that passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --test autosave_manual_save` (6/6).
//! C `manual_save(request file)` (save_restore.c:699-727, :3739-3743)
//! queues `op_SaveFile` (:1207-1235) to the save task: the list built from
//! that request file saved, its `.sav` `asVerify`'d, and with no callback
//! from the shell, `defaultCallback`'s status line. `set_savefile_name`
//! (:2264-2289) renames the file a list writes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::records::ao::AoRecord;

const CHILD: &str = "AUTOSAVE_MANUAL_SAVE_DIR";
const LINE: &str = "AUTOSAVE_MANUAL_SAVE_LINE";
const NO_SET: &str = "AUTOSAVE_MANUAL_SAVE_NO_SET";

fn output_of(dir: &Path, line: &str, with_set: bool) -> Vec<String> {
    outputs_of(dir, line, with_set).0
}

/// The child's stdout, as [`output_of`], and its errlog lines naming
/// autosave.
fn outputs_of(dir: &Path, line: &str, with_set: bool) -> (Vec<String>, Vec<String>) {
    let name = "child_manual_save";
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, dir)
        .env(LINE, line);
    if !with_set {
        cmd.env(NO_SET, "1");
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let prefix = format!("test {name} ... ");
    let stdout = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.strip_prefix(prefix.as_str()).unwrap_or(l).to_owned())
        .filter(|l| {
            !(l.is_empty()
                || l == "ok"
                || l.starts_with("running ")
                || l.starts_with("test result"))
        })
        .collect();
    let errlog = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.contains("save_restore"))
        .map(str::to_owned)
        .collect();
    (stdout, errlog)
}

/// `$LINE`, `;`-separated, in an iocsh whose manager has the manual set of `s.req`
/// (`A` = 1.5) saving to `$CHILD/s.sav`, or no manager with `$NO_SET`.
#[test]
fn child_manual_save() {
    let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) else {
        return;
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let db = Arc::new(PvDatabase::new());
    let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
    holder.lock().unwrap().save_file_path = Some(dir.clone());
    rt.block_on(async {
        db.add_record("A", Box::new(AoRecord::new(1.5)))
            .await
            .unwrap();
        if std::env::var_os(NO_SET).is_none() {
            std::fs::write(dir.join("s.req"), "A\n").unwrap();
            let mgr = AutosaveBuilder::new()
                .add_set(
                    SaveSetConfig {
                        name: "s".into(),
                        save_path: dir.join("s.sav"),
                        request_file: Some("s.req".into()),
                        request_pvs: Vec::new(),
                        backup: BackupConfig::default(),
                        macros: Default::default(),
                        macro_string: String::new(),
                        search_paths: vec![dir.clone()],
                    },
                    SaveStrategy::Manual,
                )
                .build()
                .await;
            holder.lock().unwrap().manager = Some(Arc::new(mgr));
        }
    });
    let line = std::env::var(LINE).unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        let shell = IocShell::new(db, bridge);
        for cmd in AutosaveStartupConfig::register_startup_commands(holder) {
            shell.register(cmd);
        }
        for line in line.split(';') {
            shell.execute_line(line).unwrap();
        }
    })
    .join()
    .unwrap();
    epics_base_rs::runtime::log::errlog_flush();
}

/// The set's own `.sav` and `.savB` written as any save writes them
/// (:2045-2047), and verified clean, which at verbose -1 prints nothing.
/// C reports -1 here, "Couldn't make backup ''", because its "" name takes
/// the configName path (:714, :2112-2124); that defect is not kept.
#[test]
fn the_set_of_the_request_file_is_saved_and_verified() {
    let dir = tempfile::tempdir().unwrap();
    let out = output_of(dir.path(), "manual_save(\"s.req\")", true);
    let savb = format!(
        "save_restore:write_save_file: Backup file ({}) bad or not found.  Writing a new one. [",
        dir.path().join("s.savB").display()
    );
    assert_eq!(out.len(), 2, "{out:?}");
    assert!(out[0].starts_with(&savb), "{out:?}");
    assert_eq!(out[1], "save_restore:defaultCallback:status=0");
    let sav = std::fs::read_to_string(dir.path().join("s.sav")).unwrap();
    assert!(sav.lines().any(|l| l.starts_with("A ")), "{sav}");
    assert!(dir.path().join("s.savB").exists());
}

/// No list has that request file: status -1 and no verify (:1213-1229).
#[test]
fn an_unknown_request_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        output_of(dir.path(), "manual_save(\"t.req\")", true),
        ["save_restore:defaultCallback:status=-1"]
    );
}

/// :702-705 and :723-726.
#[test]
fn the_refusals_are_cs() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        output_of(dir.path(), "manual_save", true),
        ["manual_save: bad filename"]
    );
    assert_eq!(
        output_of(dir.path(), "manual_save(\"s.req\")", false),
        ["manual_save: message queue not initialized (no restore set has been created?)"]
    );
}

/// The next save of the list writes the new name, beside the old one's
/// path (:2280).
#[test]
fn a_renamed_list_saves_to_its_new_file() {
    let dir = tempfile::tempdir().unwrap();
    output_of(
        dir.path(),
        "set_savefile_name(\"s.req\", \"n.sav\");manual_save(\"s.req\")",
        true,
    );
    assert!(dir.path().join("n.sav").exists());
    assert!(!dir.path().join("s.sav").exists());
}

/// :2268-2270 and :2286.
#[test]
fn a_rename_without_its_list_is_on_the_errlog() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        outputs_of(dir.path(), "set_savefile_name(\"t.req\", \"n.sav\")", true).1,
        ["../save_restore.c:set_savefile_name No save set enabled for t.req"]
    );
    assert_eq!(
        outputs_of(dir.path(), "set_savefile_name(\"s.req\", \"n.sav\")", false).1,
        ["  The save_restore task apparently is not running."]
    );
}
