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
//! Each test allocates its own port pair (CA Rust port + C IOC port)
//! so they can in principle run in parallel, but `--test-threads=1`
//! is recommended because both sides shell out to `caget`/`caput`
//! which scan environment variables for `EPICS_CA_ADDR_LIST`.
//!
//! Skipping logic: each test calls `require_libca()` first, which
//! returns `false` if `softIoc` / `caget` / `caput` aren't on PATH —
//! the test then becomes a noisy no-op rather than a hard failure.
//!
//! Both backends. The Rust half of each differential pair is the in-process
//! async `CaServer`, which now takes its listener capability from the tokio
//! runtime the test entered rather than from the seam `Reactor`, so the C
//! `caget` reaches it under the exec backend too.

#![cfg(tokio_backend)]

use std::process::{Command, Stdio};
use std::time::Duration;

use serial_test::serial;

fn libca_paths() -> Option<(String, String, String)> {
    let softioc = which("softIoc")?;
    let caget = which("caget")?;
    let caput = which("caput")?;
    Some((softioc, caget, caput))
}

fn which(name: &str) -> Option<String> {
    let out = Command::new("which").arg(name).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Start the C softIoc with one PV. Returns the child and the temp root
/// holding its database — the caller must keep the root alive for the life of
/// the child. softIoc reads its config from stdin.
///
/// The database used to be `<tmp>/ca-rs-diff-<port>.db`, distinguished only by
/// an ephemeral port the OS is free to hand to another process a moment later,
/// on a directory shared by every checkout on the host, and nothing ever
/// removed it. An `O_EXCL` root is private to this call, and dropping it takes
/// the database with it on every exit path including a panicking one.
fn start_c_softioc(port: u16, db_content: &str) -> (std::process::Child, tempfile::TempDir) {
    let (softioc, _, _) = libca_paths().expect("softIoc on PATH");
    let dir = tempfile::tempdir().expect("fixture root");
    let tmp = dir.path().join(format!("ca-rs-diff-{port}.db"));
    std::fs::write(&tmp, db_content).expect("write db");
    let child = Command::new(softioc)
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        .env("EPICS_CAS_SERVER_PORT", port.to_string())
        .arg("-d")
        .arg(&tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn softIoc");
    // Give it time to initialize.
    std::thread::sleep(Duration::from_millis(800));
    (child, dir)
}

/// Run caget against the given port and return the parsed value as
/// a string (everything after the first whitespace).
fn run_caget(port: u16, pv: &str) -> Option<String> {
    let (_, caget, _) = libca_paths()?;
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
    let (_, _, caput) = libca_paths().unwrap_or_default();
    if caput.is_empty() {
        return false;
    }
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

#[allow(dead_code)]
trait DefaultExt {
    fn unwrap_or_default(self) -> String;
}
impl DefaultExt for Option<(String, String, String)> {
    fn unwrap_or_default(self) -> String {
        self.map(|t| t.2).unwrap_or_default()
    }
}

// Pick a fixed pair of high ports for the rust + C softiocs.
//
// Using `TcpListener::bind("127.0.0.1:0")` to get an ephemeral port
// returns numbers that the kernel keeps in TIME_WAIT for a window
// after the listener drops; rebinding the same port for the IOC's
// UDP responder *succeeds* but libca clients on macOS were observed
// to fail their search reliably under that pattern. Using fixed
// well-known-but-unlikely ports avoids the issue at the cost of
// requiring `--test-threads=1` (already documented in the module
// header).
/// Port for the C `softIoc` side. The Rust server no longer needs one
/// chosen up front — it binds port 0 and reports what it got.
fn c_softioc_port() -> u16 {
    49998
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "differential libca; run with --include-ignored"]
#[serial]
#[allow(clippy::approx_constant)]
async fn caget_double_matches_libca() {
    let Some(_) = libca_paths() else {
        eprintln!("skipping: libca not on PATH");
        return;
    };
    let c_port = c_softioc_port();

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
    let (mut c_child, _db_root) = start_c_softioc(
        c_port,
        "record(ai, \"DIFF:VAL\") { field(VAL, \"3.14\") }\n",
    );

    let rs_value = run_caget(rs_port, "DIFF:VAL");
    let c_value = run_caget(c_port, "DIFF:VAL");

    let _ = c_child.kill();
    let _ = c_child.wait();

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
    let Some(_) = libca_paths() else {
        eprintln!("skipping: libca not on PATH");
        return;
    };
    let c_port = c_softioc_port();

    let server = epics_ca_rs::server::CaServer::builder()
        .port(0)
        .pv("DIFF:WRITE", epics_base_rs::types::EpicsValue::Double(0.0))
        .build()
        .await
        .expect("rust server");
    let rs_port = server.udp_port();
    let _rs_handle = tokio::spawn(async move { server.run().await });

    let (mut c_child, _db_root) = start_c_softioc(
        c_port,
        "record(ao, \"DIFF:WRITE\") { field(VAL, \"0.0\") }\n",
    );

    let target = "42.5";
    let rs_put_ok = run_caput(rs_port, "DIFF:WRITE", target);
    let c_put_ok = run_caput(c_port, "DIFF:WRITE", target);
    assert!(rs_put_ok, "rust caput failed");
    assert!(c_put_ok, "c caput failed");

    let rs_value = run_caget(rs_port, "DIFF:WRITE").expect("rs caget");
    let c_value = run_caget(c_port, "DIFF:WRITE").expect("c caget");

    let _ = c_child.kill();
    let _ = c_child.wait();

    let rs_f: f64 = rs_value.parse().expect("parse rs");
    let c_f: f64 = c_value.parse().expect("parse c");
    assert!(
        (rs_f - 42.5).abs() < 1e-6 && (c_f - 42.5).abs() < 1e-6,
        "post-put mismatch: rs={rs_f} c={c_f}"
    );
}
