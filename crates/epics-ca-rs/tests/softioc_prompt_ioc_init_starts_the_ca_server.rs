//! An `iocInit` typed at the prompt of a `softioc-rs` started with no `-d`
//! and no script starts the CA server, as C's `iocRun` starts RSRV
//! (`iocInit.c:265-268`) whichever caller reached it. The prompt's `iocInit`
//! used to build a bare IOC that served nothing, while C's answered a client.
#![cfg(all(tokio_backend, feature = "client-core", target_os = "linux"))]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use epics_base_rs::types::EpicsValue;
use named_port::verdict as softioc_verdict;

/// What a CA client read from `PR` at `port`, or `None` when the IOC reports
/// `port` taken. The prompt stays open until the read is done, then EOF ends
/// the process.
fn read_after_prompt_ioc_init(db: &std::path::Path, port: u16) -> Option<Option<EpicsValue>> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn softioc-rs");
    let mut stdin = child.stdin.take().expect("piped stdin");
    writeln!(stdin, "dbLoadRecords(\"{}\")\niocInit", db.display()).expect("write stdin");

    // `iocRun: All initialization complete` is on the errlog, after the
    // runner announced it is serving.
    let mut console = String::new();
    let mut err = BufReader::new(child.stderr.take().expect("piped stderr"));
    while !console.contains("iocRun") {
        let mut line = String::new();
        if err.read_line(&mut line).expect("read stderr") == 0 {
            break;
        }
        console.push_str(&line);
    }
    if softioc_verdict::rs_ioc_port_was_taken(&console, port) {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }

    // SAFETY: the only test in this binary, before `CaClient::new` reads them.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let read = rt.block_on(async {
        let client = epics_ca_rs::client::CaClient::new()
            .await
            .expect("CA client");
        let ch = client.create_channel("PR");
        ch.wait_connected(Duration::from_secs(5)).await.ok()?;
        ch.get().await.map(|(_, v)| v).ok()
    });
    drop(rt);

    drop(stdin);
    let status = child.wait().expect("softioc-rs exits on EOF");
    assert!(status.success(), "EOF at the prompt exits 0: {status:?}");
    Some(read)
}

#[test]
fn ioc_init_at_the_prompt_serves_ca() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("pr.db");
    std::fs::write(&db, "record(ao, \"PR\") { field(VAL, \"4.5\") }\n").expect("write db");
    let read = named_port::on_a_named_port(|port| read_after_prompt_ioc_init(&db, port));
    assert_eq!(read, Some(EpicsValue::Double(4.5)), "PR was not served");
}
