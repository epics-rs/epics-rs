//! SR_rebootStatus, SR_rebootStatusStr and SR_rebootTime, written once when
//! the save task starts (save_restore.c:854-877): the worst status of the
//! `set_pass0_restoreFile` then `set_pass1_restoreFile` items, each born
//! INIT "Unknown, probably failed" (dbrestore.c:1111-1118) and set by the
//! boot restore of the first item naming its file (:801-809).
//!
//! C quadEM (`commonPlugins.cmd` and `saveRestore.cmd` both name
//! `auto_settings.sav` for both passes, no file on disk), save_restoreStatus.db:
//!
//! ```text
//! $ caget QE1:SR_rebootStatus QE1:SR_rebootStatusStr
//! QE1:SR_rebootStatus            No Status
//! QE1:SR_rebootStatusStr         Unknown, probably failed
//! ```

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandContext};
use epics_base_rs::types::EpicsValue;

/// The IOC booted from `lines`, its save task started by a
/// `create_manual_set` after `iocInit`; what the three PVs read once the
/// task has had its start.
async fn reboot_pvs(dir: &std::path::Path, lines: &[&str]) -> (f64, String, String) {
    let found = Arc::new(Mutex::new(None));
    let out = found.clone();
    std::fs::write(dir.join("a.db"), "record(ao, \"T:A\") { }\n").unwrap();
    let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
    let mut app = IocApplication::new()
        .port(0)
        .autosave_startup(holder.clone())
        .startup_line(&format!("set_savefile_path(\"{}\")", dir.display()))
        .startup_line("save_restoreSet_status_prefix(\"T:\")")
        .startup_line("dbLoadRecords(\"$(AUTOSAVE)/db/save_restoreStatus.db\", \"P=T:\")")
        .startup_line(&format!(
            "dbLoadRecords(\"{}\")",
            dir.join("a.db").display()
        ));
    for line in lines {
        app = app.startup_line(line);
    }
    app.run(move |config| async move {
        let cmds = AutosaveStartupConfig::register_startup_commands(holder);
        let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
        let db = config.db.clone();
        std::thread::spawn(move || {
            let ctx = CommandContext::new(db, bridge);
            let create = cmds.iter().find(|c| c.name == "create_manual_set");
            let args = [ArgValue::String("".into())];
            create.unwrap().handler.call(&args, &ctx).unwrap();
        })
        .join()
        .unwrap();
        // SR_rebootTime is the task's last put at its start.
        let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
        while std::time::Instant::now() < deadline {
            if is_time(&read(&config.db).2) {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
        *out.lock().unwrap() = Some(read(&config.db));
        Ok(())
    })
    .await
    .unwrap();
    found.lock().unwrap().take().unwrap()
}

fn read(db: &PvDatabase) -> (f64, String, String) {
    let text = |name: &str| match db.get_pv(name).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        other => panic!("{name}: {other:?}"),
    };
    (
        db.get_pv("T:SR_rebootStatus").unwrap().to_f64().unwrap(),
        text("T:SR_rebootStatusStr"),
        text("T:SR_rebootTime"),
    )
}

/// `%a %b %d %H:%M:%S`, C's TIMEFMT_noY.
fn is_time(text: &str) -> bool {
    chrono::NaiveTime::parse_from_str(text.get(11..).unwrap_or(""), "%H:%M:%S").is_ok()
        && text.len() == 19
}

/// quadEM's shape: the second item naming the file is never restored into,
/// so it stays INIT, which is worse than the first item's FAIL.
#[epics_macros_rs::epics_test]
async fn a_file_named_twice_reads_cs_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (code, text, time) = reboot_pvs(
        dir.path(),
        &[
            "set_pass0_restoreFile(\"s.sav\")",
            "set_pass1_restoreFile(\"s.sav\")",
            "set_pass0_restoreFile(\"s.sav\")",
            "set_pass1_restoreFile(\"s.sav\")",
        ],
    )
    .await;
    assert_eq!((code, text.as_str()), (0.0, "Unknown, probably failed"));
    assert!(is_time(&time), "{time:?}");
}

/// dbrestore.c:827-831.
#[epics_macros_rs::epics_test]
async fn a_missing_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let (code, text, _) = reboot_pvs(dir.path(), &["set_pass0_restoreFile(\"s.sav\")"]).await;
    assert_eq!((code, text.as_str()), (1.0, "Can't open save file."));
}

/// An item's OK "No errors" (dbrestore.c:1070-1072) is not worse than the
/// task's own OK "Ok", so "Ok" stands (save_restore.c:866).
#[epics_macros_rs::epics_test]
async fn a_good_restore_is_no_errors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.sav"), "# h\nT:A 2\n<END>\n").unwrap();
    let (code, text, _) = reboot_pvs(
        dir.path(),
        &[
            "set_pass0_restoreFile(\"s.sav\")",
            "set_pass1_restoreFile(\"s.sav\")",
        ],
    )
    .await;
    assert_eq!((code, text.as_str()), (4.0, "Ok"));
}

/// No restore file leaves the task's own "Ok" (save_restore.c:855-856).
#[epics_macros_rs::epics_test]
async fn no_restore_file_is_ok() {
    let dir = tempfile::tempdir().unwrap();
    let (code, text, time) = reboot_pvs(dir.path(), &[]).await;
    assert_eq!((code, text.as_str()), (4.0, "Ok"));
    assert!(is_time(&time), "{time:?}");
}

/// PVs the database does not have (dbrestore.c:966-974).
#[epics_macros_rs::epics_test]
async fn pv_errors_warn() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("s.sav"),
        "# h\nT:A 2\nT:NONE 1\nT:NOTHER 1\n<END>\n",
    )
    .unwrap();
    let (code, text, _) = reboot_pvs(dir.path(), &["set_pass1_restoreFile(\"s.sav\")"]).await;
    assert_eq!((code, text.as_str()), (2.0, "2 PV errors"));
}

/// The `!` line (dbrestore.c:994-1002), whose text stands over the error
/// count.
#[epics_macros_rs::epics_test]
async fn an_error_message_line_warns() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("s.sav"),
        "# h\n! 1 channel(s) not connected or fetch failed\nT:NONE 1\n<END>\n",
    )
    .unwrap();
    let (code, text, _) = reboot_pvs(dir.path(), &["set_pass0_restoreFile(\"s.sav\")"]).await;
    assert_eq!(
        (code, text.as_str()),
        (2.0, ".sav file contained an error message")
    );
}
