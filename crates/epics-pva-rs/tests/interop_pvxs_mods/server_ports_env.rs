//! The server's two ports come from two variables, read independently, as
//! pvxs `server::Config::_fromDefs` reads them (`config.cpp:402-415`):
//! TCP from `EPICS_PVAS_SERVER_PORT`, the search port from
//! `EPICS_PVAS_BROADCAST_PORT`. softIocPVX 1.5.3 with
//! `EPICS_PVAS_SERVER_PORT=5281 EPICS_PVAS_BROADCAST_PORT=5287` listens on
//! TCP 5281 and UDP 5287, and `pvxget` reaches it through
//! `EPICS_PVA_BROADCAST_PORT=5287` and through
//! `EPICS_PVA_NAME_SERVERS=127.0.0.1:5281`; a search on 5076 times out.

// `server::PvaServer` exists only on the tokio backend.
#![cfg(tokio_backend)]

use super::interop_helpers::PvxsOutput;
use super::interop_helpers::{PVXGET, pick_localhost_port, pvxs_command, require_pvxs};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use epics_pva_rs::server::PvaServer;
use std::sync::Arc;
use tokio::sync::watch;

fn free_udp_port() -> u16 {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").expect("bind 0");
    s.local_addr().unwrap().port()
}

fn pvxget(pvxget: &std::path::Path, env: &[(&str, String)]) -> std::process::Output {
    let mut cmd = pvxs_command(pvxget);
    cmd.args(["-w", "3", "PORTS:X"])
        .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_PVA_ADDR_LIST", "")
        .env_remove("EPICS_PVA_NAME_SERVERS")
        .env_remove("EPICS_PVA_BROADCAST_PORT");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.pvxs_output().expect("spawn pvxget")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pvxget_reaches_the_server_on_each_env_port() {
    let Some(bin) = require_pvxs(PVXGET) else {
        return;
    };
    let (tcp, udp) = (pick_localhost_port(), free_udp_port());
    // SAFETY: nextest runs this test alone in its process.
    unsafe {
        std::env::set_var("EPICS_PVAS_SERVER_PORT", tcp.to_string());
        std::env::set_var("EPICS_PVAS_BROADCAST_PORT", udp.to_string());
    }
    let db = Arc::new(PvDatabase::new());
    db.add_pv("PORTS:X", EpicsValue::Double(4.5)).await.unwrap();
    let server = PvaServer::from_parts(
        db,
        None,
        epics_base_rs::server::access_security::new_acf_cell(None),
        None,
        None,
    );
    let (tx, mut rx) = watch::channel(None);
    let run = tokio::spawn(async move { server.run_reporting(Some(tx)).await });
    let report = loop {
        if let Some(h) = rx.borrow_and_update().clone() {
            break h.report();
        }
        rx.changed().await.expect("server reports its ports");
    };
    assert_eq!((report.tcp_port, report.udp_port), (tcp, udp));

    let (reached, missed) = tokio::task::spawn_blocking(move || {
        let reached = [
            pvxget(
                &bin,
                &[
                    ("EPICS_PVA_BROADCAST_PORT", udp.to_string()),
                    ("EPICS_PVA_ADDR_LIST", "127.0.0.1".into()),
                ],
            ),
            pvxget(
                &bin,
                &[("EPICS_PVA_NAME_SERVERS", format!("127.0.0.1:{tcp}"))],
            ),
        ];
        // The default search port, 5076: nothing there serves PORTS:X, so
        // pvxget's own `-w 3` bound ends it.
        let missed = pvxget(&bin, &[("EPICS_PVA_ADDR_LIST", "127.0.0.1".into())]);
        (reached, missed)
    })
    .await
    .unwrap();
    run.abort();
    for out in reached {
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("value double = 4.5"),
            "stdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&missed.stdout);
    assert!(
        !missed.status.success() && !stdout.contains("4.5"),
        "a search on 5076 found the server: stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&missed.stderr)
    );
}
