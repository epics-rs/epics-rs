//! Every `iocInit` route through `softioc-rs` scans, as C's `iocInit` does.
//!
//! C `iocBuild` runs `scanInit` and `initialProcess` (`iocInit.c:186`,
//! `:194-195`) and `iocRun` runs `scanRun` (`:259`) whichever shell calls
//! them. A database loaded at the prompt of a piped `softioc-rs` used to
//! build with no scan owner at all, so its periodic and event records never
//! processed while C's did.

// `softioc-rs` serves through the async CA front-end, so on `exec_backend`
// it refuses at startup instead of running.
#![cfg(tokio_backend)]

use std::io::Write;
use std::process::{Command, Stdio};

const DB: &str = r#"
record(calc, "SC:PER") { field(SCAN, ".1 second") field(CALC, "VAL+1") }
record(calc, "SC:EV") { field(SCAN, "Event") field(EVNT, "7") field(CALC, "VAL+1") }
record(calc, "SC:PINI") { field(PINI, "YES") field(CALC, "VAL+1") }
"#;

/// After the IOC runs: a second of `.1 second` ticks, then two events.
const PROBE: &str = "epicsThreadSleep 1\npostEvent 7\npostEvent 7\nepicsThreadSleep 0.2\n\
                     dbgf SC:PER\ndbgf SC:EV\ndbgf SC:PINI\n";

fn db_file(dir: &tempfile::TempDir) -> String {
    let db = dir.path().join("sc.db");
    std::fs::write(&db, DB).expect("write db");
    db.to_str().expect("utf-8 path").to_string()
}

/// Every `dbgf` value `softioc-rs` printed, in order, given `args` and
/// `stdin`.
fn values(args: &[&str], stdin: &str) -> Vec<f64> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .args(args)
        .env("EPICS_CA_SERVER_PORT", "0")
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .env("EPICS_PVAS_SERVER_PORT", "0")
        .env("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("run softioc-rs");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(format!("{stdin}exit\n").as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("softioc-rs exits");
    let out = String::from_utf8_lossy(&out.stdout).into_owned();
    out.split("DBF_DOUBLE:")
        .skip(1)
        .map(|v| {
            v.split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("no value after DBF_DOUBLE in {out:?}"))
        })
        .collect()
}

fn assert_scanned(got: &[f64]) {
    let [per, ev, pini] = got else {
        panic!("three dbgf values expected, got {got:?}");
    };
    assert!(*per >= 5.0, "the .1 second record must tick: {got:?}");
    assert_eq!(*ev, 2.0, "each postEvent processes the record: {got:?}");
    assert_eq!(*pini, 1.0, "PINI=YES processes once: {got:?}");
}

/// The reported route: no `-d` and no script, so the IOC is the prompt's.
#[test]
fn ioc_init_at_a_piped_prompt_scans() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = db_file(&dir);
    assert_scanned(&values(
        &[],
        &format!("dbLoadRecords(\"{db}\")\niocInit\n{PROBE}"),
    ));
}

/// `iocBuild` starts the scan threads with the gate closed: nothing scans
/// until `iocRun`, but PINI=YES has run.
#[test]
fn ioc_build_then_ioc_run_at_a_piped_prompt_scans_only_after_ioc_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = db_file(&dir);
    let got = values(
        &[],
        &format!(
            "dbLoadRecords(\"{db}\")\niocBuild\nepicsThreadSleep 0.5\npostEvent 7\n\
             dbgf SC:PER\ndbgf SC:EV\ndbgf SC:PINI\niocRun\n{PROBE}"
        ),
    );
    assert_eq!(got.get(..3), Some(&[0.0, 0.0, 1.0][..]), "built: {got:?}");
    assert_scanned(&got[3..]);
}

/// A script argument: `iocInit` inside it is the application's build.
#[test]
fn ioc_init_in_a_script_argument_scans() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = db_file(&dir);
    let script = dir.path().join("st.cmd");
    std::fs::write(
        &script,
        format!("dbLoadRecords(\"{db}\")\niocInit\n{PROBE}"),
    )
    .expect("write script");
    assert_scanned(&values(&[script.to_str().expect("utf-8 path")], ""));
}

/// `-d`: the binary's own `iocInit`, after which the prompt probes.
#[test]
fn ioc_init_from_the_command_line_scans() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = db_file(&dir);
    assert_scanned(&values(&["-d", &db], PROBE));
}
