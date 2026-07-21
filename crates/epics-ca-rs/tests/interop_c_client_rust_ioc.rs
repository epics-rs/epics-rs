//! C CA tools (caget/caput/camonitor) ↔ Rust softioc-rs interop.
//!
//! Spawns the Rust IOC binary and exercises it from the C reference
//! implementation so we can prove wire-level compatibility in the
//! direction Rust-server → C-client.
//!
//! Host/tokio-only. The IOC spawned here is the `softioc-rs` binary, which
//! cargo builds with this test's feature set; under `rtems-exec-model` its
//! async `CaServer` reaches tokio I/O from a background-executor worker and
//! dies with "there is no reactor running" (`server/udp.rs:773`,
//! `server/tcp.rs:1338`), so the C client finds nothing to connect to. That is
//! by design rather than a defect: the feature replaces the async server, and
//! its entry point is `rtems-ca-ioc`, not `softioc-rs`. The equivalent
//! feature-ON coverage is `blocking_real_record_e2e.rs`.
//!
//! Note this file carries no census marker: its tests are plain `#[test]`
//! whose reactor dependency lives in a *child process*, which the
//! `rtems_exec_model_gate` anchors cannot see.

#![cfg(not(feature = "rtems-exec-model"))]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::{free_tcp_port, free_udp_port, require_tool, run_caget, run_caput};
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
    port: u16,
}

impl Drop for RustIoc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn `softioc-rs` from the workspace target dir on a fresh port.
fn spawn_rust_ioc(db_content: &str) -> Option<RustIoc> {
    let exe = std::path::PathBuf::from(env!("CARGO_BIN_EXE_softioc-rs"));
    let dir = tempfile::tempdir().ok()?;
    let db_path = dir.path().join("test.db");
    std::fs::write(&db_path, db_content).ok()?;
    std::mem::forget(dir);

    let port = free_udp_port();
    // softioc-rs uses the same port for UDP and TCP.
    let _ = free_tcp_port(); // reserve uniqueness pool

    let child = Command::new(&exe)
        .arg("--db")
        .arg(&db_path)
        .arg("--port")
        .arg(port.to_string())
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    std::thread::sleep(Duration::from_millis(800));
    Some(RustIoc { child, port })
}

#[test]
#[serial]
#[ignore = "spawns Rust IOC + invokes libca caget/caput; run with --include-ignored"]
fn c_caget_can_read_from_rust_ioc() {
    if !require_tool("caget") {
        return;
    }
    let Some(ioc) = spawn_rust_ioc(TEST_DB) else {
        eprintln!("SKIP: failed to spawn softioc-rs");
        return;
    };
    let out = run_caget("127.0.0.1", ioc.port, "TEST:AI").expect("caget");
    assert!(out.contains("42"), "caget output: {out}");
}

#[test]
#[serial]
#[ignore = "spawns Rust IOC + invokes libca caget/caput; run with --include-ignored"]
fn c_caput_can_write_to_rust_ioc() {
    if !require_tool("caput") || !require_tool("caget") {
        return;
    }
    let Some(ioc) = spawn_rust_ioc(TEST_DB) else {
        return;
    };
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
    let Some(ioc) = spawn_rust_ioc(TEST_DB) else {
        return;
    };

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
        out_lines.wait_for(Duration::from_secs(10), |t| t.contains("TEST:LOUT")),
        "camonitor never connected; got:\n{}",
        out_lines.text()
    );

    // Drive several writes via C caput.
    for v in [10, 20, 30] {
        let _ = run_caput("127.0.0.1", ioc.port, "TEST:LOUT", &v.to_string());
        std::thread::sleep(Duration::from_millis(150));
    }

    // Wait for the final value to be observed, then stop camonitor.
    let saw_final = out_lines.wait_for(Duration::from_secs(10), |t| t.contains("30"));
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
    let Some(ioc) = spawn_rust_ioc(TEST_DB) else {
        return;
    };
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
        return;
    }
    let pyepics_check = Command::new("python3")
        .args(["-c", "import epics"])
        .output();
    if !matches!(&pyepics_check, Ok(o) if o.status.success()) {
        eprintln!("SKIP: pyepics not installed");
        return;
    }
    let Some(ioc) = spawn_rust_ioc(TEST_DB) else {
        return;
    };
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
