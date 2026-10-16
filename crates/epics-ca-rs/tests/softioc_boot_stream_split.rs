//! The boot of `softioc-rs -d <db>` puts on each stream what `softIoc` does.
//!
//! Measured on the compiled `softIoc` (R7.0.10) with the same one-record
//! database and `iocInit`, `exit` on stdin, ports free: stdout is the `#####` version
//! banner and the prompts; stderr is `Starting iocInit`, `iocRun: All
//! initialization complete`, and the second `iocInit`'s `iocBuild: ERROR`.
//! C prints no record-count line and no `CA server:` line when the port is
//! configured, so neither may appear on either stream. The banner is C's
//! with each build's own VCS stamp.
//!
//! softioc-rs refuses to boot on the exec backend (`softioc-rs.rs` main), and
//! that backend's `realtime-ca-ioc` has no iocsh to print a banner from.

#![cfg(all(unix, tokio_backend))]

use std::io::Write;
use std::net::{TcpListener, UdpSocket};
use std::process::{Command, Stdio};

use named_port::verdict as softioc_verdict;

/// Replace the describe and the date after its `Git:` / `build date/time:`
/// label, the two arms `genVersionHeader.pl` has.
fn mask_vcs_stamp(lines: &[&str]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            if let Some(date) = l.strip_prefix("## Rev. Date ") {
                let arm = ["Git: ", "build date/time: "]
                    .into_iter()
                    .find(|arm| date.starts_with(arm) && date.len() > arm.len());
                format!("## Rev. Date {}", arm.map_or("?", |_| "<vcs date>"))
            } else if let Some(rev) = l.strip_prefix("## Rev. ") {
                format!(
                    "## Rev. {}",
                    if rev.is_empty() { "?" } else { "<describe>" }
                )
            } else {
                l.to_string()
            }
        })
        .collect()
}

/// `softioc-rs -d <db>` on `port` fed `iocInit`, `exit`; its stdout and
/// stderr, or `None` when it names `port` as taken by someone else.
fn boot(db: &std::path::Path, port: u16) -> Option<(String, String)> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .arg("-d")
        .arg(db)
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        // Port 0 is out of range to `envGetInetPortConfigParam`.
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        // Beacons go to it; nothing here binds it.
        .env(
            "EPICS_CA_REPEATER_PORT",
            named_port::free_for_tcp_and_udp().to_string(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn softioc-rs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"iocInit\nexit\n")
        .unwrap();
    let out = child.wait_with_output().expect("softioc-rs output");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (!softioc_verdict::rs_ioc_port_was_taken(&stderr, port)).then_some((stdout, stderr))
}

#[test]
fn stdout_carries_the_banner_and_stderr_only_cs_boot_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("a.db");
    std::fs::write(&db, "record(ai, \"R1\") {\n}\n").expect("write the .db");
    let (stdout, stderr) = named_port::on_a_named_port(|port| boot(&db, port));
    // softIoc 8f5015b66's banner, its VCS stamp masked: each build carries its
    // own tree's describe and date, by the same genVersionHeader rule.
    let c_banner = "\
############################################################################
## EPICS R7.0.10.1-DEV
## Rev. R7.0.10-146-g8f5015b663d764ad75df
## Rev. Date Git: 2026-08-12 07:48:22 +0900
############################################################################";
    let rs_banner: Vec<&str> = stdout.lines().take(5).collect();
    assert_eq!(
        mask_vcs_stamp(&rs_banner),
        mask_vcs_stamp(&c_banner.lines().collect::<Vec<_>>()),
        "stdout: {stdout:?}"
    );
    for line in ["Starting iocInit", "iocRun:", "records,", "CA server:"] {
        assert!(!stdout.contains(line), "{line:?} on stdout: {stdout:?}");
    }
    assert_eq!(
        stderr,
        "Starting iocInit\n\
         iocRun: All initialization complete\n\
         iocBuild: ERROR IOC can only be initialized from uninitialized or stopped state\n"
    );
}

/// The stderr of a clean boot, as softIoc prints it.
const CLEAN_STDERR: &str = "Starting iocInit\n\
                            iocRun: All initialization complete\n\
                            iocBuild: ERROR IOC can only be initialized from uninitialized or stopped state\n";

/// [`boot`] under `on_a_named_port` with the first candidate taken by `hold`
/// before softioc-rs starts: the ports tried and the winning stderr.
/// named_port has no hook for its first number, so the closure takes it on
/// its first call.
fn boot_past_a_held_port<H>(hold: impl Fn(u16) -> H) -> (Vec<u16>, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("a.db");
    std::fs::write(&db, "record(ai, \"R1\") {\n}\n").expect("write the .db");
    let mut tried = Vec::new();
    let (_, stderr) = named_port::on_a_named_port(|port| {
        let _held = tried.is_empty().then(|| hold(port));
        tried.push(port);
        boot(&db, port)
    });
    (tried, stderr)
}

#[test]
fn a_held_tcp_port_is_retried_on_a_new_one() {
    let (tried, stderr) = boot_past_a_held_port(|port| {
        TcpListener::bind(("127.0.0.1", port)).expect("hold the TCP port")
    });
    // Another process on the box may take a later candidate too.
    assert!(
        tried.len() >= 2,
        "the held port, then a clean boot: {tried:?}"
    );
    assert_ne!(tried[0], tried[1]);
    assert_eq!(stderr, CLEAN_STDERR);
}

#[test]
fn a_held_udp_port_is_retried_on_a_new_one() {
    let (tried, stderr) = boot_past_a_held_port(|port| {
        UdpSocket::bind(("127.0.0.1", port)).expect("hold the UDP port")
    });
    // Another process on the box may take a later candidate too.
    assert!(
        tried.len() >= 2,
        "the held port, then a clean boot: {tried:?}"
    );
    assert_ne!(tried[0], tried[1]);
    assert_eq!(stderr, CLEAN_STDERR);
}
