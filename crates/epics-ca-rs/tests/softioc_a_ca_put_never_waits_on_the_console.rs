//! A CA put is served on a thread the IOC created, so it is not ok to block:
//! the `@errlog` line an `lso` writes under the record hold is dropped and
//! counted when the console stalls, never waited on. C's rsrv client threads
//! are `epicsThreadCreate` threads, `isOkToBlock` 0 from `calloc`
//! (`osdThread.c:175`), so `errlogVprintf` does not flush (`errlog.c:186-187`)
//! and every `caput` returns.
#![cfg(all(tokio_backend, feature = "client-core", target_os = "linux"))]

use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use epics_base_rs::types::EpicsValue;

/// 400 lines of 40 bytes: a one-page console four times over.
const PUTS: usize = 400;

fn port_free_for_udp_and_tcp() -> u16 {
    loop {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tcp.local_addr().unwrap().port();
        if std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok() {
            return port;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_put_to_an_errlog_record_returns_while_the_console_is_stalled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ls.db");
    std::fs::write(
        &path,
        "record(lso, \"LS\") { field(DTYP, \"stdio\") field(OUT, \"@errlog\") }\n",
    )
    .expect("write db");
    let port = port_free_for_udp_and_tcp();

    let (mut out, writer) = std::io::pipe().expect("pipe");
    // SAFETY: F_SETPIPE_SZ on a pipe this test owns.
    assert!(unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETPIPE_SZ, 4096) } >= 4096);
    let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .arg("-d")
        .arg(&path)
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(writer.try_clone().expect("dup pipe"))
        .stderr(writer)
        .spawn()
        .expect("spawn softioc-rs");

    let mut seen = Vec::new();
    let mut chunk = [0u8; 65536];
    while !String::from_utf8_lossy(&seen).contains("iocRun") {
        let n = out.read(&mut chunk).expect("read console");
        assert!(n > 0, "softioc-rs exited before iocRun");
        seen.extend_from_slice(&chunk[..n]);
    }
    // From here nothing reads the console.

    // SAFETY: the only test in this binary, before `CaClient::new` reads them.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
    }
    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CA client");
    let ch = client.create_channel("LS");
    ch.wait_connected(Duration::from_secs(5))
        .await
        .expect("LS connects");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut done = 0;
    while done < PUTS && Instant::now() < deadline {
        let value = EpicsValue::String(format!("put {done:04} {:-<30}", "").into());
        match tokio::time::timeout(Duration::from_secs(5), ch.put(&value)).await {
            Ok(Ok(())) => done += 1,
            _ => break,
        }
    }
    let last = ch.get().await.map(|(_, v)| v);
    let _ = child.kill();
    let _ = child.wait();
    drop(out);

    assert_eq!(
        done, PUTS,
        "a put waited for the stalled console under the record hold"
    );
    assert_eq!(
        last.ok(),
        Some(EpicsValue::String(format!("put 0399 {:-<30}", "").into()))
    );
}
