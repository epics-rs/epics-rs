//! C's manual restore puts a scalar value as its text, `ca_put(DBR_STRING)`
//! of `value_string` (save_restore.c:3362-3371), so the field's own string
//! conversion reads it: a number's leading digits, an empty text as 0, a
//! menu or state name. The expected lines are the C oracle IOC's for the
//! same records, file and script.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_MANUAL_SCALAR_PUT_SCRIPT";

const DB: &str = r#"
record(ao, "X1") { field(VAL, "3") }
record(ao, "X2") { field(VAL, "3") }
record(waveform, "N3") { field(FTVL, "DOUBLE") field(NELM, "1") }
record(mbbo, "M") { field(ZRST, "zero") field(ONST, "one") field(VAL, "0") }
record(ai, "A") { field(VAL, "3") }
record(longout, "L") { field(VAL, "9") }
record(longout, "L2") { field(VAL, "9") }
record(stringin, "S") { field(VAL, "init") }
record(ao, "X3") { field(VAL, "3") }
record(bo, "B") { field(ZNAM, "off") field(ONAM, "on") field(VAL, "0") }
"#;

const SAV: &str = "# save/restore V5.x\tAutomatically generated - DO NOT MODIFY - 261020-120000\n\
X1 5abc\n\
X2 \n\
N3 5abc\n\
M one\n\
A.SCAN 1 second\n\
L 0x10\n\
L2 12abc\n\
S  two  spaces \n\
X3 1e400\n\
B on\n\
<END>\n";

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
fn a_manual_scalar_restore_puts_the_text() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.db"), DB).unwrap();
    std::fs::write(d.join("x.req"), "X1\n").unwrap();
    std::fs::write(d.join("m.sav"), SAV).unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         iocInit\n\
         create_manual_set(\"x.req\")\n\
         epicsThreadSleep 1\n\
         fdbrestore(\"m.sav\")\n\
         epicsThreadSleep 2\n\
         echo ==manual\n\
         dbgf X1\n\
         dbgf X2\n\
         dbgf N3\n\
         dbgf M\n\
         dbgf A.SCAN\n\
         dbgf L\n\
         dbgf L2\n\
         dbgf S\n\
         dbgf X3\n\
         dbgf B\n\
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
        .filter(|l| l.starts_with("==") || l.starts_with("DBF_") || l.contains("manual_restore"))
        .map(|l| l.trim_end().to_string())
        .collect();
    assert_eq!(
        stdout,
        [
            "save_restore:do_manual_restore: ca_put of 1e400 to X3 failed",
            "==manual",
            "DBF_DOUBLE:         5",
            "DBF_DOUBLE:         0",
            "DBF_DOUBLE:         5",
            "DBF_STRING:         \"one\"",
            "DBF_STRING:         \"1 second\"",
            "DBF_LONG:           16 = 0x10",
            "DBF_LONG:           12 = 0xc",
            "DBF_STRING:         \" two  spaces \"",
            "DBF_DOUBLE:         3",
            "DBF_STRING:         \"on\"",
            "==end",
        ],
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
