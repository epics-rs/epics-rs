//! `run_ca_ioc_app` serves the same IOC on both backends: its records over
//! CA, a `CA`-modified input link through the runner's `ca` link set, and a
//! `casr` that names the server on the console.
//!
//! The IOC runs in a child (this binary re-run on [`ioc_child`]) because the
//! runner ends in the iocsh REPL; the parent drives it through a piped stdin.
#![cfg(all(feature = "client", unix))]

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::test_block_on;
use epics_base_rs::types::EpicsValue;

const CHILD: &str = "RUN_CA_IOC_CHILD_DB";

const DB: &str = "\
record(ai, \"E2E:X\") { field(VAL, \"4.5\") }
record(ai, \"E2E:Y\") { field(INP, \"E2E:X CA\") field(SCAN, \".1 second\") }
";

/// The child half; a no-op unless the parent set [`CHILD`]. A failed run
/// panics with the error's Display, as softioc-rs prints it: the Debug form
/// hides a stolen port from the parent's verdict.
#[test]
fn ioc_child() {
    let Ok(db) = std::env::var(CHILD) else {
        return;
    };
    if let Err(e) = test_block_on(epics_ca_rs::server::run_ca_ioc_app(
        epics_base_rs::server::ioc_app::IocApplication::new()
            .startup_line(&format!("dbLoadRecords(\"{db}\")")),
    )) {
        panic!("run_ca_ioc_app: {e}");
    }
}

/// A search port somebody else holds reads as that, so the named-port retry
/// takes it: the child reports the error as softioc-rs does, in the text
/// [`named_port::verdict::rs_ioc_port_was_taken`] reads.
#[test]
fn a_held_search_port_reads_as_taken() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("e2e.db");
    std::fs::write(&db, DB).unwrap();
    let held = named_port::HeldPort::closed_udp();
    let out = ioc_child_on(&db, held.port)
        .stdin(Stdio::null())
        .output()
        .expect("run the IOC child");
    let log = format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        named_port::verdict::rs_ioc_port_was_taken(&log, held.port),
        "{log}"
    );
}

#[test]
fn run_ca_ioc_serves_records_links_and_casr() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("e2e.db");
    std::fs::write(&db, DB).unwrap();
    named_port::on_a_named_port(|port| serves_on(&db, port));
}

/// `pv` once it reads 4.5, or `None` past `deadline`.
async fn read(client: &epics_ca_rs::client::CaClient, pv: &str, deadline: Instant) -> Option<f64> {
    loop {
        if let Ok((_, EpicsValue::Double(v))) = client.caget(pv).await
            && v == 4.5
        {
            return Some(v);
        }
        if Instant::now() > deadline {
            return None;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    }
}

/// The client and the server on `port`, on loopback only.
fn ca_env(port: u16) -> [(&'static str, String); 5] {
    [
        ("EPICS_CA_SERVER_PORT", port.to_string()),
        ("EPICS_CAS_SERVER_PORT", port.to_string()),
        ("EPICS_CA_ADDR_LIST", "127.0.0.1".to_string()),
        ("EPICS_CA_AUTO_ADDR_LIST", "NO".to_string()),
        ("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1".to_string()),
    ]
}

/// [`ioc_child`] serving `db` on `port`.
fn ioc_child_on(db: &std::path::Path, port: u16) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["ioc_child", "--exact", "--nocapture"])
        .env(CHILD, db)
        .envs(ca_env(port))
        .env_remove("EPICS_CA_NAME_SERVERS");
    cmd
}

/// One run on `port`; `None` when the IOC reports that port taken.
fn serves_on(db: &std::path::Path, port: u16) -> Option<()> {
    let ca_env = ca_env(port);
    let mut child = ioc_child_on(db, port)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the IOC child");
    let mut stdin = child.stdin.take().unwrap();

    // SAFETY: nextest runs this test alone in its process; the client
    // reads these at construction.
    unsafe {
        for (k, v) in &ca_env {
            std::env::set_var(k, v);
        }
        std::env::remove_var("EPICS_CA_NAME_SERVERS");
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let got = test_block_on(async {
        let client = epics_ca_rs::client::CaClient::new()
            .await
            .expect("CA client");
        [
            read(&client, "E2E:X", deadline).await,
            read(&client, "E2E:Y", deadline).await,
        ]
    });

    let _ = stdin.write_all(b"casr 1\nexit\n");
    drop(stdin);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break Some(s);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = child.wait_with_output().unwrap();
    let log = format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if named_port::verdict::rs_ioc_port_was_taken(&log, port) {
        return None;
    }
    assert_eq!(
        got,
        [Some(4.5), Some(4.5)],
        "X by CA, Y by its ca link\n{log}"
    );
    assert!(
        status.is_some_and(|s| s.success()),
        "the REPL did not end on `exit`: {status:?}\n{log}"
    );
    assert!(
        log.contains(&format!("CAS-TCP server on 127.0.0.1:{port} with")),
        "casr did not name the served port\n{log}"
    );
    Some(())
}
