//! The boot restore's `%80s` name is refused at 80 characters and taken at
//! 79 (dbrestore.c:889, :906-910): the 79-character name is looked up, not
//! found, and its `@array@` value read on to its `}`; the 80-character one
//! is dropped before its value is read. The expected lines are the C oracle
//! IOC's for the same records, file and script.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_BOOT_NAME_LENGTH_SCRIPT";

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
fn a_name_is_too_long_from_80_characters() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    // With a field, so C appends no `.VAL`.
    let n79 = format!("{}.VAL", "Q".repeat(75));
    let n80 = format!("{}.VAL", "Q".repeat(76));
    std::fs::write(d.join("a.db"), DB).unwrap();
    std::fs::write(
        d.join("b.sav"),
        format!(
            "{HEADER}{n79} @array@ {{ \"1\"\nA 5\n\"2\" }}\n\
             {n80} @array@ {{ \"1\"\nB 6\n\"2\" }}\n<END>\n"
        ),
    )
    .unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_savefile_path(\"{d}\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         set_pass1_restoreFile(\"b.sav\")\n\
         iocInit\n\
         echo ==boot\n\
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
        .filter(|l| l.contains("dbrestore.c:"))
        .map(str::to_string)
        .collect();
    assert_eq!(
        stdout,
        [
            "==boot",
            "DBF_DOUBLE:         3",
            "DBF_DOUBLE:         6",
            "==end"
        ],
        "{stderr:?}"
    );
    assert_eq!(
        stderr,
        [
            format!("../dbrestore.c:reboot_restore dbFindRecord for '{n79}' failed"),
            format!("../dbrestore.c:reboot_restore '{n80}' is too long to be a PV name."),
        ]
    );
}
