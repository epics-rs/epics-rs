// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — its one hand-built tokio runtime ran in a suite that passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --test autosave_dbd_variables` (1/1).
//! asSupport.dbd's `variable()` lines (asSupport.dbd:19-24) make six of
//! save_restore's globals iocsh variables, so `var save_restoreDebug 1`
//! sets the level the task reads.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

#[test]
fn var_writes_the_six_dbd_variables() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    let h = holder.clone();
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        let shell = IocShell::new(Arc::new(PvDatabase::new()), bridge);
        for cmd in AutosaveStartupConfig::register_startup_commands(h) {
            shell.register(cmd);
        }
        for (name, n) in [
            ("save_restoreDebug", 7),
            ("save_restoreNumSeqFiles", 12),
            ("save_restoreSeqPeriodInSeconds", 5),
            ("save_restoreIncompleteSetsOk", 0),
            ("save_restoreDatedBackupFiles", 0),
            ("save_restoreRemountThreshold", 4),
        ] {
            shell.execute_line(format!("var {name} {n}")).unwrap();
        }
    })
    .join()
    .unwrap();
    let s = holder.lock().unwrap().settings.clone();
    let got = [
        &s.debug,
        &s.num_seq_files,
        &s.seq_period_in_seconds,
        &s.incomplete_sets_ok,
        &s.dated_backup_files,
        &s.remount_threshold,
    ]
    .map(|v| v.load(Ordering::Relaxed));
    // C's `var` writes the int as given; the task clamps at its next pass.
    assert_eq!(got, [7, 12, 5, 0, 0, 4]);
}
