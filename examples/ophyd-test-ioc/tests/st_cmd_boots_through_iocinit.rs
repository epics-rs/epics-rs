//! A debug build of the IOC runs `ioc/st.cmd` through `iocInit`, where PINI
//! runs every AD record's first cycle under its hold, and its `SIM` port has
//! simDetector's asyn flags: simDetector.cpp:1031-1036 passes 0, and C
//! `asynReport 0 SIM` on that st.cmd prints the line below
//! (asynManager.c:1045).

#![cfg(all(tokio_backend, feature = "ioc"))]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const SIM_REPORT: &str = "SIM multiDevice:No canBlock:No autoConnect:Yes";

/// The IOC's own report that `port` was held on UDP: it names the CA search
/// responder's bind and exits after `iocRun` (main's `Err`, Debug form).
fn ca_port_was_taken(stderr: &str, port: u16) -> bool {
    let what = format!("what: \"CA server UDP search responder bind 0.0.0.0:{port}\"");
    stderr
        .lines()
        .any(|l| l.contains(&what) && l.contains("kind: AddrInUse"))
}

/// Boot on CA port `port`; `None` only when the IOC says the port was taken.
fn sim_report_on(port: u16) -> Option<String> {
    let pva_port = {
        let probe = TcpListener::bind("127.0.0.1:0").expect("a PVA port");
        probe.local_addr().expect("bound").port().to_string()
    };
    let mut child = Reaped(
        Command::new(env!("CARGO_BIN_EXE_ophyd-test-ioc"))
            .arg("ioc/st.cmd")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("EPICS_CAS_SERVER_PORT", port.to_string())
            .env("EPICS_PVAS_SERVER_PORT", pva_port)
            .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1:1")
            .env("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "NO")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn ophyd-test-ioc"),
    );
    let stdout = child.0.stdout.take().expect("piped stdout");
    let stderr = child.0.stderr.take().expect("piped stderr");
    let (report_tx, report_rx) = mpsc::channel();
    std::thread::spawn(move || {
        // Drained from the start: st.cmd fills more than a pipe before iocRun.
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Some(at) = line.find("SIM multiDevice:") {
                let _ = report_tx.send(line[at..].to_string());
            }
        }
    });
    let (run_tx, run_rx) = mpsc::channel();
    let stderr = std::thread::spawn(move || {
        let mut seen = String::new();
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            seen.push_str(&line);
            seen.push('\n');
            if line.contains("iocRun: All initialization complete") {
                let _ = run_tx.send(());
            }
        }
        seen
    });
    // Both waits end early when the IOC exits; its stderr is then complete.
    let gone = |child: &mut Reaped, stderr: std::thread::JoinHandle<String>, why: &str| {
        let status = child.0.wait().expect("reap ophyd-test-ioc");
        let seen = stderr.join().expect("stderr reader");
        assert!(!seen.contains("panicked"), "{seen}");
        if ca_port_was_taken(&seen, port) {
            return None;
        }
        panic!("{why} ({status}); stderr:\n{seen}");
    };
    match run_rx.recv_timeout(Duration::from_secs(300)) {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return gone(&mut child, stderr, "ophyd-test-ioc exited before iocRun");
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("ophyd-test-ioc did not reach iocRun in 300 s")
        }
    }

    let stdin = child.0.stdin.as_mut().expect("piped stdin");
    // A pipe the exited IOC closed is judged with its stderr below.
    let _ = stdin
        .write_all(b"asynReport 0 SIM\n")
        .and_then(|()| stdin.flush());
    match report_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(report) => Some(report),
        Err(mpsc::RecvTimeoutError::Disconnected) => gone(
            &mut child,
            stderr,
            "ophyd-test-ioc exited before asynReport 0 SIM",
        ),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("asynReport 0 SIM printed no port line in 30 s")
        }
    }
}

/// The CA port is named, so another process can hold it on UDP between the
/// probe and the IOC's bind; that IOC exits, and the boot retries on a fresh
/// number (see the `named-port` crate).
#[test]
fn st_cmd_reaches_ioc_run_with_sims_c_flags() {
    let report = named_port::on_a_named_port(sim_report_on);
    assert_eq!(report, SIM_REPORT);
}

/// The retry's evidence is the IOC's real console, not a guess at it.
#[test]
fn a_ca_port_held_on_udp_is_read_as_taken() {
    let held = std::net::UdpSocket::bind("0.0.0.0:0").expect("hold a UDP port");
    let port = held.local_addr().expect("bound").port();
    assert_eq!(sim_report_on(port), None);
}
