//! A blank line in a file `fdbrestore` reads. C's `sscanf` leaves `PVname`
//! as the line before's (save_restore.c:3322) and puts "" to it, which
//! zeroes the value just restored; rs traces the name as C does and puts
//! nothing. The expected trace is the C oracle IOC's for this file.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::ioc_app::{IocApplication, RunOutcome};
use epics_base_rs::types::EpicsValue;

const CHILD: &str = "AUTOSAVE_BLANK_LINE_SCRIPT";

const DB: &str = r#"
record(ao, "A") {}
record(ao, "B") {}
record(stringout, "S") {}
record(waveform, "W") { field(FTVL, "DOUBLE") field(NELM, "4") }
"#;

const SAV: &str = "# Autosave release 6.1\n\
A 3\n\
\n\
S hi\n\
\n\
#W @array@ { \"1\" \"2\" }\n\
W @array@ { \"7\" \"8\" }\n\
\n\
B 4\n\
<END>\n";

/// The restore, then the values it left, in the child only.
#[epics_macros_rs::epics_test]
async fn child_blank_line() {
    let Some(script) = std::env::var_os(CHILD) else {
        return;
    };
    let outcome = IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_script(script.to_str().unwrap())
        .run_outcome(|config| async move {
            let db = config.db.clone();
            // iocInit returns, and the script reaches fdbrestore.
            epics_base_rs::server::db_server::announce_serving("probe");
            let deadline = Instant::now() + Duration::from_secs(30);
            while db.get_pv("B").unwrap().to_f64() != Some(4.0) {
                assert!(Instant::now() < deadline, "B never restored");
                sleep(Duration::from_millis(20)).await;
            }
            assert_eq!(db.get_pv("A").unwrap().to_f64(), Some(3.0));
            assert_eq!(db.get_pv("S").unwrap(), EpicsValue::String("hi".into()));
            match db.get_pv("W").unwrap() {
                EpicsValue::DoubleArray(w) => assert_eq!(w[..2], [7.0, 8.0]),
                other => panic!("W: {other:?}"),
            }
            config.autosave_manager.unwrap().shutdown();
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(outcome, RunOutcome::Served);
}

/// The child's `fdbrestore("m.sav")` at Debug 5.
fn restore_output() -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("sav")).unwrap();
    std::fs::write(d.join("a.db"), DB).unwrap();
    std::fs::write(d.join("sav/m.sav"), SAV).unwrap();
    let script = format!(
        "dbLoadRecords(\"{d}/a.db\")\n\
         set_savefile_path(\"{d}/sav\")\n\
         create_manual_set(\"\")\n\
         iocInit\n\
         save_restoreSet_Debug(5)\n\
         fdbrestore(\"m.sav\")\n",
        d = d.display()
    );
    std::fs::write(d.join("st.cmd"), script).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_blank_line",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, d.join("st.cmd"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    out
}

#[test]
fn a_blank_line_traces_the_name_before_and_puts_nothing() {
    let out = restore_output();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let names: Vec<&str> = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("save_restore:do_manual_restore: PVname="))
        .collect();
    assert_eq!(
        names,
        ["'A'", "'A'", "'S'", "'S'", "'#W'", "'W'", "'W'", "'B'"],
        "{stdout}"
    );
    // C's "is scalar ... Treating as array." for W's blank line leads only
    // to the put rs skips.
    assert!(!stdout.contains("is scalar in .sav file"), "{stdout}");
}

/// C's `#` branch gobbles a commented-out array through `SR_array_restore`
/// (save_restore.c:3443-3449), as on the oracle.
#[test]
fn a_commented_array_is_gobbled() {
    let out = restore_output();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let gobble: Vec<&str> = stderr
        .lines()
        .filter(|l| l.contains("SR_array_restore"))
        .collect();
    assert_eq!(
        gobble,
        [
            "../dbrestore.c:SR_array_restore :entry: PV = '#W'",
            "../dbrestore.c:SR_array_restore Gobbled unused array data.",
        ],
        "{stderr}"
    );
}

/// `manual_array_restore`'s lines for W, as the oracle prints them; its
/// `:entry:` printf has no separator before it (save_restore.c:2927).
#[test]
fn the_array_restore_traces_as_c() {
    let out = restore_output();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let walk: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("save_restore:manual_array_restore"))
        .collect();
    assert_eq!(
        walk,
        [
            "save_restore:manual_array_restore:entry: PV = 'W'",
            "save_restore:manual_array_restore: p_data = (nil), p_data_size = 0",
            "save_restore:manual_array_restore: Writing array to database",
        ],
        "{stdout}"
    );
}
