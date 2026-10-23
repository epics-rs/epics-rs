// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — its one hand-built tokio runtime ran in a suite that passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --test autosave_manual_restore` (6/6).
//! C `fdbrestore(filename)` (save_restore.c:2809-2813) queues
//! `do_manual_restore(filename, FROM_SAVE_FILE)` (:3195-3473) to the save
//! task: the file named, under the save path, through `fopen_and_check`,
//! its lines put with `ca_put` and the file copied to `<file>.bu`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::records::ao::AoRecord;

const CHILD: &str = "AUTOSAVE_MANUAL_RESTORE_DIR";

/// The stdout autosave lines and the errlog lines of the child test `name`
/// run on `dir`, with `dir` written `DIR`.
fn output_of(name: &str, dir: &Path) -> (Vec<String>, Vec<String>) {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "{name}: {out:?}");
    let prefix = format!("test {name} ... ");
    let dir = dir.display().to_string();
    let lines = |text: &[u8], keep: &dyn Fn(&str) -> bool| -> Vec<String> {
        String::from_utf8_lossy(text)
            .lines()
            .map(|l| l.strip_prefix(prefix.as_str()).unwrap_or(l))
            .filter(|l| keep(l))
            .map(|l| l.replace(&dir, "DIR"))
            .collect()
    };
    (
        lines(&out.stdout, &|l| l.contains("save_restore")),
        lines(&out.stderr, &|l| {
            l.contains("save_restore.c") || l.contains("dbrestore.c")
        }),
    )
}

/// `line` run in an iocsh with the autosave startup commands, a running
/// save task, the save path `$CHILD` and `record(ao, "A")`, in the child
/// only; `A`'s value once the task has served the line's ops.
fn child_shell(line: &str) -> Option<(f64, PathBuf)> {
    let dir = PathBuf::from(std::env::var_os(CHILD)?);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let db = Arc::new(PvDatabase::new());
    rt.block_on(db.add_record("A", Box::new(AoRecord::new(0.0))))
        .unwrap();
    let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
    holder.lock().unwrap().save_file_path = Some(dir.clone());
    let settings = holder.lock().unwrap().settings.clone();
    let mgr = Arc::new(rt.block_on(AutosaveBuilder::new().settings(settings).build()));
    holder.lock().unwrap().manager = Some(mgr.clone());
    let _task = mgr.clone().start(db.clone());
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    let shell_db = db.clone();
    let line = line.to_string();
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        let shell = IocShell::new(shell_db, bridge);
        for cmd in AutosaveStartupConfig::register_startup_commands(holder) {
            shell.register(cmd);
        }
        shell.execute_line(&line).unwrap();
    })
    .join()
    .unwrap();
    rt.block_on(mgr.ops_served());
    epics_base_rs::runtime::log::errlog_flush();
    Some((db.get_pv("A").unwrap().to_f64().unwrap(), dir))
}

#[test]
fn child_restore_s() {
    if let Some((value, dir)) = child_shell("fdbrestore(\"s.sav\")") {
        assert_eq!(value, 3.0);
        assert_eq!(
            std::fs::read_to_string(dir.join("s.sav.bu")).unwrap(),
            std::fs::read_to_string(dir.join("s.sav")).unwrap()
        );
    }
}

#[test]
fn child_restore_none() {
    if let Some((value, _)) = child_shell("fdbrestore(\"none.sav\")") {
        assert_eq!(value, 0.0);
    }
}

/// The `!` line (:3426-3429) and a PV no IOC serves (:3367-3369).
#[test]
fn the_file_named_is_restored_with_cs_lines() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("s.sav"),
        "# h\n! 2 channel(s) not connected - or not all gets were successful\nB 1\nA 3\n<END>\n",
    )
    .unwrap();
    let (stdout, errlog) = output_of("child_restore_s", dir.path());
    assert_eq!(
        stdout,
        [
            "save_restore:fdbrestore:entry",
            "save_restore:do_manual_restore: 2 PVs had no saved value",
            "save_restore:do_manual_restore: ca_search for B timeout",
        ]
    );
    assert_eq!(errlog, Vec::<String>::new());
}

/// :3282-3286, after `fopen_and_check`'s own lines.
#[test]
fn a_file_that_cannot_be_opened_is_named() {
    let dir = tempfile::tempdir().unwrap();
    let (stdout, errlog) = output_of("child_restore_none", dir.path());
    assert_eq!(
        stdout,
        [
            "save_restore:fdbrestore:entry",
            "save_restore: **********************************",
        ]
    );
    assert_eq!(
        errlog.last().map(String::as_str),
        Some("../save_restore.c:do_manual_restore Can't open save file.")
    );
}

#[test]
fn child_restore_x() {
    if let Some((value, dir)) = child_shell("fdbrestoreX(\"x.cfg\", \"P=A\")") {
        assert_eq!(value, 4.0);
        assert!(!dir.join("x.cfg.bu").exists());
    }
}

/// `fdbrestoreX(filename, macrostring)` (:2815-2818) is `FROM_ASCII_FILE`:
/// no header line to skip, the macros expanded, no `.bu`, and the file
/// `asVerify`ed at verbose -1 (:1146-1158); the shell gives no callback, so
/// `defaultCallback` prints the status.
#[test]
fn an_ascii_file_is_restored_with_its_macros_and_verified() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.cfg"), "$(P) 4\nB 1\n").unwrap();
    let (stdout, errlog) = output_of("child_restore_x", dir.path());
    assert_eq!(
        stdout,
        [
            "save_restore:do_manual_restore: ca_search for B timeout",
            "save_restore:defaultCallback:status=1",
        ]
    );
    assert_eq!(errlog, Vec::<String>::new());
}
