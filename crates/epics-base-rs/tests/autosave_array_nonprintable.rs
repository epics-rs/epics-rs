//! C's array-element parse keeps only the bytes `isprint` accepts
//! (dbrestore.c:589, save_restore.c:3063): a raw tab, control byte, DEL or
//! non-ASCII byte in a `.sav` element is dropped, while the `\n` C writes
//! for a newline survives. The expected values are the C oracle IOC's
//! `dbgf`s after a pass-1 restore and after `fdbrestore` of this file.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::{IocApplication, RunOutcome};
use epics_base_rs::types::EpicsValue;

const CHILD: &str = "AUTOSAVE_NONPRINTABLE_SCRIPT";

const DB: &str = r#"
record(waveform, "S") { field(FTVL, "STRING") field(NELM, "4") }
record(waveform, "L") { field(FTVL, "LONG") field(NELM, "4") }
"#;

const SAV: &[u8] = b"# Autosave release 6.1\n\
S @array@ { \"a\tb\" \"c\x01d\" \"\xc3\xa9x\" \"p\\nq\" }\n\
L @array@ { \"1\t2\" \"3\x7f\" }\n\
<END>\n";

/// The script, then the values once L is restored, in the child only.
#[epics_macros_rs::epics_test]
async fn child_restore() {
    let Some(script) = std::env::var_os(CHILD) else {
        return;
    };
    let outcome = IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(script.to_str().unwrap())
        .run_outcome(|config| async move {
            let db = config.db.clone();
            // iocInit returns, and a manual script reaches fdbrestore.
            epics_base_rs::server::db_server::announce_serving("probe");
            let deadline = Instant::now() + fact_budget::FACT_BUDGET;
            while db.get_pv("L").unwrap() != EpicsValue::LongArray(vec![12, 3].into()) {
                assert!(Instant::now() < deadline, "L: {:?}", db.get_pv("L"));
                sleep(Duration::from_millis(20)).await;
            }
            let s: Vec<String> = match db.get_pv("S").unwrap() {
                EpicsValue::StringArray(s) => s.iter().map(|e| e.to_string()).collect(),
                other => panic!("S: {other:?}"),
            };
            assert_eq!(s, ["ab", "cd", "x", "p\nq"]);
            config.autosave_manager.unwrap().shutdown();
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(outcome, RunOutcome::Served);
}

fn restore(after_init: &str, before_init: &str) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("sav")).unwrap();
    std::fs::write(d.join("a.db"), DB).unwrap();
    std::fs::write(d.join("sav/s.sav"), SAV).unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         {before_init}\
         create_manual_set(\"\")\n\
         iocInit\n\
         {after_init}",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_restore",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, d.join("st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}

#[test]
fn a_boot_restore_drops_what_isprint_rejects() {
    restore("", "set_pass1_restoreFile(\"s.sav\")\n");
}

#[test]
fn a_manual_restore_drops_what_isprint_rejects() {
    restore("fdbrestore(\"s.sav\")\n", "");
}
