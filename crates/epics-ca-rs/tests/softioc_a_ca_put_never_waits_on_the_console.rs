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

use named_port::verdict as softioc_verdict;

/// `PUTS` puts to `LS` on softioc-rs at `port`, its console unread from
/// `iocRun` on: how many returned and what `LS` then read, or `None` when the
/// IOC reports `port` taken.
fn puts_past_a_stalled_console(
    path: &std::path::Path,
    port: u16,
) -> Option<(usize, Option<EpicsValue>)> {
    let (mut out, writer) = std::io::pipe().expect("pipe");
    // SAFETY: F_SETPIPE_SZ on a pipe this test owns.
    assert!(unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETPIPE_SZ, 4096) } >= 4096);
    let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .arg("-d")
        .arg(path)
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
        if n == 0 {
            // RSRV binds in iocInit, so a taken port ends the IOC before iocRun.
            let _ = child.wait();
            let console = String::from_utf8_lossy(&seen);
            if softioc_verdict::rs_ioc_port_was_taken(&console, port) {
                return None;
            }
            panic!("softioc-rs exited before iocRun:\n{console}");
        }
        seen.extend_from_slice(&chunk[..n]);
    }
    // From here nothing reads the console.

    // SAFETY: the only test in this binary, before `CaClient::new` reads them.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let served = rt.block_on(async {
        let client = epics_ca_rs::client::CaClient::new()
            .await
            .expect("CA client");
        let ch = client.create_channel("LS");
        ch.wait_connected(Duration::from_secs(5)).await.ok()?;

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut done = 0;
        while done < PUTS && Instant::now() < deadline {
            let value = EpicsValue::String(format!("put {done:04} {:-<30}", "").into());
            match ch.put_with_timeout(&value, Duration::from_secs(5)).await {
                Ok(()) => done += 1,
                Err(_) => break,
            }
        }
        Some((
            done,
            ch.get_with_timeout(fact_budget::FACT_BUDGET)
                .await
                .map(|(_, v)| v)
                .ok(),
        ))
    });
    drop(rt);
    let _ = child.kill();
    let _ = child.wait();
    out.read_to_end(&mut seen).expect("read console");

    let console = String::from_utf8_lossy(&seen);
    if softioc_verdict::rs_ioc_port_was_taken(&console, port) {
        return None;
    }
    Some(served.unwrap_or_else(|| panic!("LS never connected; console:\n{console}")))
}

#[test]
fn a_put_to_an_errlog_record_returns_while_the_console_is_stalled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ls.db");
    std::fs::write(
        &path,
        "record(lso, \"LS\") { field(DTYP, \"stdio\") field(OUT, \"@errlog\") }\n",
    )
    .expect("write db");
    let (done, last) = named_port::on_a_named_port(|port| puts_past_a_stalled_console(&path, port));

    assert_eq!(
        done, PUTS,
        "a put waited for the stalled console under the record hold"
    );
    assert_eq!(
        last,
        Some(EpicsValue::String(format!("put 0399 {:-<30}", "").into()))
    );
}
