// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — its one hand-built tokio runtime ran in a suite that passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --test autosave_settings_commands` (3/3).
//! C's `save_restoreSet_*` setters (save_restore.c:453-491) write the
//! globals the save task and the restores read; two of them print.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::settings::SrSettings;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const CHILD: &str = "AUTOSAVE_SETTINGS_COMMANDS_LINES";

/// `lines` run in an iocsh with the autosave startup commands; the
/// settings they leave.
fn run(lines: &[&str]) -> Arc<SrSettings> {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    let lines: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    let h = holder.clone();
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        let shell = IocShell::new(Arc::new(PvDatabase::new()), bridge);
        for cmd in AutosaveStartupConfig::register_startup_commands(h) {
            shell.register(cmd);
        }
        for line in lines {
            shell.execute_line(&line).unwrap();
        }
    })
    .join()
    .unwrap();
    holder.lock().unwrap().settings.clone()
}

/// `$CHILD`'s lines, `;`-separated, in the child only.
#[test]
fn child_settings() {
    let Ok(lines) = std::env::var(CHILD) else {
        return;
    };
    run(&lines.split(';').collect::<Vec<_>>());
}

fn stdout_of(lines: &str) -> Vec<String> {
    let name = "child_settings";
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, lines)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let prefix = format!("test {name} ... ");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.strip_prefix(prefix.as_str()).unwrap_or(l).to_owned())
        .filter(|l| {
            !(l.is_empty()
                || l == "ok"
                || l.starts_with("running ")
                || l.starts_with("test result"))
        })
        .collect()
}

fn get(v: &std::sync::atomic::AtomicI32) -> i32 {
    v.load(Ordering::Relaxed)
}

#[test]
fn each_setter_writes_its_global() {
    let s = run(&[
        "save_restoreSet_Debug(2)",
        "save_restoreSet_NumSeqFiles(5)",
        "save_restoreSet_SeqPeriodInSeconds(3)",
        "save_restoreSet_IncompleteSetsOk(0)",
        "save_restoreSet_DatedBackupFiles(0)",
        "save_restoreSet_FilePermissions(0640)",
        "save_restoreSet_RetrySeconds(-1)",
        "save_restoreSet_UseStatusPVs(0)",
        "save_restoreSet_CAReconnect(1)",
        "save_restoreSet_CallbackTimeout(2)",
        "save_restoreSet_periodicDatedBackups(2)",
    ]);
    assert_eq!(get(&s.debug), 2);
    assert_eq!(get(&s.num_seq_files), 5);
    // MAX(10, period) (:455)
    assert_eq!(get(&s.seq_period_in_seconds), 10);
    assert_eq!(get(&s.incomplete_sets_ok), 0);
    assert_eq!(get(&s.dated_backup_files), 0);
    assert_eq!(s.file_permissions.load(Ordering::Relaxed), 0o640);
    assert_eq!(get(&s.must_set_permissions), 1);
    // a negative one is refused (:480)
    assert_eq!(get(&s.retry_seconds), 60);
    assert_eq!(get(&s.use_status_pvs), 0);
    assert_eq!(get(&s.ca_reconnect), 1);
    // 0 <= t < MIN_PERIOD is refused (:486)
    assert_eq!(get(&s.callback_timeout), 600);
    assert_eq!(get(&s.periodic_dated_backups), 1);
    assert_eq!(get(&s.dated_backup_period), 120);

    let s = run(&["save_restoreSet_periodicDatedBackups(0)"]);
    assert_eq!(get(&s.periodic_dated_backups), 0);
}

/// :474 and :489.
#[test]
fn the_permissions_and_a_refused_timeout_are_printed() {
    assert_eq!(
        stdout_of("save_restoreSet_FilePermissions(0640);save_restoreSet_CallbackTimeout(2)"),
        [
            "save_restore: File permissions set to 0640",
            "save_restoreCallbackTimeout must be either negative (forever) or >= 4 seconds",
        ]
    );
}
