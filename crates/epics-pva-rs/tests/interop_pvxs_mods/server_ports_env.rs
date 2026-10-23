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
use super::interop_helpers::{PVXGET, pvxs_command, require_pvxs};
use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use epics_pva_rs::server::PvaServer;
use std::sync::Arc;
use tokio::sync::watch;

/// Start the server on `tcp`/`udp` from the environment. `None` when it did
/// not bind them and one of the two is held by another socket.
async fn serve_on(tcp: u16, udp: u16) -> Option<tokio::task::JoinHandle<CaResult<()>>> {
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
    // `None`: the server ended before it bound, which is how a held search
    // port reads (the start fails and the sender is dropped).
    let report = loop {
        if let Some(h) = rx.borrow_and_update().clone() {
            break Some(h.report());
        }
        if rx.changed().await.is_err() {
            break None;
        }
    };
    let Some(report) = report else {
        let _ = run.await;
        return held_port_or_panic(tcp, udp, "the server did not start");
    };
    if (report.tcp_port, report.udp_port) == (tcp, udp) {
        return Some(run);
    }
    run.abort();
    let _ = run.await;
    // Only a number the server did not bind can be somebody else's. A
    // missed number that nobody holds means the environment was not read.
    let (tcp_missed, udp_missed) = (report.tcp_port != tcp, report.udp_port != udp);
    assert!(
        (tcp_missed && named_port::tcp_port_is_held(tcp))
            || (udp_missed && named_port::udp_port_is_held(udp)),
        "the server ignored the environment: it bound {report:?}, not tcp {tcp} / \
         udp {udp}, and the number it missed is not held"
    );
    None
}

/// `None` (retry) only when the kernel says `tcp` or `udp` is held.
fn held_port_or_panic(
    tcp: u16,
    udp: u16,
    what: &str,
) -> Option<tokio::task::JoinHandle<CaResult<()>>> {
    assert!(
        named_port::tcp_port_is_held(tcp) || named_port::udp_port_is_held(udp),
        "{what}, and neither tcp {tcp} nor udp {udp} is held"
    );
    None
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
    let bin = require_pvxs(PVXGET);
    // The named numbers are the subject: the server must bind exactly the
    // two the environment gives it. A server that did not, while the kernel
    // says somebody else holds one of them, lost a race; a free number it
    // still did not bind is a regression and fails below.
    let (tcp, udp, run) = tokio::task::block_in_place(|| {
        let handle = tokio::runtime::Handle::current();
        named_port::on_a_named_port(|tcp| {
            let udp = named_port::free_for_tcp_and_udp();
            handle
                .block_on(serve_on(tcp, udp))
                .map(|run| (tcp, udp, run))
        })
    });
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
