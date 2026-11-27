//! `caput -c` whose put callback does not complete within `-w`: C prints
//! `Write callback operation timed out` (`caput.c:565-567`). The port then
//! exits 1 where C reads back and exits 0, so a script sees the failure.

#![cfg(tokio_backend)]

use std::collections::HashMap;

use epics_ca_rs::server::CaServer;
use tokio::process::Command;

/// The put completes only when the 30 s output delay ends.
const DB: &str = r#"record(calcout, "CB:SLOW") { field(CALC, "A") field(ODLY, "30") }"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_put_callback_past_w_prints_cs_line_and_exits_one() {
    let server = CaServer::builder()
        .port(0)
        .db_string(DB, &HashMap::new())
        .expect("load db")
        .build()
        .await
        .expect("build CA server");
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });

    let out = Command::new(env!("CARGO_BIN_EXE_caput-rs"))
        .args(["-c", "-w", "2", "CB:SLOW.A", "5"])
        .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"))
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        .output()
        .await
        .expect("run caput-rs");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stderr, "Write callback operation timed out\n");
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr:?}");
}
