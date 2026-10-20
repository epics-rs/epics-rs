//! C `readReqFile`, `do_manual_restore` and `reboot_restore` each make one
//! macLib handle and expand every line through it (save_restore.c:3298,
//! :3563, dbrestore.c:841), so a faulty definition is reported once per
//! call, and a faulty reference once per line, naming the whole line read.
//! The expected lines are the C oracle IOC's for the same scripts.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

mod autosave_wait;

const CHILD: &str = "AUTOSAVE_MACRO_HANDLE_DIR";

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
        // Returning ends the IOC, so it waits for the script's last put.
        .run(|config| async move {
            epics_base_rs::server::db_server::announce_serving("probe");
            for _ in 0..200 {
                if config.db.get_pv("P:DONE").is_ok_and(|v| v.text_g() == "1") {
                    break;
                }
                epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
            }
            Ok(())
        })
        .await
        .unwrap();
}

/// Runs `body` after the common head and returns (stdout, stderr).
fn run(d: &Path, body: &str) -> (String, String) {
    std::fs::create_dir_all(d.join("sav")).unwrap();
    std::fs::write(
        d.join("a.db"),
        "record(ao, \"A\") { field(VAL, \"3\") }\n\
         record(stringout, \"N:name\") { field(VAL, \"named.sav\") }\n\
         record(stringout, \"N:path\") { field(VAL, \"sub\") }\n\
         record(bo, \"P:DONE\") {}\n",
    )
    .unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         {body}\
         dbpf P:DONE 1\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_ioc", "--nocapture", "--test-threads=1"])
        .env(CHILD, d)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `stderr`'s macLib notices, a reference's split by the line it names.
fn notices(stderr: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut lines = stderr.lines();
    while let Some(l) = lines.next() {
        if l.starts_with("macLib:") {
            found.push(l);
            if l.contains("(expanding string") {
                found.push(lines.next().unwrap_or_default());
            }
        }
    }
    found
}

#[test]
fn each_call_reports_a_faulty_definition_once() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let sav = "# Autosave release 6.1\tAutomatically generated - DO NOT MODIFY - 261020-095752\n\
               A 4\nN:path sub\n<END>\n";
    std::fs::create_dir_all(d.join("sav")).unwrap();
    std::fs::write(d.join("sav/r.sav"), sav).unwrap();
    std::fs::write(d.join("sav/f.sav"), sav).unwrap();
    for (req, text) in [
        ("inc", "A\n"),
        ("j", "A\nN:name\nN:path\n"),
        ("k", "$(Q)A\n"),
        ("l", "file $(Q)inc.req\n"),
    ] {
        std::fs::write(d.join(format!("{req}.req")), text).unwrap();
    }
    let (stdout, stderr) = run(
        d,
        "set_pass0_restoreFile(\"r.sav\", 'X=$(Q)')\n\
         iocInit\n\
         create_manual_set(\"j.req\", 'X=$(Q)')\n\
         wait_ops_served\n\
         create_manual_set(\"k.req\", 'P=x')\n\
         wait_ops_served\n\
         create_manual_set(\"l.req\", 'P=x')\n\
         wait_ops_served\n\
         fdbrestoreX(\"f.sav\", 'X=$(Q)')\n\
         wait_ops_served\n",
    );
    assert_eq!(
        notices(&stderr),
        [
            "macLib: macro Q is undefined (expanding macro X)",
            "macLib: macro Q is undefined (expanding macro X)",
            "macLib: macro Q is undefined (expanding string $(Q)A",
            ")",
            "macLib: macro Q is undefined (expanding string file $(Q)inc.req",
            ")",
            "macLib: macro Q is undefined (expanding macro X)",
        ],
        "{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains(
            "save_restore:readReqFile: unable to open file $(Q,undefined)inc.req. Exiting."
        ),
        "{stdout}"
    );
}

#[test]
fn a_handle_reports_and_traces_its_expanded_values() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("k.req"), "$(Q)A\n").unwrap();
    let (stdout, stderr) = run(
        d,
        "iocInit\n\
         save_restoreSet_Debug(5)\n\
         create_manual_set(\"k.req\", 'B=$(Q),A=$(B)x')\n\
         wait_ops_served\n\
         save_restoreSet_Debug(0)\n",
    );
    assert_eq!(
        notices(&stderr),
        [
            "macLib: macro Q is undefined (expanding macro B)",
            "macLib: macro Q is undefined (expanding macro A)",
            "macLib: macro Q is undefined (expanding string $(Q)A",
            ")",
        ],
        "{stdout}\n{stderr}"
    );
    let trace: Vec<&str> = stdout
        .lines()
        .skip_while(|l| *l != "save_restore:readReqFile: Current macro definitions:")
        .filter(|l| !l.starts_with("save_restore:save_restore: entry"))
        .take(9)
        .collect();
    assert_eq!(
        trace,
        [
            "save_restore:readReqFile: Current macro definitions:",
            "e name             rawval           value",
            "- ----             ------           -----",
            "* B                $(Q)             $(Q,undefined)",
            "* A                $(B)x            $(Q,undefined)x",
            "save_restore:readReqFile: --------------------------",
            "save_restore:readReqFile: line='$(Q)A",
            "', eline='$(Q,undefined)A",
            "', name='$(Q,undefined)A'",
        ],
        "{stdout}"
    );
}
