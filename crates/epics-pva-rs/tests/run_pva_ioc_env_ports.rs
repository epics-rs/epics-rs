//! `run_pva_ioc` serves on the pvxs server ports, not the CA port it is handed:
//! softIocPVX 1.5.3 with `EPICS_PVAS_SERVER_PORT=5281
//! EPICS_PVAS_BROADCAST_PORT=5287` listens on TCP 5281 and UDP 5287
//! (`config.cpp:402-415`).
//!
//! The IOC runs in a child (this binary re-run on [`ioc_child`]) because
//! `run_pva_ioc` ends in the iocsh REPL; the parent drives that REPL through a
//! piped stdin and ends it with `exit`.
#![cfg(all(feature = "client", tokio_backend, unix))]

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CHILD: &str = "RUN_PVA_IOC_CHILD_DB";

/// The child half; a no-op unless the parent set [`CHILD`].
#[test]
fn ioc_child() {
    let Ok(db) = std::env::var(CHILD) else {
        return;
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(epics_pva_rs::server::run_pva_ioc_app(
        epics_base_rs::server::ioc_app::IocApplication::new()
            .startup_line(&format!("dbLoadRecords(\"{db}\")")),
    ))
    .expect("run_pva_ioc");
}

#[test]
fn run_pva_ioc_serves_on_the_pvas_env_ports() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("e2e.db");
    std::fs::write(&db, "record(ai, \"E2E:X\") { field(VAL, \"4.5\") }\n").unwrap();
    let free_tcp = || {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    let (tcp, cas) = (free_tcp(), free_tcp());
    let udp = std::net::UdpSocket::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["ioc_child", "--exact", "--nocapture"])
        .env(CHILD, &db)
        .env("EPICS_PVAS_SERVER_PORT", tcp.to_string())
        .env("EPICS_PVAS_BROADCAST_PORT", udp.to_string())
        // `IocRunConfig::port`, the port `run_pva_ioc` used to bind.
        .env("EPICS_CAS_SERVER_PORT", cas.to_string())
        .env("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_PVAS_AUTO_BEACON_ADDR_LIST", "NO")
        .env("EPICS_PVAS_BEACON_ADDR_LIST", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the IOC child");
    // The one process this test started; every exit path below ends it by
    // this handle (its pid), never by name.
    let mut stdin = child.stdin.take().unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    // SAFETY: nextest runs this test alone in its process; the client
    // config reads these at build.
    unsafe {
        std::env::set_var("EPICS_PVA_ADDR_LIST", "127.0.0.1");
        std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_PVA_BROADCAST_PORT", udp.to_string());
        std::env::remove_var("EPICS_PVA_NAME_SERVERS");
    }
    let by_tcp = epics_pva_rs::client::PvaClient::builder()
        .timeout(Duration::from_secs(2))
        .server_addr(SocketAddr::from((Ipv4Addr::LOCALHOST, tcp)))
        .build();
    let by_search = epics_pva_rs::client::PvaClient::builder()
        .timeout(Duration::from_secs(2))
        .build();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut got = Vec::new();
    for client in [&by_tcp, &by_search] {
        let v = loop {
            match rt.block_on(client.pvget("E2E:X")) {
                Ok(v) => break Some(v),
                Err(_) if Instant::now() < deadline && child.try_wait().unwrap().is_none() => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                Err(_) => break None,
            }
        };
        got.push(v.map(|v| v.to_string()));
    }
    drop((by_tcp, by_search));

    let _ = stdin.write_all(b"exit\n");
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
    for v in &got {
        assert!(
            v.as_deref().is_some_and(|v| v.contains("4.5")),
            "{got:?}\n{log}"
        );
    }
    assert!(
        status.is_some_and(|s| s.success()),
        "the REPL did not end on `exit`: {status:?}\n{log}"
    );
}
