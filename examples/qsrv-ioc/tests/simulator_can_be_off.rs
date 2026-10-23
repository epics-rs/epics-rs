//! The demo simulator writes DEMO:AI and toggles DEMO:BI every 500 ms. A C
//! softIocPVA serving the same database has nothing writing to it, so
//! `QSRV_IOC_NO_SIMULATOR` leaves the records as the database made them.

#![cfg(tokio_backend)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn no_simulator_leaves_demo_ai_at_its_database_value() {
    // CA's UDP responder must get this number or the IOC exits (a taken TCP
    // number only moves CA to a dynamic one), so it is held for the child,
    // whose fanout bind joins the hold.
    let ca_held = named_port::HeldPort::udp();
    let ca_port = ca_held.port;
    let pva_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let st_cmd = format!("{}/ioc/st.cmd", env!("CARGO_MANIFEST_DIR"));
    let mut ioc = Reaped(
        Command::new(env!("CARGO_BIN_EXE_qsrv-ioc"))
            .arg(&st_cmd)
            .env("QSRV_IOC_NO_SIMULATOR", "1")
            .env("EPICS_CAS_SERVER_PORT", ca_port.to_string())
            .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1:1")
            .env("EPICS_PVAS_SERVER_PORT", pva_port.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn qsrv-ioc"),
    );

    let mut stderr = BufReader::new(ioc.0.stderr.take().unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut line = String::new();
    while !line.contains("PVA port: ") {
        assert!(Instant::now() < deadline, "qsrv-ioc never came up");
        line.clear();
        assert!(stderr.read_line(&mut line).unwrap() > 0, "qsrv-ioc exited");
    }
    // Past two simulator ticks.
    std::thread::sleep(Duration::from_millis(1200));
    let mut stdin = ioc.0.stdin.take().unwrap();
    writeln!(stdin, "dbgf DEMO:AI").unwrap();
    writeln!(stdin, "exit").unwrap();
    drop(stdin);
    let mut out = String::new();
    ioc.0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let value = out
        .lines()
        .find_map(|l| {
            l.split_once("DBF_DOUBLE:")
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap_or_else(|| panic!("no dbgf line in {out:?}"));
    assert_eq!(value.parse::<f64>().unwrap(), 0.0, "{out}");
}
