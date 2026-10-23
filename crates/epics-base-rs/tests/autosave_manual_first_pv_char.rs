//! The manual restore takes a name whose first character passes
//! `isValid1stPVChar` (save_restore.c:495-499, :3326), so an `@array@` value
//! on `_X` through `<X` reads on to its `}` and swallows the `A 5` inside.
//! The expected lines are the C oracle IOC's for the same records, file and
//! script.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_MANUAL_FIRST_PV_CHAR_SCRIPT";

const DB: &str = r#"
record(ao, "A") { field(VAL, "3") }
record(ao, "B") { field(VAL, "3") }
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
fn a_valid_first_character_reads_its_array_on() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut sav = HEADER.to_string();
    for c in ['_', '-', '+', ':', '[', ']', '>', ';', '<'] {
        sav.push_str(&format!("{c}X @array@ {{ \"1\"\nA 5\n\"2\" }}\n"));
    }
    sav.push_str("B 6\n<END>\n");
    std::fs::write(d.join("a.db"), DB).unwrap();
    std::fs::write(d.join("x.req"), "A\n").unwrap();
    std::fs::write(d.join("m.sav"), sav).unwrap();
    // C's nine channels each wait out ca_pend_io(0.5).
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
         epicsThreadSleep 8\n\
         dbgf A\n\
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
        .filter(|l| l.starts_with("==") || l.starts_with("DBF_"))
        .map(|l| l.trim_end().to_string())
        .collect();
    assert_eq!(
        stdout,
        [
            "==manual",
            "DBF_DOUBLE:         3",
            "DBF_DOUBLE:         6",
            "==end"
        ],
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
