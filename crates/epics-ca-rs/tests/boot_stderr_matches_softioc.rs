//! An IOC's boot stderr is C's stream, so it carries no line softIoc does not
//! print. softIoc R7.0.10 on `record(ai, "X:AI") { field(VAL, "1") }`,
//! `softIoc -d x.db < /dev/null` with `EPICS_CAS_INTF_ADDR_LIST=127.0.0.1`
//! and `EPICS_CA_AUTO_ADDR_LIST=NO`, stderr:
//!
//! ```text
//! Starting iocInit
//! iocRun: All initialization complete
//! ```
//!
//! Each binary runs on the backend that boots it: `softioc-rs` on tokio,
//! `realtime-ca-ioc` (whose argv is its st.cmd) on the exec backend.

#![cfg(feature = "client-core")]

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SOFTIOC_BOOT_STDERR: &str = "Starting iocInit\niocRun: All initialization complete\n";
const DB: &str = "record(ai, \"X:AI\") { field(VAL, \"1\") }\n";

fn port_free_for_udp_and_tcp() -> u16 {
    loop {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tcp.local_addr().unwrap().port();
        if std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok() {
            return port;
        }
    }
}

/// `bin` with the db and C's environment, its stderr in a file under `dir`.
fn spawn(bin: &str, args: &[&std::ffi::OsStr], dir: &std::path::Path) -> Child {
    let err = std::fs::File::create(dir.join("stderr")).unwrap();
    let mut cmd = Command::new(bin);
    for (k, _) in std::env::vars().filter(|(k, _)| k.starts_with("EPICS_")) {
        cmd.env_remove(k);
    }
    cmd.args(args)
        .env(
            "EPICS_CA_SERVER_PORT",
            port_free_for_udp_and_tcp().to_string(),
        )
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(err)
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {bin}: {e}"))
}

/// The stderr once iocRun has been printed and `settle` more has passed, the
/// child killed if it is still serving.
fn boot_stderr(mut child: Child, dir: &std::path::Path, settle: Duration) -> String {
    let read = || {
        let mut s = String::new();
        std::fs::File::open(dir.join("stderr"))
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        s
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while !read().contains("iocRun: All initialization complete\n") {
        if Instant::now() > deadline || child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(settle);
    let _ = child.kill();
    let _ = child.wait();
    read()
}

#[cfg(tokio_backend)]
#[test]
fn softioc_rs_boot_stderr_is_softiocs() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("x.db");
    std::fs::write(&db, DB).unwrap();
    let child = spawn(
        env!("CARGO_BIN_EXE_softioc-rs"),
        &["-d".as_ref(), db.as_os_str()],
        dir.path(),
    );
    assert_eq!(
        boot_stderr(child, dir.path(), Duration::from_millis(500)),
        SOFTIOC_BOOT_STDERR
    );
}

#[cfg(exec_backend)]
#[test]
fn realtime_ca_ioc_boot_stderr_is_softiocs() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("x.db");
    std::fs::write(&db, DB).unwrap();
    let child = spawn(
        env!("CARGO_BIN_EXE_realtime-ca-ioc"),
        &[db.as_os_str()],
        dir.path(),
    );
    assert_eq!(
        boot_stderr(child, dir.path(), Duration::from_millis(500)),
        SOFTIOC_BOOT_STDERR
    );
}
