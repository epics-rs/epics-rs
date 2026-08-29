//! C CA tools (caget/caput/camonitor) ↔ Rust softioc-rs interop.
//!
//! Spawns the Rust IOC binary and exercises it from the C reference
//! implementation so we can prove wire-level compatibility in the
//! direction Rust-server → C-client.
//!
//! Both backends. The `softioc-rs` these tests spawn is built with the test's
//! own feature set, and its async `CaServer` takes the listener capability
//! from the tokio runtime rather than from the seam `Reactor`, so the C client
//! reaches it under `EPICS_RS_BUILD_EXEC_BACKEND=thread` as well: all five
//! cases pass under `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run
//! --profile interop -p epics-ca-rs --run-ignored all`.
//!
//! Note this file carries no census marker: its tests are plain `#[test]`
//! whose reactor dependency lives in a *child process*, which the
//! `rtems_exec_model_gate` anchors cannot see.

mod common;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::{LineCollector, require_tool, run_caget, run_caput};
use serial_test::serial;

const TEST_DB: &str = "
record(ai, \"TEST:AI\") {
    field(VAL, \"42.0\")
    field(EGU, \"V\")
}
record(stringin, \"TEST:STR\") {
    field(VAL, \"hello\")
}
record(longout, \"TEST:LOUT\") {
    field(VAL, \"0\")
}
";

struct RustIoc {
    child: Child,
    /// The UDP search port the IOC reported. Under `--port 0` the server's
    /// TCP listener lands on a different number, which is fine and is the
    /// protocol working: a search reply carries the server's own TCP port and
    /// the C client dials what the reply names.
    port: u16,
    err: LineCollector,
}

impl Drop for RustIoc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl RustIoc {
    /// Everything the IOC has said, for a failure message. The collector this
    /// reads is also what keeps the child's stderr pipe drained for its whole
    /// life.
    fn console(&self) -> String {
        self.err.text()
    }
}

/// Spawn `softioc-rs` on a port the kernel picks, and read back the number it
/// bound.
///
/// No candidate port and so no race: `--port 0` makes the process that binds
/// the one that chooses, and it reports what it got
/// (`server/ca_server.rs:1266`). A probed number would have been free when the
/// probe closed its socket and anyone's by the time this child bound it.
///
/// Panics on every failure and never reports an absence: the binary is
/// `CARGO_BIN_EXE_softioc-rs`, which cargo builds for this test, so there is
/// no machine where it can be legitimately missing. The `Option` this used to
/// return made our own IOC failing to start indistinguishable from an unmet
/// prerequisite, and each caller turned that into an early return — which
/// nextest scores as a pass (`epics-base-rs` `src/reference.rs:80-96`).
fn spawn_rust_ioc(db_content: &str) -> RustIoc {
    let exe = std::path::PathBuf::from(env!("CARGO_BIN_EXE_softioc-rs"));
    let dir = tempfile::tempdir().expect("temp dir for the IOC database");
    let db_path = dir.path().join("test.db");
    std::fs::write(&db_path, db_content).expect("write the IOC database");
    std::mem::forget(dir);

    let mut child = Command::new(&exe)
        .arg("-S")
        .arg("--db")
        .arg(&db_path)
        .arg("--port")
        .arg("0")
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| {
            panic!("`softioc-rs` at {} would not start: {e}", exe.display());
        });

    let err = LineCollector::spawn(child.stderr.take().expect("piped stderr"));
    let found = err.wait_for(common::budget::FACT_BUDGET, |t| report_port(t).is_some());
    let port = report_port(&err.text()).unwrap_or_else(|| {
        assert!(!found, "the wait succeeded but the report did not parse");
        panic!("`softioc-rs` never reported its CA port:\n{}", err.text())
    });
    RustIoc { child, port, err }
}

/// The UDP search port out of the server's own startup report.
///
/// One `eprintln!`, `CA server: UDP search on port {p}, TCP on port {t},
/// beacons -> {n} address(es)` (`server/ca_server.rs:1266`), printed after
/// both sockets are bound — so a number here is a bind that returned inside
/// the child, not a socket somebody on this box happens to own.
fn report_port(console: &str) -> Option<u16> {
    console
        .lines()
        .find_map(|l| l.split_once("CA server: UDP search on port "))
        .and_then(|(_, rest)| rest.split(',').next())
        .and_then(|n| n.trim().parse().ok())
}

#[test]
#[serial]
#[ignore = "spawns Rust IOC + invokes libca caget/caput; run with --include-ignored"]
fn c_caget_can_read_from_rust_ioc() {
    if !require_tool("caget") {
        return;
    }
    let ioc = spawn_rust_ioc(TEST_DB);
    let out = run_caget("127.0.0.1", ioc.port, "TEST:AI").expect("caget");
    assert!(out.contains("42"), "caget output: {out}\n{}", ioc.console());
}

#[test]
#[serial]
#[ignore = "spawns Rust IOC + invokes libca caget/caput; run with --include-ignored"]
fn c_caput_can_write_to_rust_ioc() {
    if !require_tool("caput") || !require_tool("caget") {
        return;
    }
    let ioc = spawn_rust_ioc(TEST_DB);
    assert!(run_caput("127.0.0.1", ioc.port, "TEST:LOUT", "9876"));
    let readback = run_caget("127.0.0.1", ioc.port, "TEST:LOUT").expect("caget");
    assert!(readback.contains("9876"), "readback: {readback}");
}

#[test]
#[serial]
#[ignore = "spawns Rust IOC + invokes libca caget/caput; run with --include-ignored"]
fn c_camonitor_sees_rust_ioc_changes() {
    if !require_tool("camonitor") || !require_tool("caput") {
        return;
    }
    let ioc = spawn_rust_ioc(TEST_DB);

    let mut mon = Command::new("camonitor")
        .arg("TEST:LOUT")
        .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", ioc.port.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("camonitor");

    // Wait for camonitor's leading event (it prints the current value on
    // connect) rather than assuming a fixed sleep covers search + connect.
    let out_lines = common::LineCollector::spawn(mon.stdout.take().expect("piped stdout"));
    assert!(
        out_lines.wait_for(budget::FACT_BUDGET, |t| t.contains("TEST:LOUT")),
        "camonitor never connected; got:\n{}",
        out_lines.text()
    );

    // Drive several writes via C caput.
    for v in [10, 20, 30] {
        let _ = run_caput("127.0.0.1", ioc.port, "TEST:LOUT", &v.to_string());
        std::thread::sleep(Duration::from_millis(150));
    }

    // Wait for the final value to be observed, then stop camonitor.
    let saw_final = out_lines.wait_for(budget::FACT_BUDGET, |t| t.contains("30"));
    let _ = mon.kill();
    let _ = mon.wait();
    assert!(
        saw_final,
        "camonitor never observed final value 30; got:\n{}",
        out_lines.into_text()
    );
}

#[test]
#[serial]
#[ignore = "spawns Rust IOC + invokes libca caget/caput; run with --include-ignored"]
fn c_cainfo_describes_rust_ioc_channel() {
    if !require_tool("cainfo") {
        return;
    }
    let ioc = spawn_rust_ioc(TEST_DB);
    let out = Command::new("cainfo")
        .arg("TEST:AI")
        .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", ioc.port.to_string())
        .output()
        .expect("cainfo");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("TEST:AI"), "cainfo output: {text}");
    assert!(
        text.contains("State:") || text.contains("Connected"),
        "cainfo output: {text}"
    );
}

#[test]
#[serial]
#[ignore = "spawns Rust IOC + invokes libca caget/caput; run with --include-ignored"]
fn pyepics_caget_via_libca_against_rust_ioc() {
    // Pyepics uses libca; if the C tools work this is largely covered.
    // Provide an explicit smoke through Python only when pyepics is present.
    let have_python = Command::new("python3").arg("--version").output().is_ok();
    if !have_python {
        eprintln!("SKIP: `python3` not found on PATH; install it to run this test");
        return;
    }
    let pyepics_check = Command::new("python3")
        .args(["-c", "import epics"])
        .output();
    if !matches!(&pyepics_check, Ok(o) if o.status.success()) {
        eprintln!("SKIP: pyepics not installed");
        return;
    }
    let ioc = spawn_rust_ioc(TEST_DB);
    let mut child = Command::new("python3")
        .args([
            "-c",
            "import os, epics, sys; \
             v = epics.caget('TEST:AI', timeout=5); \
             print(v); \
             sys.exit(0 if v is not None else 1)",
        ])
        .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", ioc.port.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("python3");
    let _ = child.stdin.take();
    let out = child.wait_with_output().expect("py wait");
    assert!(
        out.status.success(),
        "pyepics caget failed: stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("42"), "pyepics output: {text}");
}

use common::budget;
