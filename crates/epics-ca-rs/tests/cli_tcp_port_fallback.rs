//! R18-22: a CA server that could not get its configured TCP port SAYS SO.
//!
//! C `caservertask.c:578-595` compares the port `rsrv_grab_tcp` came back with
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

// Every case here drives the `softioc-rs` binary as a subprocess, and that
// binary serves through the async CA front-end, so on `exec_backend` it
// refuses at startup instead of running. `realtime-ca-ioc` is the entry
// point that brings a CA IOC up on that backend, through the blocking
// thread-per-client driver; it is a different binary with a different
// command line, so these cases follow `softioc-rs` rather than move.
#![cfg(tokio_backend)]

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;
use std::time::Instant;

/// A spawned child that is killed and reaped on every exit path.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// How long the server is given to print a line after the one the test is
/// waiting for. C prints nothing past the block, so there is no later line to
/// wait on; a miss here can only pass a late line, never fail a correct run.
const NO_MORE_GRACE: Duration = Duration::from_millis(1500);

fn stderr_lines(stderr: std::process::ChildStderr) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
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
            .args([
                "-S",
                "--port",
                &port.to_string(),
                "--pv",
                "TST:AI:double:1.0",
            ])
            .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1:1")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn softioc-rs"),
    );

    let rx = stderr_lines(ioc.0.stderr.take().expect("piped stderr"));
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    let mut seen: Vec<String> = Vec::new();
    // Read until the block under test is complete — NOT until some later
    // startup milestone. The five lines are written by `bind_tcp_listeners`,
    // and the UDP responders bind after it on the same configured port number:
    // `--port` sets both. That port was reserved here for TCP only, so on a
    // loaded host another process can hold the same number on UDP, and
    // softioc-rs then exits with `AddrInUse` — *after* printing all five lines.
    // EOF *before* the block is still a failure: the block is printed during
    // the TCP bind, so nothing that happens later can suppress it.
    let mut block: Vec<String> = Vec::new();
    while block.len() < 5 {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = rx.recv_timeout(left).unwrap_or_else(|e| {
            panic!(
                "softioc-rs printed {} of the 5 cas lines ({e}); stderr so far: {seen:#?}",
                block.len()
            )
        });
        if line.starts_with("cas ") {
            block.push(line.clone());
        }
        seen.push(line);
    }
    // "and no more": a sixth `cas` line inside the grace window. The server
    // prints nothing after the block when the port is configured, so there is
    // no later line to wait for; the window can only miss a late sixth, never
    // fail a correct run.
    if let Ok(line) = rx.recv_timeout(NO_MORE_GRACE) {
        assert!(
            !line.starts_with("cas "),
            "C writes five lines and no more; a sixth: {line:?}"
        );
    }

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
                .args([
                    "-S",
                    "--port",
                    &port.to_string(),
                    "--pv",
                    "TST:AI:double:1.0",
                ])
                .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1:1")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn softioc-rs"),
        );

        let rx = stderr_lines(ioc.0.stderr.take().expect("piped stderr"));
        let deadline = Instant::now() + fact_budget::FACT_BUDGET;
        let mut seen: Vec<String> = Vec::new();
        // C prints nothing for a configured port, so the only milestone is the
        // listener itself: a connect to the port succeeds once the TCP bind is
        // done. The `cas` block, if the port was stolen, is written during
        // that bind, so a short drain after the connect sees it. A steal is
        // also an exit before the listener comes up.
        let mut stolen = false;
        loop {
            assert!(
                Instant::now() < deadline,
                "softioc-rs never opened its TCP port; stderr so far: {seen:#?}"
            );
            while let Ok(line) = rx.try_recv() {
                stolen |= line.starts_with("cas ");
                seen.push(line);
            }
            if stolen {
                break;
            }
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                while let Ok(line) = rx.recv_timeout(NO_MORE_GRACE) {
                    stolen |= line.starts_with("cas ");
                    seen.push(line);
                }
                break;
            }
            if matches!(ioc.0.try_wait(), Ok(Some(_))) {
                stolen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
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
