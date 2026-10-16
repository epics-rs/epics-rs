//! Differential tests against C `epics-base` softIoc + caget/caput.
//!
//! Runs the same operation against both `softioc-rs` and the C
//! `softIoc` from `epics-base` and asserts the two implementations
//! agree on the visible behavior. The point is to catch wire-level
//! drift that unit tests don't see — e.g. a header field encoded
//! one way by us and a different way by libca.
//!
//! These tests are `#[ignore]` by default so the suite still runs on
//! machines without `epics-base` installed. To run:
//!
//! ```bash
//! cargo test -p epics-ca-rs --test differential_libca -- --ignored --test-threads=1
//! ```
//!
//! The Rust server binds port 0 and the C IOC takes a probed candidate
//! through `c_ca_tools::spawn_softioc`, which retries a stolen one;
//! `--test-threads=1` is recommended because both sides shell out to
//! `caget`/`caput`, which scan `EPICS_CA_ADDR_LIST`.
//!
//! Each test calls `libca_paths()` first, which panics if `softIoc` /
//! `caget` / `caput` aren't on PATH: an early return would score as a pass.
//!
//! Tokio backend only: the Rust half of each differential pair is the
//! in-process async `CaServer`, served from the tokio runtime the test
//! entered.

#![cfg(tokio_backend)]

use std::process::{Command, Stdio};

use serial_test::serial;

fn libca_paths() -> (String, String, String) {
    let need = |name| which(name).unwrap_or_else(|| panic!("`{name}` not found on PATH"));
    (need("softIoc"), need("caget"), need("caput"))
}

fn which(name: &str) -> Option<String> {
    let out = Command::new("which").arg(name).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Run caget against the given port and return the parsed value as
/// a string (everything after the first whitespace).
fn run_caget(port: u16, pv: &str) -> Option<String> {
    let (_, caget, _) = libca_paths();
    let out = Command::new(&caget)
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"))
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        .arg("-w")
        .arg("3")
        .arg(pv)
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() || stdout.is_empty() {
        eprintln!(
            "caget failed: status={:?}\n  stdout: {stdout}\n  stderr: {stderr}",
            out.status
        );
        return None;
    }
    // Format: `<pv> <value>` — drop the PV name. Some caget builds
    // pad the columns so multiple spaces; split on whitespace.
    let mut parts = stdout.split_whitespace();
    let _pv = parts.next();
    let value = parts.collect::<Vec<_>>().join(" ");
    if value.is_empty() { None } else { Some(value) }
}

/// Run caput against the given port. Returns success.
fn run_caput(port: u16, pv: &str, value: &str) -> bool {
    let (_, _, caput) = libca_paths();
    Command::new(&caput)
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"))
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        .arg("-w")
        .arg("3")
        .arg(pv)
        .arg(value)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "differential libca; run with --include-ignored"]
#[serial]
#[allow(clippy::approx_constant)]
async fn caget_double_matches_libca() {
    libca_paths();
    // Rust side
    let server = epics_ca_rs::server::CaServer::builder()
        .port(0)
        .pv("DIFF:VAL", epics_base_rs::types::EpicsValue::Double(3.14))
        .build()
        .await
        .expect("rust server");
    let rs_port = server.udp_port();
    let _rs_handle = tokio::spawn(async move { server.run().await });

    // C side
    let c_ioc = c_ca_tools::spawn_softioc("record(ai, \"DIFF:VAL\") { field(VAL, \"3.14\") }\n");
    let c_port = c_ioc.udp_port;

    let rs_value = run_caget(rs_port, "DIFF:VAL");
    let c_value = run_caget(c_port, "DIFF:VAL");

    drop(c_ioc);

    let rs = rs_value.unwrap_or_else(|| panic!("rs caget returned no value"));
    let c = c_value.unwrap_or_else(|| panic!("c caget returned no value"));
    let rs_f: f64 = rs.parse().expect("parse rs");
    let c_f: f64 = c.parse().expect("parse c");
    assert!(
        (rs_f - c_f).abs() < 1e-6,
        "value mismatch: rs={rs_f} c={c_f}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "differential libca; run with --include-ignored"]
#[serial]
async fn caput_then_caget_matches_libca() {
    libca_paths();
    let server = epics_ca_rs::server::CaServer::builder()
        .port(0)
        .pv("DIFF:WRITE", epics_base_rs::types::EpicsValue::Double(0.0))
        .build()
        .await
        .expect("rust server");
    let rs_port = server.udp_port();
    let _rs_handle = tokio::spawn(async move { server.run().await });

    let c_ioc = c_ca_tools::spawn_softioc("record(ao, \"DIFF:WRITE\") { field(VAL, \"0.0\") }\n");
    let c_port = c_ioc.udp_port;

    let target = "42.5";
    let rs_put_ok = run_caput(rs_port, "DIFF:WRITE", target);
    let c_put_ok = run_caput(c_port, "DIFF:WRITE", target);
    assert!(rs_put_ok, "rust caput failed");
    assert!(c_put_ok, "c caput failed");

    let rs_value = run_caget(rs_port, "DIFF:WRITE").expect("rs caget");
    let c_value = run_caget(c_port, "DIFF:WRITE").expect("c caget");

    drop(c_ioc);

    let rs_f: f64 = rs_value.parse().expect("parse rs");
    let c_f: f64 = c_value.parse().expect("parse c");
    assert!(
        (rs_f - 42.5).abs() < 1e-6 && (c_f - 42.5).abs() < 1e-6,
        "post-put mismatch: rs={rs_f} c={c_f}"
    );
}
