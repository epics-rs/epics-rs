//! The manual restore unescapes a scalar value and cuts it to 40 bytes
//! before its `DBR_STRING` put (save_restore.c:3362-3363), so 1, 39 zeros
//! and 1 restores 1e+39, 39 zeros then `12` restores 1, `a\"b\\c` restores
//! `a"b\c` and `\x31\x32` restores 12. The expected lines are the C oracle
//! IOC's for the same records, file and script.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_MANUAL_SCALAR_TEXT_SCRIPT";

const DB: &str = r#"
record(ao, "X") { field(VAL, "3") }
record(ao, "Y") { field(VAL, "3") }
record(stringin, "S") { field(VAL, "init") }
record(longout, "L") { field(VAL, "9") }
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
fn a_scalar_is_unescaped_then_cut_to_40_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let zeros = "0".repeat(39);
    std::fs::write(d.join("a.db"), DB).unwrap();
    std::fs::write(d.join("x.req"), "X\n").unwrap();
    std::fs::write(
        d.join("m.sav"),
        format!("{HEADER}X 1{zeros}1\nY {zeros}12\nS a\\\"b\\\\c\nL \\x31\\x32\n<END>\n"),
    )
    .unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         iocInit\n\
         create_manual_set(\"x.req\")\n\
         epicsThreadSleep 1\n\
         echo ==manual\n\
         fdbrestore(\"m.sav\")\n\
         epicsThreadSleep 2\n\
         dbgf X\n\
         dbgf Y\n\
         dbgf S\n\
         dbgf L\n\
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
            "==manual",
            "DBF_DOUBLE:         1e+39",
            "DBF_DOUBLE:         1",
            "DBF_STRING:         \"a\\\"b\\\\c\"",
            "DBF_LONG:           12 = 0xc",
            "==end",
        ],
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
