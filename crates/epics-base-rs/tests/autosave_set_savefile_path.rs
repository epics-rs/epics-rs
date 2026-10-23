// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — its one hand-built tokio runtime ran in a suite that passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --test autosave_set_savefile_path` (1/1).
//! C `set_savefile_path` (save_restore.c:2623-2636) stores the path and
//! makes nothing; a save to a directory that is not there is `write_it`'s
//! "unable to open file".

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

#[test]
fn the_save_path_is_not_created() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("autosave");
    let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
    let h = holder.clone();
    let line = format!("set_savefile_path(\"{}\", \"sub\")", missing.display());
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        let shell = IocShell::new(Arc::new(PvDatabase::new()), bridge);
        for cmd in AutosaveStartupConfig::register_startup_commands(h) {
            shell.register(cmd);
        }
        shell.execute_line(&line).unwrap();
    })
    .join()
    .unwrap();
    assert_eq!(
        holder.lock().unwrap().save_file_path,
        Some(missing.join("sub"))
    );
    assert!(!missing.exists());
}
