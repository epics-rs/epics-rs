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

use std::time::{Duration, Instant};

use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServer;
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
    let got = qsrv_named_ports::on_a_qsrv_port(
        &db,
        |tcp| {
            vec![
                ("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{ca_port}")),
                ("EPICS_CA_AUTO_ADDR_LIST", "NO".into()),
                ("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1".into()),
                ("EPICS_PVAS_SERVER_PORT", tcp.to_string()),
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
