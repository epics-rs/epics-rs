//! A boot restore of a DBF_NOACCESS field: C's `dbFindRecord` finds every
//! declared field (dbrestore.c:966-975), so `scalar_restore` is reached and
//! skips it in pass 0 (`:294-306`); in pass 1 `dbPut` refuses the field
//! type. The C oracle IOC, restoring `A.ASP 1`, `A.TIME 1` and `A.MLOK 1`
//! (a field rs keeps only by name) on an ao, prints nothing in pass 0 and
//! in pass 1, for each:
//!
//! ```text
//! recGblDbaddrError: dbPut: dbrType = 0, field_type = DBF_NOACCESS (17). Illegal Database Request Type PV: A.ASP
//! ../dbrestore.c:scalar_restore restore of '1' for 'A.ASP' failed
//! Illegal Database Request Type filename="../dbrestore.c" line number=318
//! ../dbrestore.c:reboot_restore restore for PV 'A.ASP' failed
//! ```

use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;

const CHILD: &str = "AUTOSAVE_NOACCESS_BOOT_RESTORE_DIR";

#[epics_macros_rs::epics_test]
async fn child_ioc() {
    let Some(dir) = std::env::var_os(CHILD).map(std::path::PathBuf::from) else {
        return;
    };
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(dir.join("st.cmd").to_str().unwrap())
        .run(|_| async move {
            epics_base_rs::server::db_server::announce_serving("probe");
            Ok(())
        })
        .await
        .unwrap();
}

/// The console's lines naming a restored field after a boot restore in
/// `pass`, each trimmed at the end.
fn boot(pass: u8) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.db"), "record(ao, \"A\") { field(VAL, \"3\") }\n").unwrap();
    std::fs::write(d.join("s.sav"), "# h\nA.ASP 1\nA.TIME 1\nA.MLOK 1\n<END>\n").unwrap();
    std::fs::write(
        d.join("st.cmd"),
        format!(
            "dbLoadRecords(\"{d}/a.db\")\n\
             set_savefile_path(\"{d}\")\n\
             save_restoreSet_DatedBackupFiles(0)\n\
             set_pass{pass}_restoreFile(\"s.sav\")\n\
             iocInit\n",
            d = d.display()
        ),
    )
    .unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_ioc", "--nocapture", "--test-threads=1"])
        .env(CHILD, d)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&out.stderr).lines())
        .map(|l| l.strip_prefix("test child_ioc ... ").unwrap_or(l))
        .filter(|l| !l.starts_with('\u{1b}'))
        .filter(|l| {
            ["A.ASP", "A.TIME", "A.MLOK", "filename="]
                .iter()
                .any(|s| l.contains(s))
        })
        .map(|l| l.trim_end().to_string())
        .collect()
}

#[test]
fn pass_0_skips_a_noaccess_field_silently() {
    assert_eq!(boot(0), Vec::<String>::new());
}

#[test]
fn pass_1_reports_dbput_refusing_a_noaccess_field() {
    let expected: Vec<String> = ["A.ASP", "A.TIME", "A.MLOK"]
        .iter()
        .flat_map(|pv| {
            [
                format!(
                    "recGblDbaddrError: dbPut: dbrType = 0, field_type = DBF_NOACCESS (17). \
                     Illegal Database Request Type PV: {pv}"
                ),
                format!("../dbrestore.c:scalar_restore restore of '1' for '{pv}' failed"),
                "Illegal Database Request Type filename=\"../dbrestore.c\" line number=318"
                    .to_string(),
                format!("../dbrestore.c:reboot_restore restore for PV '{pv}' failed"),
            ]
        })
        .collect();
    assert_eq!(boot(1), expected);
}
