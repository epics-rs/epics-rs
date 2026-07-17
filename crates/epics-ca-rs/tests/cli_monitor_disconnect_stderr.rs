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

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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

#[test]
fn a_disconnect_under_camonitor_writes_no_per_pv_stderr_line() {
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

    // Connect + first monitor event, then take the IOC away under it.
    std::thread::sleep(Duration::from_millis(1500));
    ioc.0.kill().expect("kill softioc-rs");
    let _ = ioc.0.wait();
    std::thread::sleep(Duration::from_millis(1500));

    mon.0.kill().expect("kill camonitor-rs");
    let out = mon
        .0
        .stdout
        .take()
        .map(|mut so| {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut so, &mut buf).expect("read stdout");
            buf
        })
        .expect("piped stdout");
    let err = mon
        .0
        .stderr
        .take()
        .map(|mut se| {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut se, &mut buf).expect("read stderr");
            buf
        })
        .expect("piped stderr");
    let out = std::process::Output {
        status: mon.0.wait().expect("reap camonitor-rs"),
        stdout: out,
        stderr: err,
    };
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

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
    // R18-19: stderr carries the ECA_DISCONN block, and C's `Context:` is the
    // RESOLVED peer name (the circuit's `hostNameCache`), not the dotted IP.
    assert!(
        stderr.contains("    Warning: \"Virtual circuit disconnect\"\n")
            && stderr.contains("    Source File: ../cac.cpp line 1240\n"),
        "stderr: {stderr:?}"
    );
    // The resolved loopback name is a `/etc/hosts` guarantee on unix: there
    // `getnameinfo` returns `localhost`, so pin that exact name. Windows has no
    // such alias, but `getnameinfo` still resolves 127.0.0.1 to the runner's
    // own computer name (e.g. `runnervmuktm0`), so C's `tcpiiu::getHostName`
    // yields `<machine>:<port>` — a resolved name, not the dotted IP. The
    // machine name varies per runner, so assert the SHAPE the R18-19 contract
    // guarantees (a non-empty resolved host with the peer port preserved)
    // rather than a fixed host string.
    #[cfg(unix)]
    assert!(
        stderr.contains("    Context: \"localhost:"),
        "the exception context is the resolved peer name, as C's \
         `tcpiiu::getHostName` gives it; stderr: {stderr:?}"
    );
    #[cfg(windows)]
    {
        let ctx = stderr
            .lines()
            .find_map(|l| l.trim().strip_prefix("Context: \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap_or_else(|| panic!("no `Context:` line on stderr: {stderr:?}"));
        let (host, port) = ctx
            .rsplit_once(':')
            .unwrap_or_else(|| panic!("Context is not `<host>:<port>`: {ctx:?}"));
        assert!(
            !host.is_empty(),
            "the exception context is the RESOLVED peer name (C's \
             `tcpiiu::getHostName`), which must be non-empty; ctx: {ctx:?}"
        );
        assert!(
            port.parse::<u16>().is_ok(),
            "R18-19: the peer port must be preserved in the context; ctx: {ctx:?}"
        );
    }
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
