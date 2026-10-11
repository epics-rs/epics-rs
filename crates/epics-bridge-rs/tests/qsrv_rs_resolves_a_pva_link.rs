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

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pvdata::{PvField, ScalarValue};

/// A free TCP port whose successor is a free UDP port: `PvaServer::from_parts`
/// listens for searches on `port + 1` whatever `EPICS_PVAS_BROADCAST_PORT`
/// says, and the IOC's own pvalink client must find it there.
fn free_port_pair() -> (u16, u16) {
    loop {
        let tcp = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = tcp.local_addr().unwrap().port();
        if port < u16::MAX
            && std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, port + 1)).is_ok()
        {
            return (port, port + 1);
        }
    }
}

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
    let (tcp, udp) = free_port_pair();
    let mut child = Command::new(env!("CARGO_BIN_EXE_qsrv-rs"))
        .arg("--db-file")
        .arg(&db)
        .env("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_PVAS_SERVER_PORT", tcp.to_string())
        .env("EPICS_PVAS_BROADCAST_PORT", udp.to_string())
        .env("EPICS_PVA_BROADCAST_PORT", udp.to_string())
        .env("EPICS_PVA_ADDR_LIST", "127.0.0.1")
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
        .server_addr(std::net::SocketAddr::from(([127, 0, 0, 1], tcp)))
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
