//! R18-21: `camonitor` prints NOTHING for a non-normal subscription status.
//!
//! C `camonitor.c:108-124` `event_handler` records `pv->status` and prints only
//! when `args.status == ECA_NORMAL`; nothing in the tool ever reads that status
//! back. The port's monitor loop had `Err(e) => eprintln!("{pv_name}: {e}")`, so
//! every IOC restart under a `camonitor-rs` emitted a line C never emits — and
//! after R18-18/R18-19 routed ECA_DISCONN to the subscriber, it emitted
//! `TST:AI: server reported ECA status 0x00c0` on *every* disconnect.
//!
//! The disconnect itself is not silent, and stderr is not empty: C reports the
//! lost IOC on stdout as `*** disconnected` (the connection callback,
//! `tool_lib.c:515`) and on stderr as the ECA_DISCONN `CA.Client.Exception`
//! block (`cac.cpp:1240`, R18-19). What C never writes is a per-PV line from
//! the event handler — that is the byte this test pins.

// Host/tokio-only: drives the async `caget`/`caput` CLI binaries out of
// process. Those binaries are built with this feature too, so their
// `CaClient` stack routes `spawn` to the background executor and then
// reaches tokio I/O with no reactor. Inapplicable under the executor
// backend; the RTEMS model has no async CLI client.
#![cfg(not(feature = "rtems-exec-model"))]

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::LineCollector;

/// A spawned child that is killed and reaped on every exit path, including a
/// panicking assertion inside the test.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start a real `softioc-rs` in its own process so the test can kill it and
/// take the circuit down under a live monitor — an in-process `CaServer` keeps
/// its listener in a detached task and cannot be shut down this way.
///
/// The IOC TAKES its port by binding it (`--port 0`) and reports what it got;
/// nothing probes a port and hands the number on.
fn start_ioc() -> (Reaped, u16) {
    let mut ioc = Reaped(
        Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
            .args(["--port", "0", "--pv", "TST:AI:double:1.0"])
            .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1:1")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn softioc-rs"),
    );

    // "CA server: UDP search on port 41234, TCP on port 41235, beacons → …"
    let stderr = ioc.0.stderr.take().expect("piped stderr");
    let mut reader = BufReader::new(stderr);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut line = String::new();
    loop {
        assert!(
            Instant::now() < deadline,
            "softioc-rs never announced a port"
        );
        line.clear();
        let n = reader.read_line(&mut line).expect("read softioc-rs banner");
        assert!(n > 0, "softioc-rs exited before announcing a port");
        if let Some(rest) = line.split_once("UDP search on port ") {
            let port: u16 = rest
                .1
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| panic!("unparsable banner: {line}"));
            return (ioc, port);
        }
    }
}

/// One full connect → IOC-kill → disconnect pass; returns the monitor's
/// complete (stdout, stderr). Every wait is on the output itself (a fixed
/// sleep here raced connect + first event under load).
///
/// `resolver_grace` holds the circuit open between the first event and the
/// IOC kill. It is NOT an output gate (nothing asserted below depends on it
/// existing) — it is the width of the window the async hostname engine gets
/// to win the R18-19 race before the disconnect snapshots the cache. macOS
/// reverse lookups go through mDNSResponder IPC and can lose a ~30 ms
/// window on every pass; C's `ipAddrToAsciiEngine` wins the same race only
/// when given the same room.
fn run_disconnect_scenario(resolver_grace: Duration) -> (String, String) {
    let (mut ioc, port) = start_ioc();

    let mut mon = Reaped(
        Command::new(env!("CARGO_BIN_EXE_camonitor-rs"))
            .arg("TST:AI")
            .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"))
            .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_CA_SERVER_PORT", port.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn camonitor-rs"),
    );

    let out_lines = LineCollector::spawn(mon.0.stdout.take().expect("piped stdout"));
    let err_lines = LineCollector::spawn(mon.0.stderr.take().expect("piped stderr"));

    // Wait for the first monitor event itself, then take the IOC away
    // under it.
    assert!(
        out_lines.wait_for(Duration::from_secs(10), |t| t.contains("TST:AI")),
        "no first monitor line within 10s; stdout: {:?}",
        out_lines.text()
    );
    std::thread::sleep(resolver_grace);
    ioc.0.kill().expect("kill softioc-rs");
    let _ = ioc.0.wait();

    // The disconnect must surface on both streams; each stream is gated on
    // its own marker so the kill below cannot race either write.
    assert!(
        out_lines.wait_for(Duration::from_secs(10), |t| t.contains("*** disconnected")),
        "no `*** disconnected` on stdout within 10s; stdout: {:?}",
        out_lines.text()
    );
    assert!(
        err_lines.wait_for(Duration::from_secs(10), |t| t.contains("Source File:")),
        "no CA.Client.Exception block on stderr within 10s; stderr: {:?}",
        err_lines.text()
    );

    mon.0.kill().expect("kill camonitor-rs");
    let _ = mon.0.wait();
    (out_lines.into_text(), err_lines.into_text())
}

/// The deterministic half of the contract — must hold on EVERY pass.
fn assert_disconnect_contract(stdout: &str, stderr: &str) {
    assert!(
        stdout.contains("TST:AI") && stdout.contains("1"),
        "the monitor must have delivered its first value before the IOC died; \
         stdout: {stdout:?}"
    );
    // R18-18, end to end: the circuit death reaches the connection callback.
    assert!(
        stdout.contains("*** disconnected"),
        "C reports a lost IOC on stdout, from the connection callback; \
         stdout: {stdout:?}"
    );
    // R18-19: stderr carries the ECA_DISCONN block.
    assert!(
        stderr.contains("    Warning: \"Virtual circuit disconnect\"\n")
            && stderr.contains("    Source File: ../cac.cpp line 1240\n"),
        "stderr: {stderr:?}"
    );
    // The `Context:` is `<host>:<port>` with the peer port preserved —
    // whatever the circuit's `hostNameCache` held when the block printed.
    let ctx = stderr
        .lines()
        .find_map(|l| l.trim().strip_prefix("Context: \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or_else(|| panic!("no `Context:` line on stderr: {stderr:?}"));
    let (host, port) = ctx
        .rsplit_once(':')
        .unwrap_or_else(|| panic!("Context is not `<host>:<port>`: {ctx:?}"));
    assert!(!host.is_empty(), "empty Context host; ctx: {ctx:?}");
    assert!(
        port.parse::<u16>().is_ok(),
        "R18-19: the peer port must be preserved in the context; ctx: {ctx:?}"
    );
    // R18-21: and that block is ALL of it. C's `event_handler` prints nothing
    // for a non-NORMAL status, so the per-PV status line the port used to emit
    // (`TST:AI: server reported ECA status 0x00c0`) is a line C never writes.
    assert!(
        !stderr.contains("TST:AI"),
        "camonitor's event_handler prints nothing for a non-NORMAL status \
         (camonitor.c:108-124) — no per-PV line belongs on stderr; \
         stderr: {stderr:?}"
    );
}

#[test]
fn a_disconnect_under_camonitor_writes_no_per_pv_stderr_line() {
    // The resolved-name half of R18-19 races the client's hostname engine:
    // `hostname::warm` fires at circuit-connect but resolves OFF-TASK, the
    // exact async shape of C's `ipAddrToAsciiEngine` (tcpiiu.cpp:600) — so a
    // disconnect arriving before the engine answers legitimately prints the
    // dotted IP, in C as in the port. Each pass must satisfy the
    // deterministic contract; the resolved name must show up within the
    // attempt budget (a cache-bypass regression — the pre-R18-19 port —
    // prints the dotted IP on EVERY pass and fails all attempts).
    const ATTEMPTS: usize = 5;
    #[cfg_attr(windows, allow(unused_variables, unused_mut))]
    let mut resolver_won = false;
    #[cfg(unix)]
    let mut last_stderr = String::new();
    for attempt in 0..ATTEMPTS {
        // Attempt 0 runs the raw race — the fast path a real disconnect hits.
        // Later attempts widen the resolver's window stepwise: on macOS the
        // lookup is an mDNSResponder IPC round-trip that can lose a ~30 ms
        // connect-to-kill window on every pass (it lost 15 straight on a
        // loaded CI runner), while a cache-bypass regression stays dotted-IP
        // at ANY width and still fails every attempt.
        let grace = Duration::from_millis(250 * attempt as u64);
        let (stdout, stderr) = run_disconnect_scenario(grace);
        assert_disconnect_contract(&stdout, &stderr);
        // The resolved loopback name is a `/etc/hosts` guarantee on unix:
        // there `getnameinfo` returns `localhost`, so pin that exact name.
        // Windows has no such alias — `getnameinfo` resolves 127.0.0.1 to the
        // machine's own (runner-varying) name, so the per-pass shape assert
        // above is the whole R18-19 contract there and one pass suffices.
        #[cfg(unix)]
        {
            if stderr.contains("    Context: \"localhost:") {
                resolver_won = true;
                break;
            }
            last_stderr = stderr;
        }
        #[cfg(windows)]
        break;
    }
    #[cfg(unix)]
    assert!(
        resolver_won,
        "the exception context never showed the resolved peer name in \
         {ATTEMPTS} passes — C's `tcpiiu::getHostName` yields it once its \
         async engine has answered, so a warmed loopback circuit must \
         resolve within the attempt budget; last stderr: {last_stderr:?}"
    );
}
