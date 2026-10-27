//! `write_it` writes a `field$` member as its long string, escaped and cut
//! at BUF_SIZE-1 characters (save_restore.c:1844-1849). The expected file
//! is the C oracle IOC's for the same records, request file and `dbpf`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::{IocApplication, RunOutcome};

const CHILD: &str = "AUTOSAVE_LONG_STRING_SAVE_SCRIPT";

const DB: &str = r#"
record(stringout, "SO") { field(DESC, "hello world") field(VAL, "v1") }
record(stringout, "SO2") { field(DESC, "a\"b\\c{d}") }
record(calcout, "C") { field(CALC, "A+B+C+D") }
record(ai, "A") { field(INP, "SO.VAL NPP NMS") }
record(lsi, "LS") { field(SIZV, "300") }
record(stringout, "E") { }
record(ao, "AO") { }
"#;

const REQ: &str = "SO.DESC$\nSO2.DESC$\nC.CALC$\nA.INP$\nLS.VAL$\nE.DESC$\nSO.DESC\nSO.VAL$\nAO.VAL$\nNONE.DESC$\nAO.PREC$\n";

/// 250 characters, of which C writes the first 199.
fn long_value() -> String {
    (0..250u8).map(|i| char::from(b'0' + i % 40)).collect()
}

/// The oracle's `s.sav` after its header line. A `$` member that did not
/// connect is written as its empty buffer, not `Search Issued`
/// (save_restore.c:1844-1849: the long-string branch ignores `valid`).
fn c_body() -> String {
    format!(
        "! 3 channel(s) not connected - or not all gets were successful\n\
         SO.DESC$ hello world\n\
         SO2.DESC$ a\\\"b\\\\c{{d}}\n\
         C.CALC$ A+B+C+D\n\
         A.INP$ SO.VAL NPP NMS\n\
         LS.VAL$ {}\n\
         E.DESC$ \n\
         SO.DESC hello world\n\
         SO.VAL$ v1\n\
         #AO.VAL$ \n\
         #NONE.DESC$ \n\
         #AO.PREC$ \n\
         <END>\n",
        &long_value()[..199]
    )
}

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
            let deadline = Instant::now() + fact_budget::FACT_BUDGET;
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
fn a_long_string_member_is_written_as_its_string() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("sav")).unwrap();
    std::fs::write(d.join("a.db"), DB).unwrap();
    std::fs::write(d.join("s.req"), REQ).unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         set_requestfile_path(\"{d}\")\n\
         save_restoreSet_DatedBackupFiles(0)\n\
         create_manual_set(\"s.req\",\"\")\n\
         iocInit\n\
         dbpf LS.VAL$ \"{long}\"\n\
         manual_save(\"s.req\")\n",
        d = d.display(),
        long = long_value()
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
    assert_eq!(body, c_body(), "{out:?}");
}
