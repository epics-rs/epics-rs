//! The array marker is C's: a value that begins with `@array@` is an array
//! (`strncmp`, save_restore.c:3333, dbrestore.c:913), any other is a
//! scalar, wherever the marker sits in it. The expected lines are the C
//! oracle IOC's for this file.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::types::EpicsValue;

const CHILD: &str = "AUTOSAVE_ARRAY_MARKER_SCRIPT";

const DB: &str = r#"
record(ao, "A") {}
record(stringout, "S") {}
"#;

const SAV: &str = "# Autosave release 6.1\n\
NOPE @array@ { \"1\" \"2\" }\n\
NOPS 5\n\
S x@array@y\n\
A 3\n\
<END>\n";

/// The restore, then S as it left it, in the child only.
#[epics_macros_rs::epics_test]
async fn child_marker() {
    let Some(script) = std::env::var_os(CHILD) else {
        return;
    };
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(script.to_str().unwrap())
        .run(|config| async move {
            let db = config.db.clone();
            epics_base_rs::server::db_server::announce_serving("probe");
            let deadline = Instant::now() + Duration::from_secs(30);
            while db.get_pv("A").unwrap().to_f64() != Some(3.0) {
                assert!(Instant::now() < deadline, "A never restored");
                sleep(Duration::from_millis(20)).await;
            }
            assert_eq!(
                db.get_pv("S").unwrap(),
                EpicsValue::String("x@array@y".into())
            );
            config.autosave_manager.unwrap().shutdown();
            Ok(())
        })
        .await
        .unwrap();
}

#[test]
fn only_a_value_that_begins_with_the_marker_is_an_array() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("sav")).unwrap();
    std::fs::write(d.join("a.db"), DB).unwrap();
    std::fs::write(d.join("sav/n.sav"), SAV).unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         create_manual_set(\"\")\n\
         iocInit\n\
         save_restoreSet_Debug(1)\n\
         fdbrestore(\"n.sav\")\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_marker", "--nocapture", "--test-threads=1"])
        .env(CHILD, d.join("st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|l| {
            l.starts_with("save_restore:manual_array_restore")
                || l.starts_with("save_restore:do_manual_restore")
        })
        .collect();
    assert_eq!(
        lines,
        [
            "save_restore:manual_array_restore:entry: PV = 'NOPE'",
            "save_restore:manual_array_restore: Gobbled unused array data.",
            "save_restore:do_manual_restore: ca_search for NOPS timeout",
        ],
        "{stdout}"
    );
}
