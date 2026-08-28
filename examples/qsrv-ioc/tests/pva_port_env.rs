//! R18-23: the PVA port comes from pvxs's rules, not CA's — and not from a
//! strict `parse()` that has neither.
//!
//! pvxs `Config::_fromDefs` reads the SERVER-specific `EPICS_PVAS_SERVER_PORT`
//! before the shared `EPICS_PVA_SERVER_PORT` (`config.cpp:402-408`), which is
//! why `epics_pva_rs::config::env::pvas_server_port` exists. This IOC parsed
//! `EPICS_PVA_SERVER_PORT` itself with `str::parse` and never looked at the
//! server-specific variable, so a pvxs-style deployment that configured only
//! `EPICS_PVAS_SERVER_PORT` silently got 5075.

// Every case spawns `qsrv-ioc`, whose `main` on the exec backend prints a
// refusal and returns: the demo stands `CaServer` and `PvaServer` up itself and
// the reactor-free backend compiles neither. Without this gate the child exits
// before printing a port and the assertion reads as "IOC died", which is what
// `--all-features` did while it still resolved the backend.
#![cfg(tokio_backend)]

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

/// Start the IOC with the given PVA environment and return the port it reports.
fn pva_port_with(env: &[(&str, &str)]) -> u16 {
    // TAKE ports by binding, so the numbers handed to the IOC are real and
    // nothing collides with a neighbouring test.
    let ca_port = {
        let probe = TcpListener::bind("127.0.0.1:0").expect("take a CA port");
        probe.local_addr().expect("bound").port()
    };

    // The IOC's default database is macro-parameterized, so it needs the demo
    // st.cmd (which supplies `P`) exactly as the README runs it.
    let st_cmd = format!("{}/ioc/st.cmd", env!("CARGO_MANIFEST_DIR"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_qsrv-ioc"));
    cmd.arg(&st_cmd)
        .env("EPICS_CAS_SERVER_PORT", ca_port.to_string())
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1:1")
        .env_remove("EPICS_PVA_SERVER_PORT")
        .env_remove("EPICS_PVAS_SERVER_PORT")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut ioc = Reaped(cmd.spawn().expect("spawn qsrv-ioc"));

    let stderr = ioc.0.stderr.take().expect("piped stderr");
    let mut reader = BufReader::new(stderr);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut line = String::new();
    loop {
        assert!(
            Instant::now() < deadline,
            "qsrv-ioc never reported its port"
        );
        line.clear();
        let n = reader.read_line(&mut line).expect("read qsrv-ioc stderr");
        assert!(n > 0, "qsrv-ioc exited before reporting its port");
        if let Some(rest) = line.split_once("PVA port: ") {
            return rest.1.trim().parse().expect("a port number");
        }
    }
}

/// pvxs `PickOne`: the server-specific variable wins, and it is the ONLY one
/// set here — pre-fix this IOC never read it and bound 5075.
#[test]
fn the_server_specific_pva_port_variable_is_honoured() {
    let port = {
        let probe = TcpListener::bind("127.0.0.1:0").expect("take a PVA port");
        probe.local_addr().expect("bound").port()
    };
    assert_eq!(
        pva_port_with(&[("EPICS_PVAS_SERVER_PORT", &port.to_string())]),
        port
    );
}

/// And it wins OVER the shared one when both are set (pvxs reads
/// `EPICS_PVAS_SERVER_PORT` first and stops).
#[test]
fn the_server_specific_variable_wins_over_the_shared_one() {
    let (specific, shared) = {
        let a = TcpListener::bind("127.0.0.1:0").expect("take a PVA port");
        let b = TcpListener::bind("127.0.0.1:0").expect("take a PVA port");
        (
            a.local_addr().expect("bound").port(),
            b.local_addr().expect("bound").port(),
        )
    };
    assert_eq!(
        pva_port_with(&[
            ("EPICS_PVAS_SERVER_PORT", &specific.to_string()),
            ("EPICS_PVA_SERVER_PORT", &shared.to_string()),
        ]),
        specific
    );
}

/// The shared variable still works on its own — this is the path that already
/// worked, and it must keep working through the owner.
#[test]
fn the_shared_pva_port_variable_still_works() {
    let port = {
        let probe = TcpListener::bind("127.0.0.1:0").expect("take a PVA port");
        probe.local_addr().expect("bound").port()
    };
    assert_eq!(
        pva_port_with(&[("EPICS_PVA_SERVER_PORT", &port.to_string())]),
        port
    );
}
