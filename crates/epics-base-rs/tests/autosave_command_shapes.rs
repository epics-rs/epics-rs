// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — its one hand-built tokio runtime ran in a suite that passed under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p epics-base-rs --test autosave_command_shapes` (2/2).
//! C's autosave iocsh table (save_restore.c:3727-4023, dbrestore.c:1746-
//! 1794): each command's argument names and types, and no usage text, so
//! `help` prints the synopsis alone.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const C_SYNOPSES: &[&str] = &[
    "create_manual_set filename macrostring",
    "create_monitor_set filename period 'macro string'",
    "create_periodic_set filename period 'macro string'",
    "create_triggered_set filename trigger_channel 'macro string'",
    "fdbrestore filename",
    "save_restoreSet_status_prefix prefix",
    "set_pass0_restoreFile file macrostring",
    "set_pass1_restoreFile file macrostring",
    "set_requestfile_path path subpath",
    "set_savefile_path path subpath",
];

#[test]
fn help_prints_cs_synopsis_alone() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("help.txt");
    let found = std::thread::spawn(move || {
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        let shell = IocShell::new(Arc::new(PvDatabase::new()), bridge);
        let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
        for cmd in AutosaveStartupConfig::register_startup_commands(holder) {
            shell.register(cmd);
        }
        let mut found = Vec::new();
        for synopsis in C_SYNOPSES {
            let name = synopsis.split(' ').next().unwrap();
            shell
                .execute_line(format!("help {name} > \"{}\"", out.display()))
                .unwrap();
            // C's bold name, which `NO_COLOR` turns off.
            let text = std::fs::read_to_string(&out).unwrap();
            found.push(text.replace("\x1b[1m", "").replace("\x1b[0m", ""));
        }
        found
    })
    .join()
    .unwrap();
    let want: Vec<String> = C_SYNOPSES.iter().map(|s| format!("\n{s}\n")).collect();
    assert_eq!(found, want);
}

/// Every command C autosave registers (`IOCSH_FUNCDEF` in asApp/src).
const C_COMMANDS: &[&str] = &[
    "appendToFile",
    "asVerify",
    "autosaveBuild",
    "create_manual_set",
    "create_monitor_set",
    "create_periodic_set",
    "create_triggered_set",
    "dbrestoreShow",
    "eraseFile",
    "fdbrestore",
    "fdbrestoreX",
    "makeAutosaveFileFromDbInfo",
    "makeAutosaveFiles",
    "manual_save",
    "reload_manual_set",
    "reload_monitor_set",
    "reload_periodic_set",
    "reload_triggered_set",
    "remove_data_set",
    "save_restoreSet_CallbackTimeout",
    "save_restoreSet_CAReconnect",
    "save_restoreSet_DatedBackupFiles",
    "save_restoreSet_Debug",
    "save_restoreSet_FilePermissions",
    "save_restoreSet_IncompleteSetsOk",
    "save_restoreSet_NFSHost",
    "save_restoreSet_NumSeqFiles",
    "save_restoreSet_periodicDatedBackups",
    "save_restoreSet_RetrySeconds",
    "save_restoreSet_SeqPeriodInSeconds",
    "save_restoreSet_status_prefix",
    "save_restoreSet_UseStatusPVs",
    "save_restoreShow",
    "set_pass0_restoreFile",
    "set_pass1_restoreFile",
    "set_requestfile_path",
    "set_savefile_name",
    "set_savefile_path",
    "set_saveTask_priority",
];

#[test]
fn every_command_is_cs() {
    let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
    let extra: Vec<String> = AutosaveStartupConfig::register_startup_commands(holder)
        .into_iter()
        .map(|c| c.name.to_string())
        .filter(|n| !C_COMMANDS.contains(&n.as_str()))
        .collect();
    assert!(extra.is_empty(), "{extra:?}");
}
