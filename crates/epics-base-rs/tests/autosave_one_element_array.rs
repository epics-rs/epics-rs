//! `write_it` writes a member of more than one element whose current count
//! is at most one as a scalar (save_restore.c:1853-1860): its first element,
//! or the zero `ca_array_get` returns for an empty array. The expected file
//! is the C oracle IOC's for the same records and `dbpf`s.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::{IocApplication, RunOutcome};
use epics_base_rs::types::EpicsValue;

const CHILD: &str = "AUTOSAVE_ONE_ELEMENT_SCRIPT";
const RESTORE_CHILD: &str = "AUTOSAVE_ONE_ELEMENT_RESTORE";

const DB: &str = r#"
record(waveform, "W1") { field(FTVL, "DOUBLE") field(NELM, "4") }
record(waveform, "W0") { field(FTVL, "DOUBLE") field(NELM, "4") }
record(waveform, "W2") { field(FTVL, "DOUBLE") field(NELM, "4") }
record(waveform, "F1") { field(FTVL, "FLOAT") field(NELM, "4") }
record(waveform, "S1") { field(FTVL, "STRING") field(NELM, "4") }
record(waveform, "S0") { field(FTVL, "STRING") field(NELM, "4") }
record(waveform, "L1") { field(FTVL, "LONG") field(NELM, "4") }
record(waveform, "N1") { field(FTVL, "DOUBLE") field(NELM, "1") }
record(waveform, "N0") { field(FTVL, "DOUBLE") field(NELM, "1") }
record(waveform, "C0") { field(FTVL, "CHAR") field(NELM, "4") }
record(waveform, "L0") { field(FTVL, "LONG") field(NELM, "4") }
"#;

/// The oracle's `s.sav` after its header line.
const C_BODY: &str = "W1 5.5\n\
W0 0\n\
W2 @array@ { \"1.5\" \"2\" }\n\
F1 0.1\n\
S1 hi\n\
S0 \n\
L1 7\n\
N1 2.5\n\
N0 0\n\
C0 \n\
L0 \n\
<END>\n";

/// The script, then a wait for its save, in the child only.
#[epics_macros_rs::epics_test]
async fn child_save() {
    let Some(script) = std::env::var_os(CHILD) else {
        return;
    };
    let script = std::path::PathBuf::from(script);
    let sav = script.with_file_name("sav").join("s.sav");
    let outcome = IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(script.to_str().unwrap())
        .run_outcome(move |config| async move {
            epics_base_rs::server::db_server::announce_serving("probe");
            let deadline = Instant::now() + Duration::from_secs(30);
            while !std::fs::read_to_string(&sav).is_ok_and(|t| t.ends_with("<END>\n")) {
                assert!(Instant::now() < deadline, "no save");
                sleep(Duration::from_millis(20)).await;
            }
            config.autosave_manager.unwrap().shutdown();
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(outcome, RunOutcome::Served);
}

#[test]
fn a_member_of_one_current_element_is_written_as_a_scalar() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("sav")).unwrap();
    std::fs::write(d.join("a.db"), DB).unwrap();
    std::fs::write(
        d.join("s.req"),
        "W1\nW0\nW2\nF1\nS1\nS0\nL1\nN1\nN0\nC0\nL0\n",
    )
    .unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         set_requestfile_path(\"{d}\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         create_manual_set(\"s.req\",\"\")\n\
         iocInit\n\
         dbpf W1 5.5\n\
         dbpf W2 \"[1.5,2]\"\n\
         dbpf F1 0.1\n\
         dbpf S1 '[\"hi\"]'\n\
         dbpf L1 7\n\
         dbpf N1 2.5\n\
         manual_save(\"s.req\")\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_save", "--nocapture", "--test-threads=1"])
        .env(CHILD, d.join("st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = std::fs::read_to_string(d.join("sav/s.sav")).unwrap();
    let body = text.split_once('\n').unwrap().1;
    assert_eq!(body, C_BODY, "{out:?}");
}

/// The values a pass-1 restore of the oracle's file leaves, in the child
/// only: the oracle's `dbgf`s read W1 5.5 and W0, C0 and L0 0 with NORD 1,
/// S0 empty.
#[epics_macros_rs::epics_test]
async fn child_restore() {
    let Some(script) = std::env::var_os(RESTORE_CHILD) else {
        return;
    };
    let outcome = IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(script.to_str().unwrap())
        .run_outcome(|config| async move {
            let db = config.db.clone();
            let get = |name: &str| db.get_pv(name).unwrap();
            assert_eq!(get("W1"), EpicsValue::DoubleArray(vec![5.5].into()));
            assert_eq!(get("W0"), EpicsValue::DoubleArray(vec![0.0].into()));
            assert_eq!(get("W2"), EpicsValue::DoubleArray(vec![1.5, 2.0].into()));
            assert_eq!(get("F1"), EpicsValue::FloatArray(vec![0.1].into()));
            assert_eq!(get("S1"), EpicsValue::StringArray(vec!["hi".into()].into()));
            assert_eq!(get("S0"), EpicsValue::StringArray(Vec::new().into()));
            assert_eq!(get("L1"), EpicsValue::LongArray(vec![7].into()));
            assert_eq!(get("N1"), EpicsValue::DoubleArray(vec![2.5].into()));
            assert_eq!(get("N0"), EpicsValue::DoubleArray(vec![0.0].into()));
            assert_eq!(get("C0"), EpicsValue::CharArray(vec![0].into()));
            assert_eq!(get("L0"), EpicsValue::LongArray(vec![0].into()));
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(outcome, RunOutcome::Served);
}

#[test]
fn the_oracle_file_restores_as_on_the_oracle() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("sav")).unwrap();
    std::fs::write(d.join("a.db"), DB).unwrap();
    let header = "# Autosave release 6.1, compiled Sun Oct 18 09:52:34 2026\t\
                  Automatically generated - DO NOT MODIFY - 261018-192410\n";
    std::fs::write(d.join("sav/s.sav"), format!("{header}{C_BODY}")).unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         set_pass1_restoreFile(\"s.sav\")\n\
         iocInit\n",
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
        .env(RESTORE_CHILD, d.join("st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}
