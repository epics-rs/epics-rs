//! qsrv-rs alone installs the `pva` link set before link init, so a `{pva:…}`
//! link it loads resolves, as softIocPVA R7.0.10-146-g8f5015b66 resolves it:
//!
//! ```text
//! record(ao, "SRC") { field(VAL, "5") field(PINI, "YES") }
//! record(ai, "DST") { field(INP, {pva:"SRC"}) field(SCAN, "1 second") }
//!
//! $ pvget DST DST.SEVR
//! DST       5
//! DST.SEVR  (0) NO_ALARM
//! ```
#![cfg(all(feature = "qsrv-bin", tokio_backend, unix))]

use std::time::{Duration, Instant};

use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pvdata::{PvField, ScalarValue};

mod qsrv_named_ports;

/// `(value, alarm.severity)` of `pv`'s NTScalar.
async fn value_and_severity(client: &PvaClient, pv: &str) -> Option<(f64, i32)> {
    let PvField::Structure(s) = client.pvget(pv).await.ok()? else {
        return None;
    };
    let ScalarValue::Double(value) = *s.get_value()? else {
        return None;
    };
    let PvField::Scalar(ScalarValue::Int(sevr)) = s.get_alarm()?.get_field("severity")? else {
        return None;
    };
    Some((value, *sevr))
}

#[tokio::test]
async fn qsrv_rs_resolves_a_pva_link_as_soft_ioc_pva() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pl.db");
    std::fs::write(
        &db,
        "record(ao, \"SRC\") { field(VAL, \"5\") field(PINI, \"YES\") }\n\
         record(ai, \"DST\") { field(INP, {pva:\"SRC\"}) field(SCAN, \"1 second\") }\n",
    )
    .unwrap();
    let got = qsrv_named_ports::on_a_qsrv_port(
        &db,
        |tcp| {
            let udp = tcp + 1;
            vec![
                ("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1".into()),
                ("EPICS_PVAS_SERVER_PORT", tcp.to_string()),
                ("EPICS_PVAS_BROADCAST_PORT", udp.to_string()),
                ("EPICS_PVA_BROADCAST_PORT", udp.to_string()),
                ("EPICS_PVA_ADDR_LIST", "127.0.0.1".into()),
                ("EPICS_PVA_AUTO_ADDR_LIST", "NO".into()),
            ]
        },
        |tcp| async move {
            let client = PvaClient::builder()
                .server_addr(std::net::SocketAddr::from(([127, 0, 0, 1], tcp)))
                .timeout(Duration::from_secs(2))
                .build();
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if let got @ Some((5.0, 0)) = value_and_severity(&client, "DST").await {
                    return got;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            None
        },
    )
    .await;
    assert_eq!(got, (5.0, 0));
}

/// The console qsrv-rs prints when its search port is held is what
/// `on_a_qsrv_port` retries on: one UDP socket this test owns holds `tcp + 1`.
#[test]
fn a_held_search_port_reads_as_a_stolen_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pl.db");
    std::fs::write(&db, "record(ao, \"SRC\") { field(VAL, \"5\") }\n").unwrap();
    let tcp = named_port::free_tcp_with_udp_successor();
    let held = std::net::UdpSocket::bind(("0.0.0.0", tcp + 1)).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_qsrv-rs"))
        .arg("--db-file")
        .arg(&db)
        .envs([
            ("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1".to_string()),
            ("EPICS_PVAS_SERVER_PORT", tcp.to_string()),
            ("EPICS_PVAS_BROADCAST_PORT", (tcp + 1).to_string()),
            ("EPICS_PVA_BROADCAST_PORT", (tcp + 1).to_string()),
            ("EPICS_PVA_ADDR_LIST", "127.0.0.1".to_string()),
            ("EPICS_PVA_AUTO_ADDR_LIST", "NO".to_string()),
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run qsrv-rs");
    let console = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{console}");
    assert!(
        named_port::verdict::qsrv_rs_port_was_taken(
            &console,
            tcp,
            named_port::udp_port_is_held(tcp + 1)
        ),
        "{console}"
    );
    drop(held);
}
