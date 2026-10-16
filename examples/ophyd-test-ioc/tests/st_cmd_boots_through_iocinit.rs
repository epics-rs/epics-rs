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

#[test]
fn st_cmd_reaches_ioc_run_with_sims_c_flags() {
    let port = |what: &str| {
        let probe = TcpListener::bind("127.0.0.1:0").expect(what);
        probe.local_addr().expect("bound").port().to_string()
    };
    let mut child = Reaped(
        Command::new(env!("CARGO_BIN_EXE_ophyd-test-ioc"))
            .arg("ioc/st.cmd")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("EPICS_CAS_SERVER_PORT", port("a CA port"))
            .env("EPICS_PVAS_SERVER_PORT", port("a PVA port"))
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
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut seen = String::new();
        let mut lines = BufReader::new(stderr).lines();
        for line in lines.by_ref() {
            let Ok(line) = line else { break };
            seen.push_str(&line);
            seen.push('\n');
            if line.contains("iocRun: All initialization complete") {
                let _ = tx.send(Ok(seen));
                lines.for_each(drop);
                return;
            }
        }
        let _ = tx.send(Err(seen));
    });
    match rx.recv_timeout(Duration::from_secs(300)) {
        Ok(Ok(seen)) => assert!(!seen.contains("panicked"), "{seen}"),
        Ok(Err(seen)) => panic!("ophyd-test-ioc exited before iocRun; stderr:\n{seen}"),
        Err(_) => panic!("ophyd-test-ioc did not reach iocRun in 300 s"),
    }

    let stdin = child.0.stdin.as_mut().expect("piped stdin");
    stdin
        .write_all(b"asynReport 0 SIM\n")
        .expect("write asynReport");
    stdin.flush().expect("flush stdin");
    let report = report_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("asynReport 0 SIM printed no port line");
    assert_eq!(report, SIM_REPORT);
}
