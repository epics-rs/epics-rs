//! R18-22: a CA server that could not get its configured TCP port SAYS SO.
//!
//! C `caservertask.c:576-593` compares the port `rsrv_grab_tcp` came back with
//! against the configured one and, when they differ, writes five `cas WARNING`
//! lines — because the consequence is not local: the server keeps the
//! configured UDP port, so two servers now answer searches on one UDP port and
//! a client that reaches this IOC by unicast may not find it at all.
//!
//! The port bound an ephemeral socket and said nothing. Captured from the
//! compiled `softIoc` with its configured TCP port held by another process
//! (stderr redirected, so errlog strips the ANSI):
//!
//! ```text
//! cas WARNING: Configured TCP port was unavailable.
//! cas WARNING: Using dynamically assigned TCP port 41201,
//! cas WARNING: but now two or more servers share the same UDP port.
//! cas WARNING: Depending on your IP kernel this server may not be
//! cas WARNING: reachable with UDP unicast (a host's IP in EPICS_CA_ADDR_LIST)
//! ```

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A spawned child that is killed and reaped on every exit path.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_taken_tcp_port_makes_the_server_print_cs_five_warning_lines() {
    // TAKE the port by binding it — the listener is held for the whole test, so
    // the IOC's TCP bind on it cannot succeed. Nothing here probes a port and
    // hands the number on.
    let held = TcpListener::bind("0.0.0.0:0").expect("take a TCP port");
    let port = held.local_addr().expect("bound addr").port();

    let mut ioc = Reaped(
        Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
            .args(["--port", &port.to_string(), "--pv", "TST:AI:double:1.0"])
            .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1:1")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn softioc-rs"),
    );

    let stderr = ioc.0.stderr.take().expect("piped stderr");
    let mut reader = BufReader::new(stderr);
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut seen: Vec<String> = Vec::new();
    let mut line = String::new();
    // The IOC keeps running, so read until the banner that follows the warning
    // block rather than to EOF.
    loop {
        assert!(
            Instant::now() < deadline,
            "softioc-rs never got past startup; stderr so far: {seen:#?}"
        );
        line.clear();
        let n = reader.read_line(&mut line).expect("read softioc-rs stderr");
        assert!(n > 0, "softioc-rs exited; stderr: {seen:#?}");
        seen.push(line.trim_end().to_string());
        if line.contains("CA server:") {
            break;
        }
    }

    let block: Vec<&String> = seen.iter().filter(|l| l.starts_with("cas ")).collect();
    assert_eq!(
        block.len(),
        5,
        "C writes five lines and no more; stderr: {seen:#?}"
    );
    assert_eq!(
        block[0],
        "cas WARNING: Configured TCP port was unavailable."
    );
    // Line 2 names the port the server actually got — which is NOT the one that
    // was configured, and is the whole point of the message.
    let assigned: u16 = block[1]
        .strip_prefix("cas WARNING: Using dynamically assigned TCP port ")
        .and_then(|rest| rest.strip_suffix(','))
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("line 2 does not match C's: {:?}", block[1]));
    assert_ne!(assigned, port, "that port is held by this test");
    assert_eq!(
        block[2],
        "cas WARNING: but now two or more servers share the same UDP port."
    );
    assert_eq!(
        block[3],
        "cas WARNING: Depending on your IP kernel this server may not be"
    );
    assert_eq!(
        block[4],
        "cas WARNING: reachable with UDP unicast (a host's IP in EPICS_CA_ADDR_LIST)"
    );

    drop(held);
}

/// The other half of C's block: an IOC that DID get the port it asked for says
/// nothing at all. The warning is about a conflict, not a startup trace.
#[test]
fn a_free_tcp_port_is_silent() {
    // The port is obtained by binding an ephemeral socket and releasing it, so
    // there is an unavoidable window between the release and softioc-rs's own
    // bind in which another process on a busy test host can steal it. A steal
    // is an environment artifact, not the behavior under test: softioc-rs then
    // finds the port taken and prints the same `cas` fallback block test 1
    // checks, or fails to bind at all. Retry on a non-clean run and let only a
    // run where softioc-rs actually got the port (and stayed silent) count.
    const ATTEMPTS: usize = 10;
    for attempt in 0..ATTEMPTS {
        let port = {
            let probe = TcpListener::bind("0.0.0.0:0").expect("take a TCP port");
            probe.local_addr().expect("bound addr").port()
        };

        let mut ioc = Reaped(
            Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
                .args(["--port", &port.to_string(), "--pv", "TST:AI:double:1.0"])
                .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1:1")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn softioc-rs"),
        );

        let stderr = ioc.0.stderr.take().expect("piped stderr");
        let mut reader = BufReader::new(stderr);
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut seen: Vec<String> = Vec::new();
        let mut line = String::new();
        // Outcomes: reached "CA server:" silently (clean pass), or the port was
        // stolen — surfaced either as a `cas` fallback line or as softioc-rs
        // exiting before the banner (it could not bind the stolen port at all).
        let mut stolen = false;
        loop {
            assert!(
                Instant::now() < deadline,
                "softioc-rs never got past startup; stderr so far: {seen:#?}"
            );
            line.clear();
            let n = reader.read_line(&mut line).expect("read softioc-rs stderr");
            if n == 0 {
                // Exited before the banner: treat as a steal and retry.
                stolen = true;
                break;
            }
            seen.push(line.trim_end().to_string());
            if line.starts_with("cas ") {
                stolen = true;
            }
            if line.contains("CA server:") {
                break;
            }
        }

        if !stolen {
            // softioc-rs got its configured port and said nothing — the case
            // under test.
            return;
        }

        assert!(
            attempt < ATTEMPTS - 1,
            "the configured TCP port was stolen in the release→bind window on \
             every one of {ATTEMPTS} attempts; last stderr: {seen:#?}"
        );
    }
}
