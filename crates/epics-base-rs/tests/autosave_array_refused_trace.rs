//! A pass-1 `@array@` restore that the type switch refuses writes nothing
//! and says so at debug 1, as one the file ended inside does: C's test is
//! `!status && p_data` (dbrestore.c:725-731), not end of file. The expected
//! lines are the C oracle IOC's for the same record, file and script, with
//! the `p_data` address cut.

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_ARRAY_REFUSED_TRACE_SCRIPT";

const SAV: &str = "# save/restore V5.x\tAutomatically generated - DO NOT MODIFY - 261020-120000\n\
A.DTYP @array@ { \"1\" }\n\
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
fn a_refused_array_is_reported_unwritten() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.db"), "record(ao, \"A\") { field(VAL, \"3\") }\n").unwrap();
    std::fs::write(d.join("b.sav"), SAV).unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_savefile_path(\"{d}\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         set_pass1_restoreFile(\"b.sav\")\n\
         save_restoreSet_Debug(1)\n\
         iocInit\n\
         save_restoreSet_Debug(0)\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_script", "--nocapture", "--test-threads=1"])
        .env(CHILD, d.join("st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let dtyp: Vec<&str> = stderr
        .lines()
        .skip_while(|l| !l.ends_with("PV = 'A.DTYP'"))
        .take(6)
        .map(|l| l.split(", p_data=0x").next().unwrap())
        .collect();
    assert_eq!(
        dtyp,
        [
            "../dbrestore.c:SR_array_restore :entry: PV = 'A.DTYP'",
            "../dbrestore.c:SR_array_restore p_data = (nil), p_data_size = 0",
            "../dbrestore.c:SR_array_restore field_type 'DBF_DEVICE' not handled",
            "../dbrestore.c:SR_array_restore No array write to database attempted because of error condition",
            "../dbrestore.c:SR_array_restore status=-1",
            "../dbrestore.c:reboot_restore restore for PV 'A.DTYP' failed",
        ],
        "{stderr}"
    );
}
