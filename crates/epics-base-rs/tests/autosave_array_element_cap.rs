//! An `@array@` element keeps its first 39 characters and skips the rest to
//! its closing quote (`SR_array_restore`, dbrestore.c:571;
//! `manual_array_restore`, save_restore.c:3045), so 39 zeros then `12` restores
//! 0. The expected lines are the C oracle IOC's for the same record, files
//! and script.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_ARRAY_ELEMENT_CAP_SCRIPT";

const DB: &str = r#"
record(waveform, "W") { field(FTVL, "DOUBLE") field(NELM, "4") }
"#;

const HEADER: &str =
    "# save/restore V5.x\tAutomatically generated - DO NOT MODIFY - 261020-120000\n";

/// The st.cmd, in the child only.
#[epics_macros_rs::epics_test]
async fn child_script() {
    let Some(script) = std::env::var_os(CHILD) else {
        return;
    };
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(script.to_str().unwrap())
        .run(|config| async move {
            if let Some(mgr) = config.autosave_manager {
                mgr.shutdown();
            }
            Ok(())
        })
        .await
        .unwrap();
}

#[test]
fn an_element_keeps_its_first_39_characters() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let sav = format!(
        "{HEADER}W @array@ {{ \"{}12\" \"7\" }}\n<END>\n",
        "0".repeat(39)
    );
    std::fs::write(d.join("a.db"), DB).unwrap();
    std::fs::write(d.join("x.req"), "W\n").unwrap();
    std::fs::write(d.join("b.sav"), &sav).unwrap();
    std::fs::write(d.join("m.sav"), &sav).unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         set_pass1_restoreFile(\"b.sav\")\n\
         iocInit\n\
         echo ==boot\n\
         dbgf W\n\
         create_manual_set(\"x.req\")\n\
         epicsThreadSleep 1\n\
         dbpf W 9\n\
         echo ==manual\n\
         fdbrestore(\"m.sav\")\n\
         epicsThreadSleep 2\n\
         dbgf W\n\
         echo ==end\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_script", "--nocapture", "--test-threads=1"])
        .env(CHILD, d.join("st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.strip_prefix("test child_script ... ").unwrap_or(l))
        .filter(|l| l.starts_with("==") || l.starts_with("DBF_"))
        .map(|l| l.trim_end().to_string())
        .collect();
    assert_eq!(
        stdout,
        [
            "==boot",
            "DBF_DOUBLE[2]:      0         7",
            "DBF_DOUBLE:         9",
            "==manual",
            "DBF_DOUBLE[2]:      0         7",
            "==end",
        ],
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
