//! The save task with a managed NFS mount that has failed: past
//! `save_restoreRemountThreshold` I/O errors `save_restoreNFSOK` is 0
//! (save_restore.c:1987-1990), and each pass sets its status from
//! `restore_mount` (:913-921), which within its first minute is OK.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::manager::{AutosaveBuilder, AutosaveManager, SaveTaskHandle};
use epics_base_rs::server::autosave::nfs::save_restore_set_nfs_host;
use epics_base_rs::server::autosave::save_set::SrStatus;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// A list-less manager on a startup config whose mount `/mnt` is managed
/// and has had 11 failed opens, its task started; the manager holds the
/// config only weakly.
struct Rig {
    mgr: Arc<AutosaveManager>,
    task: Option<SaveTaskHandle>,
    _holder: Arc<Mutex<AutosaveStartupConfig>>,
}

async fn failed_mount() -> Rig {
    failed_mount_on(Arc::new(PvDatabase::new()), AutosaveBuilder::new()).await
}

/// [`failed_mount`] from `builder`, on `db`.
async fn failed_mount_on(db: Arc<PvDatabase>, builder: AutosaveBuilder) -> Rig {
    let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
    let settings = {
        let mut cfg = holder.lock().unwrap();
        save_restore_set_nfs_host(&mut cfg, "h", "1.2.3.4", Some("/mnt"));
        cfg.settings.clone()
    };
    for _ in 0..11 {
        settings.note_io_error(true);
    }
    assert!(!settings.nfs_ok());
    let mgr = Arc::new(builder.settings(settings).build().await);
    mgr.attach_startup(&holder);
    let task = mgr.clone().start(db);
    Rig {
        mgr,
        task,
        _holder: holder,
    }
}

/// The pass's status is `restore_mount`'s.
#[epics_macros_rs::epics_test]
async fn a_failed_mount_sets_the_pass_status() {
    let Rig { mgr, task, _holder } = failed_mount().await;
    sleep(Duration::from_millis(300)).await;
    let status = mgr.sr_status();
    mgr.shutdown();
    drop(task);
    assert_eq!(
        (status.code, status.text.as_str()),
        (SrStatus::Ok, "NFS remounted")
    );
}

const CHILD: &str = "AUTOSAVE_NFS_TASK_CHILD";

/// [`failed_mount`] for a moment, in the child only.
#[epics_macros_rs::epics_test]
async fn child_task() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let Rig { mgr, task, _holder } = failed_mount().await;
    sleep(Duration::from_millis(300)).await;
    mgr.shutdown();
    drop(task);
}

/// The task tries the mount once as it starts (:804-810), after
/// `save_restoreSet_NFSHost`'s own try.
#[test]
fn the_task_mounts_a_failed_file_system_at_its_start() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_task", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mounts = stdout
        .lines()
        .filter(|l| *l == "save_restore: Can't mount '/mnt'")
        .count();
    assert_eq!(mounts, 2, "{stdout}");
}

/// The I/O errors' "Too many I/O errors" is the newest event
/// SR_recentlyStr shows (:1987-1990).
#[epics_macros_rs::epics_test]
async fn too_many_io_errors_is_the_recent_event() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(stringout, "T:SR_recentlyStr") { field(VAL, "Status unknown") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let Rig { mgr, task, _holder } =
        failed_mount_on(db.clone(), AutosaveBuilder::new().status_prefix("T:")).await;
    sleep(Duration::from_millis(300)).await;
    mgr.shutdown();
    drop(task);
    assert_eq!(
        db.get_pv("T:SR_recentlyStr").unwrap(),
        EpicsValue::String("Too many I/O errors".into())
    );
}
