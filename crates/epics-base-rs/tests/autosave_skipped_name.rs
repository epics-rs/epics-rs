//! A line whose name C neither restores nor comments is skipped before its
//! value is read (dbrestore.c:891-1016, save_restore.c:3326-3449), so an
//! `@array@` value on it does not read on into the lines after it. The
//! boot restore takes a letter or digit first, refuses a name of 80
//! characters and ends at a `<`; the manual one takes `isValid1stPVChar`.
//! The expected lines are the C oracle IOC's for the same records, files
//! and script.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_SKIPPED_NAME_SCRIPT";

const DB: &str = r#"
record(ao, "A") { field(VAL, "3") }
record(ao, "B") { field(VAL, "3") }
record(ao, "C") { field(VAL, "3") }
record(ao, "D") { field(VAL, "3") }
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
fn a_skipped_name_does_not_read_on() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let long = "Q".repeat(85);
    std::fs::write(d.join("a.db"), DB).unwrap();
    std::fs::write(d.join("x.req"), "A\n").unwrap();
    std::fs::write(
        d.join("b.sav"),
        format!(
            "{HEADER}_X @array@ {{ \"1\"\nA 5\n{long} @array@ {{ \"1\"\nB 6\n\
             .Y @array@ {{ \"1\"\nC 7\n<x @array@ {{ \"1\"\nD 8\n<END>\n"
        ),
    )
    .unwrap();
    std::fs::write(
        d.join("m.sav"),
        format!("{HEADER}.X @array@ {{ \"1\"\nA 5\n$Y @array@ {{ \"1\"\nB 6\n<END>\n"),
    )
    .unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_requestfile_path(\"{d}\")\n\
         set_savefile_path(\"{d}\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         set_pass1_restoreFile(\"b.sav\")\n\
         iocInit\n\
         echo ==boot\n\
         dbgf A\n\
         dbgf B\n\
         dbgf C\n\
         dbgf D\n\
         create_manual_set(\"x.req\")\n\
         epicsThreadSleep 1\n\
         dbpf A 3\n\
         dbpf B 3\n\
         echo ==manual\n\
         fdbrestore(\"m.sav\")\n\
         epicsThreadSleep 2\n\
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
    let stderr: Vec<String> = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| l.contains("dbrestore.c:") || l.contains("save_restore.c:"))
        .map(str::to_string)
        .collect();
    let mut expected = vec!["==boot"];
    expected.extend(["DBF_DOUBLE:         5", "DBF_DOUBLE:         6"]);
    expected.extend(["DBF_DOUBLE:         7", "DBF_DOUBLE:         3"]);
    expected.extend(["DBF_DOUBLE:         3", "DBF_DOUBLE:         3", "==manual"]);
    expected.extend(["DBF_DOUBLE:         5", "DBF_DOUBLE:         6", "==end"]);
    assert_eq!(stdout, expected, "{stderr:?}");
    assert_eq!(
        stderr,
        [format!(
            "../dbrestore.c:reboot_restore '{}' is too long to be a PV name.",
            &long[..80]
        )]
    );
}
