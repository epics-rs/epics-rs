//! C's save_restore task exists from the first `create_*_set`
//! (save_restore.c:2335-2351), and before `iocInit` none of its channels to
//! the IOC's own records connects. The C oracle IOC, for a manual set, two
//! seconds for the task's first pass, and an `fdbrestore("s.sav")` of
//! `A 1.5` before `iocInit`, with A at 3:
//!
//! ```text
//! save_restore: connect failed for channel 'A'
//! s.sav: 0 of 1 PV's connected
//! save_restore:fdbrestore:entry
//! save_restore task: calling do_manual_restore('s.sav')
//! fopen_and_check: checkFile returned 0x740e3c002660
//! save_restore:do_manual_restore: ca_search for A timeout
//! ```
//!
//! and A stays 3. With no set the task never runs, so nothing writes the
//! SR_* status records (save_restore.c:796-830): they stay UDF.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

mod autosave_wait;

const CHILD: &str = "AUTOSAVE_PRE_IOC_INIT_TASK_DIR";

const STATUS_DB: &str = r#"
record(bo, "P:SR_heartbeat") {}
record(mbbo, "P:SR_rebootStatus") {}
record(mbbo, "P:SR_status") {}
record(stringout, "P:SR_recentlyStr") {}
record(stringout, "P:SR_rebootStatusStr") {}
record(stringout, "P:SR_rebootTime") {}
record(stringout, "P:SR_statusStr") {}
"#;

/// Run `st.cmd` in `$CHILD`; after `iocInit` and two seconds, print each
/// record's `VAL` and `UDF` as `name=VAL/UDF`.
#[epics_macros_rs::epics_test]
async fn child_ioc() {
    let Some(dir) = std::env::var_os(CHILD).map(std::path::PathBuf::from) else {
        return;
    };
    let holder = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
    IocApplication::new()
        .port(0)
        .autosave_startup(holder.clone())
        .register_startup_command(autosave_wait::wait_ops_served(holder))
        .startup_script(dir.join("st.cmd").to_str().unwrap())
        .run(|config| async move {
            epics_base_rs::server::db_server::announce_serving("probe");
            epics_base_rs::runtime::task::sleep(Duration::from_secs(2)).await;
            let mut names = config.db.all_record_names();
            names.sort();
            for name in names {
                let val = config.db.get_pv(&name).unwrap();
                let udf = config.db.get_pv(format!("{name}.UDF")).unwrap();
                println!("{name}={val}/{udf}");
            }
            if let Some(mgr) = config.autosave_manager {
                mgr.shutdown();
            }
            Ok(())
        })
        .await
        .unwrap();
}

/// The child's stdout for `script` over `db`, from the first line naming
/// save_restore.
fn run(db: &str, files: &[(&str, &str)], script: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.db"), db).unwrap();
    for (name, text) in files {
        std::fs::write(d.join(name), text).unwrap();
    }
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}\")\n\
         {script}",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_ioc", "--nocapture", "--test-threads=1"])
        .env(CHILD, d)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.strip_prefix("test child_ioc ... ").unwrap_or(l))
        // The shell's own echo of each line is bold.
        .filter(|l| !l.starts_with('\u{1b}'))
        .filter(|l| {
            l.contains("save_restore")
                || l.contains("PV's connected")
                || l.contains("checkFile")
                || l.starts_with("==")
                || l.contains('=')
        })
        .map(str::to_string)
        .collect()
}

#[test]
fn a_restore_before_ioc_init_connects_nothing() {
    // Nothing gates the task's first pass (save_restore.c:935-956); the
    // first wait puts it before `fdbrestore`, the second the restore before
    // the marker. The C oracle slept two seconds for each.
    let lines = run(
        "record(ao, \"A\") { field(VAL, \"3\") }\n",
        &[("s.req", "A\n"), ("s.sav", "# x\nA 1.5\n<END>\n")],
        "save_restoreSet_Debug(1)\n\
         create_manual_set(\"s.req\")\n\
         wait_ops_served\n\
         fdbrestore(\"s.sav\")\n\
         wait_ops_served\n\
         echo ==before-iocInit\n\
         iocInit\n",
    );
    let before: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .take_while(|l| *l != "==before-iocInit")
        .map(
            |l| match l.starts_with("fopen_and_check: checkFile returned ") {
                true => "fopen_and_check: checkFile returned <FILE>",
                false => l,
            },
        )
        .collect();
    assert_eq!(
        before,
        [
            "save_restore: connect failed for channel 'A'",
            "s.sav: 0 of 1 PV's connected",
            "save_restore:fdbrestore:entry",
            "save_restore task: calling do_manual_restore('s.sav')",
            "fopen_and_check: checkFile returned <FILE>",
            "save_restore:do_manual_restore: ca_search for A timeout",
        ],
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l == "A=3/0"), "{lines:?}");
}

#[test]
fn with_no_set_the_status_records_stay_udf() {
    let lines = run(
        STATUS_DB,
        &[],
        "save_restoreSet_status_prefix(\"P:\")\n\
         iocInit\n",
    );
    let status: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|l| l.starts_with("P:"))
        .collect();
    assert_eq!(
        status,
        [
            "P:SR_heartbeat=0/1",
            "P:SR_rebootStatus=0/1",
            "P:SR_rebootStatusStr=/1",
            "P:SR_rebootTime=/1",
            "P:SR_recentlyStr=/1",
            "P:SR_status=0/1",
            "P:SR_statusStr=/1",
        ]
    );
}

/// The reboot status is put once, as the task starts (save_restore.c:874-
/// 877), so a task started before `iocInit` never puts it; the pass's
/// status is put once its channels connect.
#[test]
fn a_task_started_before_ioc_init_puts_no_reboot_status() {
    let lines = run(
        STATUS_DB,
        &[("s.req", "P:SR_heartbeat\n")],
        "save_restoreSet_status_prefix(\"P:\")\n\
         create_manual_set(\"s.req\")\n\
         iocInit\n",
    );
    let line = |name: &str| {
        lines
            .iter()
            .find(|l| l.starts_with(&format!("P:{name}=")))
            .cloned()
            .unwrap_or_default()
    };
    assert_eq!(line("SR_rebootTime"), "P:SR_rebootTime=/1");
    assert_eq!(line("SR_rebootStatusStr"), "P:SR_rebootStatusStr=/1");
    assert_eq!(line("SR_statusStr"), "P:SR_statusStr=Enabling list.../0");
    let console: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|l| l.contains("save_restore") || l.contains("PV's connected"))
        .take(4)
        .collect();
    assert_eq!(
        console,
        [
            "save_restore: Can't connect to all status PV(s)",
            "save_restore: connect failed for channel 'P:SR_heartbeat'",
            "s.sav: 0 of 1 PV's connected",
            "save_restore: Can't connect to status PV(s) for list 's.sav'",
        ]
    );
}
