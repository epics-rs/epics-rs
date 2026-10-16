//! `run_ca_pva_qsrv_ioc_app` declares `pva` before its script runs, as
//! softIocPVA's `.dbd` carries pvxs's `link("pva", "lsetPVX")`, so `{pva:…}`
//! loads clean. softIocPVA R7.0.10-146-g8f5015b66, whole stderr:
//!
//! ```text
//! $ softIocPVA st.cmd </dev/null   # dbLoadRecords("jp.db") / iocInit
//! Starting iocInit
//! iocRun: All initialization complete
//! ```
//!
//! The port's `CA server:` line, which C's RSRV does not print, is required
//! exactly once wherever the server's start lands it. The IOC serves until
//! killed, so it runs in a child: this test binary re-run on [`ioc_child`].
#![cfg(all(feature = "qsrv", tokio_backend, unix))]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

const CHILD: &str = "QSRV_PVA_LINK_CHILD_STCMD";

/// The child half; a no-op unless the parent set [`CHILD`].
#[test]
fn ioc_child() {
    let Ok(stcmd) = std::env::var(CHILD) else {
        return;
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let _ = rt.block_on(epics_bridge_rs::qsrv::run_ca_pva_qsrv_ioc_app(
        epics_base_rs::server::ioc_app::IocApplication::new()
            .port(0)
            .startup_script(&stcmd),
    ));
}

#[test]
fn a_declared_pva_link_loads_clean_as_in_soft_ioc_pva() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("jp.db");
    std::fs::write(&db, "record(ai, \"A3\") { field(INP, {pva:\"REM:Z\"}) }\n").unwrap();
    let stcmd = dir.path().join("st.cmd");
    std::fs::write(
        &stcmd,
        format!("dbLoadRecords(\"{}\")\niocInit\n", db.display()),
    )
    .unwrap();

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["ioc_child", "--exact", "--nocapture"])
        .env(CHILD, &stcmd)
        .env("EPICS_PVAS_SERVER_PORT", "0")
        .env("EPICS_PVAS_BROADCAST_PORT", "0")
        .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_PVA_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the IOC child");
    // Read to the `iocRun` line and the server's own, kill, then drain what
    // the child wrote before it died: nothing else may appear.
    let mut stderr = String::new();
    let mut seen = Vec::new();
    let mut server = 0;
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut killed = false;
    while let Some(Ok(line)) = lines.next() {
        stderr.push_str(&line);
        stderr.push('\n');
        if line.starts_with("CA server: UDP search on port ") {
            server += 1;
        } else {
            seen.push(line);
        }
        if !killed && server == 1 && seen.iter().any(|l| l.starts_with("iocRun: ")) {
            child.kill().expect("kill the IOC child");
            killed = true;
        }
    }
    child.wait().expect("reap the IOC child");

    assert_eq!(server, 1, "{stderr}");
    assert_eq!(
        seen,
        ["Starting iocInit", "iocRun: All initialization complete"],
        "{stderr}"
    );
}
