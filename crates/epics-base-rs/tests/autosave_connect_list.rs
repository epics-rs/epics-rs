//! C's save task connects a list before enabling it, `connect_list(plist, 1)`
//! (save_restore.c:1276-1374), and says on the console how it went, as
//! the C oracle IOC did for a set made after `iocInit`:
//!
//! ```text
//! save_restore: connect failed for channel 'GONE1'
//! auto_settings.sav: 1 of 2 PV's connected
//! ```
//!
//! Its "not all searches successful" never printed. For a set made before
//! `iocInit` every member failed and none connected.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::{AutosaveBuilder, AutosaveManager, SaveTaskHandle};
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::autosave::settings::SrSettings;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_app::{IocApplication, RunOutcome};
use epics_base_rs::server::records::ao::AoRecord;

const CHILD: &str = "AUTOSAVE_CONNECT_LIST_CHILD";

/// Run the `child_*` test `name` in a fresh process and return its stdout's
/// autosave lines, which an in-process listener cannot hear.
fn console_of(name: &str) -> Vec<String> {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{name}: {out:?}");
    let prefix = format!("test {name} ... ");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        // libtest's own line runs on into the first print.
        .map(|l| l.strip_prefix(prefix.as_str()).unwrap_or(l))
        .filter(|l| l.contains("save_restore") || l.contains("PV's connected"))
        .map(str::to_string)
        .collect()
}

/// Start a manual set of `pvs` on a database holding only `TEMP`, and stop
/// after the first pass.
async fn first_pass(pvs: &[&str]) {
    first_pass_of(false, pvs).await;
}

/// [`first_pass`], before `iocInit` when `loading`.
async fn first_pass_of(loading: bool, pvs: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(PvDatabase::new());
    if loading {
        db.begin_load().unwrap();
    }
    db.add_record("TEMP", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let cfg = SaveSetConfig {
        name: "auto_settings".into(),
        save_path: dir.path().join("auto_settings.sav"),
        request_file: None,
        request_pvs: pvs.iter().map(|p| p.to_string()).collect(),
        backup: BackupConfig::default(),
        macros: HashMap::new(),
        macro_string: None,
        search_paths: Vec::new(),
    };
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .add_set(cfg, SaveStrategy::Manual)
            .build()
            .await,
    );
    // Detached; `shutdown` ends the task.
    drop(mgr.clone().start(db.clone()));
    let set = mgr.sets()[0].0.clone();
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        if set.list_status().text == "Enabling list..." {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }
    mgr.shutdown();
}

#[epics_macros_rs::epics_test]
async fn child_some_members_missing() {
    if std::env::var_os(CHILD).is_some() {
        first_pass(&["TEMP", "GONE1", "GONE2"]).await;
    }
}

#[epics_macros_rs::epics_test]
async fn child_every_member_present() {
    if std::env::var_os(CHILD).is_some() {
        first_pass(&["TEMP"]).await;
    }
}

#[epics_macros_rs::epics_test]
async fn child_a_member_listed_twice() {
    if std::env::var_os(CHILD).is_some() {
        first_pass(&["TEMP", "TEMP"]).await;
    }
}

#[epics_macros_rs::epics_test]
async fn child_made_before_ioc_init() {
    if std::env::var_os(CHILD).is_some() {
        first_pass_of(true, &["TEMP", "GONE1"]).await;
    }
}

#[test]
fn a_list_with_missing_members_names_each_one() {
    assert_eq!(
        console_of("child_some_members_missing"),
        [
            "save_restore: connect failed for channel 'GONE1'",
            "save_restore: connect failed for channel 'GONE2'",
            "auto_settings.sav: 1 of 3 PV's connected",
        ]
    );
}

#[test]
fn a_list_that_connects_says_only_how_many() {
    assert_eq!(
        console_of("child_every_member_present"),
        ["auto_settings.sav: 1 of 1 PV's connected"]
    );
}

/// C `readReqFile` appends every PV line (save_restore.c:3650-3672) and
/// `connect_list` counts each channel it made.
#[test]
fn a_member_listed_twice_is_two_channels() {
    assert_eq!(
        console_of("child_a_member_listed_twice"),
        ["auto_settings.sav: 2 of 2 PV's connected"]
    );
}

#[test]
fn a_list_made_before_ioc_init_connects_nothing_first() {
    assert_eq!(
        console_of("child_made_before_ioc_init"),
        [
            "save_restore: connect failed for channel 'TEMP'",
            "save_restore: connect failed for channel 'GONE1'",
            "auto_settings.sav: 0 of 2 PV's connected",
        ]
    );
}

/// A manual list of `TEMP` made before `iocInit`, with `settings`, its
/// task started.
async fn early_list(
    settings: Arc<SrSettings>,
) -> (
    Arc<AutosaveManager>,
    Option<SaveTaskHandle>,
    tempfile::TempDir,
) {
    let (mgr, task, _db, dir) = early_list_of(settings, &["TEMP"]).await;
    (mgr, task, dir)
}

/// [`early_list`] of `pvs`, with its database.
async fn early_list_of(
    settings: Arc<SrSettings>,
    pvs: &[&str],
) -> (
    Arc<AutosaveManager>,
    Option<SaveTaskHandle>,
    Arc<PvDatabase>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(PvDatabase::new());
    db.begin_load().unwrap();
    db.add_record("TEMP", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let cfg = SaveSetConfig {
        name: "auto_settings".into(),
        save_path: dir.path().join("auto_settings.sav"),
        request_file: None,
        request_pvs: pvs.iter().map(|p| p.to_string()).collect(),
        backup: BackupConfig::default(),
        macros: HashMap::new(),
        macro_string: None,
        search_paths: Vec::new(),
    };
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .settings(settings)
            .add_set(cfg, SaveStrategy::Manual)
            .build()
            .await,
    );
    let task = mgr.clone().start(db.clone());
    (mgr, task, db, dir)
}

/// C's first pass counts each member of a list made before `iocInit` as
/// not connected (save_restore.c:1374), present or not.
#[epics_macros_rs::epics_test]
async fn a_list_made_before_ioc_init_counts_every_member_unconnected() {
    let (mgr, task, _dir) = early_list(Arc::default()).await;
    let set = mgr.sets()[0].0.clone();
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        if set.not_connected() > 0 {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
    }
    mgr.shutdown();
    drop(task);
    assert_eq!(set.not_connected(), 1);
}

/// [`early_list`] with `LATE`, made after the first pass and `iocInit`,
/// through the CA reconnect a minute later (:958-963), in the child only.
#[epics_macros_rs::epics_test]
async fn child_reconnect() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let settings = Arc::new(SrSettings::default());
    settings.set_ca_reconnect(1);
    settings
        .debug
        .store(10, std::sync::atomic::Ordering::Relaxed);
    let (mgr, task, db, _dir) = early_list_of(settings, &["TEMP", "LATE"]).await;
    let set = mgr.sets()[0].0.clone();
    let wait_for = async |n: i32, within: Duration| {
        let deadline = std::time::Instant::now() + within;
        while set.not_connected() != n && std::time::Instant::now() < deadline {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
        }
        set.not_connected()
    };
    assert_eq!(
        wait_for(2, fact_budget::FACT_BUDGET).await,
        2,
        "the first pass"
    );
    db.ioc_init().await;
    db.add_record("LATE", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    assert_eq!(
        wait_for(0, Duration::from_secs(75)).await,
        0,
        "the reconnect pass"
    );
    mgr.shutdown();
    drop(task);
}

/// A search before `iocInit` finds nothing. Each member then connects
/// through its first channel's callback, so the reconnect a minute later
/// traces both and searches neither (save_restore.c:1290), as the C
/// oracle's did for a member whose server started after the search.
#[test]
fn a_pre_ioc_init_list_reconnects_after_its_first_pass() {
    let lines = console_of("child_reconnect");
    let reconnect: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .skip_while(|l| !l.ends_with("PV's connected"))
        .filter(|l| l.starts_with("save_restore:connect_list:"))
        .collect();
    assert_eq!(
        reconnect,
        [
            "save_restore:connect_list: channel 'TEMP'",
            "save_restore:connect_list: channel 'LATE'",
        ],
        "{lines:?}"
    );
}

/// An IOC whose st.cmd makes the set before `iocInit`, in the child only.
#[epics_macros_rs::epics_test]
async fn child_ioc() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.db"), r#"record(ao, "TEMP") {}"#).unwrap();
    std::fs::write(d.join("auto_settings.req"), "TEMP\n").unwrap();
    std::fs::write(
        d.join("st.cmd"),
        format!(
            "dbLoadRecords(\"{d}/a.db\")\n\
             set_savefile_path(\"{d}\")\n\
             set_requestfile_path(\"{d}\")\n\
             create_manual_set(\"auto_settings.req\")\n\
             iocInit\n",
            d = d.display()
        ),
    )
    .unwrap();
    let outcome = IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(d.join("st.cmd").to_str().unwrap())
        .run_outcome(|config| async move {
            let mgr = config.autosave_manager.unwrap();
            let set = mgr.sets()[0].0.clone();
            let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
            while std::time::Instant::now() < deadline {
                if set.not_connected() > 0 {
                    break;
                }
                epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
            }
            mgr.shutdown();
            Ok(())
        })
        .await
        .unwrap();
    // A set that never saw `iocInit` connects nothing either.
    assert_eq!(outcome, RunOutcome::Served);
}

/// The st.cmd path makes its sets before `iocInit` too.
#[test]
fn a_set_made_in_st_cmd_connects_nothing_first() {
    let lines = console_of("child_ioc");
    assert!(
        lines.contains(&"auto_settings.sav: 0 of 1 PV's connected".to_string()),
        "{lines:?}"
    );
}
