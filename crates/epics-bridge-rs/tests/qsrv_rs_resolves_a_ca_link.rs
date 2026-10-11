//! qsrv-rs alone installs the `ca` link set before link init, so a CA link it
//! loads resolves, as every softIoc's dbCa resolves it. A second softIoc
//! R7.0.10-146-g8f5015b66 serving `record(ao, "SRC") { field(VAL, "5")
//! field(PINI, "YES") }`, and this one loading:
//!
//! ```text
//! record(ai, "DST") { field(INP, "SRC") field(SCAN, "1 second") }
//!
//! $ caget DST DST.SEVR DST.STAT
//! DST                            5
//! DST.SEVR                       NO_ALARM
//! DST.STAT                       NO_ALARM
//! ```
#![cfg(all(feature = "qsrv-bin", tokio_backend, unix))]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServer;
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pvdata::{PvField, ScalarValue};

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

#[tokio::test(flavor = "multi_thread")]
async fn qsrv_rs_resolves_a_ca_link_as_soft_ioc() {
    let source = CaServer::builder()
        .port(0)
        .pv("SRC", EpicsValue::Double(5.0))
        .build()
        .await
        .expect("CA server");
    let ca_port = source.udp_port();
    let _source = tokio::spawn(async move { source.run().await });

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("dst.db");
    std::fs::write(
        &db,
        "record(ai, \"DST\") { field(INP, \"SRC\") field(SCAN, \"1 second\") }\n",
    )
    .unwrap();
    let pva_port = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_qsrv-rs"))
        .arg("--db-file")
        .arg(&db)
        .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{ca_port}"))
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_PVAS_SERVER_PORT", pva_port.to_string())
        .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn qsrv-rs");
    let mut boot = String::new();
    for line in BufReader::new(child.stderr.take().unwrap()).lines() {
        let line = line.expect("stderr");
        boot.push_str(&line);
        boot.push('\n');
        if line.starts_with("iocRun: ") {
            break;
        }
    }

    let client = PvaClient::builder()
        .server_addr(std::net::SocketAddr::from(([127, 0, 0, 1], pva_port)))
        .timeout(Duration::from_secs(2))
        .build();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = None;
    while Instant::now() < deadline {
        got = value_and_severity(&client, "DST").await;
        if got == Some((5.0, 0)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(got, Some((5.0, 0)), "boot:\n{boot}");
}
