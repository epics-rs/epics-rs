//! `create_data_set`'s traces ahead of the request file (save_restore.c:
//! 2457-2460): the save file's `concatenate_paths` line, then `readReqFile`
//! naming the `macrostring` it was given, `NULL` when the command had
//! none. The expected lines are the C oracle IOC's for the same script.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_CREATE_TRACES_DIR";

#[epics_macros_rs::epics_test]
async fn child_ioc() {
    let Some(dir) = std::env::var_os(CHILD).map(std::path::PathBuf::from) else {
        return;
    };
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(dir.join("st.cmd").to_str().unwrap())
        .run(|config| async move {
            epics_base_rs::server::db_server::announce_serving("probe");
            epics_base_rs::runtime::task::sleep(Duration::from_secs(2)).await;
            if let Some(mgr) = config.autosave_manager {
                mgr.shutdown();
            }
            Ok(())
        })
        .await
        .unwrap();
}

#[test]
fn a_create_traces_the_save_file_then_its_macrostring() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.db"), "record(ao, \"A\") {}\n").unwrap();
    for req in ["s.req", "t.req", "u.req"] {
        std::fs::write(d.join(req), "A\nfile i.req\n").unwrap();
    }
    std::fs::write(d.join("i.req"), "A\n").unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}\")\n\
         save_restoreSet_Debug(3)\n\
         echo ==s\n\
         create_manual_set(\"s.req\")\n\
         echo ==t\n\
         create_manual_set(\"t.req\", \"\")\n\
         echo ==u\n\
         create_monitor_set(\"u.req\", 5)\n\
         echo ==init\n\
         iocInit\n\
         echo ==reload\n\
         reload_manual_set(\"s.req\", \"\")\n\
         epicsThreadSleep(1)\n\
         echo ==end\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_ioc", "--nocapture", "--test-threads=1"])
        .env(CHILD, d)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.strip_prefix("test child_ioc ... ").unwrap_or(l))
        .filter(|l| {
            l.starts_with("==")
                || l.contains("concatenate_paths")
                || l.contains("readReqFile: entry")
                || l.contains("readReqFile: calling")
        })
        .map(|l| {
            let l = l.replace(&d.display().to_string(), "DIR");
            match (
                l.find("0x"),
                l.find("0x").and_then(|at| l[at..].find([',', '\''])),
            ) {
                (Some(at), Some(len)) => format!("{}ADDR{}", &l[..at], &l[at + len..]),
                _ => l,
            }
        })
        .collect();
    let set = |name: &str, macrostring: &str| {
        [
            format!("save_restore:concatenate_paths: dest='DIR/{name}.sav'"),
            format!(
                "save_restore:readReqFile: entry: reqFile='{name}.req', plist=ADDR, macrostring='{macrostring}'"
            ),
            format!("save_restore:concatenate_paths: dest='DIR/{name}.req'"),
            "save_restore:readReqFile: calling readReqFile('i.req', ADDR,'')".into(),
            "save_restore:readReqFile: entry: reqFile='i.req', plist=ADDR, macrostring=''".into(),
            "save_restore:concatenate_paths: dest='DIR/i.req'".into(),
        ]
    };
    let mut expected = vec!["==s".to_string()];
    expected.extend(set("s", "NULL"));
    expected.push("==t".into());
    expected.extend(set("t", ""));
    expected.push("==u".into());
    expected.extend(set("u", "NULL"));
    expected.extend(["==init".into(), "==reload".into()]);
    expected.extend(set("s", ""));
    expected.push("==end".into());
    assert_eq!(lines, expected);
}

/// A macrostring with no definition still gives C a `pairs` array, its
/// first entry NULL, which glibc prints `(null)` (save_restore.c:3595-3599).
#[test]
fn a_macrostring_with_no_definition_traces_a_null_first_pair() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.db"), "record(ao, \"A\") {}\n").unwrap();
    std::fs::write(d.join("s.req"), "A\n").unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}\")\n\
         save_restoreSet_Debug(6)\n\
         create_manual_set(\"s.req\", \",\")\n\
         save_restoreSet_Debug(0)\n\
         iocInit\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_ioc", "--nocapture", "--test-threads=1"])
        .env(CHILD, d)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<String> = stdout
        .lines()
        .skip_while(|l| !l.ends_with("readReqFile: Current macro definitions:"))
        .take(7)
        .map(|l| match l.split_once("handle=0x") {
            Some((head, _)) => format!("{head}handle=ADDR"),
            None => l.to_string(),
        })
        .collect();
    assert_eq!(
        lines,
        [
            "save_restore:readReqFile: Current macro definitions:",
            "e name             rawval           value",
            "- ----             ------           -----",
            "save_restore:readReqFile: --------------------------",
            "save_restore:readReqFile:handle=ADDR",
            "save_restore:readReqFile:pairs[0]='(null)'",
            "save_restore:readReqFile: line='A",
        ],
        "{stdout}"
    );
}
