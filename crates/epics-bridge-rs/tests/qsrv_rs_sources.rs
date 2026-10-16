//! qsrv-rs serves a single record and a group the way softIocPVX 1.5.3 does
//! on the same db, where `pvxmonitor -v A` and `pvxget -v G` print:
//!
//! ```text
//! A   value double = 1.5, alarm.status int32_t = 2, alarm.message "UDF"
//! G   record._options.atomic bool = true, a.value double = 1.5,
//!     a.display.units string = "mm"
//! ```
#![cfg(all(feature = "qsrv-bin", unix, tokio_backend))]

mod qsrv_named_ports;

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pvdata::{PvField, ScalarValue};

const DB: &str = r#"record(ai, "A") {
    field(VAL, "1.5") field(EGU, "mm") field(PREC, "2")
    info(Q:group, {"G":{"a":{"+channel":"VAL"}}})
}
"#;

fn leaf<'a>(v: &'a PvField, path: &str) -> &'a ScalarValue {
    let mut at = v;
    for name in path.split('.') {
        at = match at {
            PvField::Structure(s) => s
                .get_field(name)
                .unwrap_or_else(|| panic!("no {path}: {v:?}")),
            _ => panic!("no {path}: {v:?}"),
        };
    }
    match at {
        PvField::Scalar(s) => s,
        _ => panic!("{path} is not a scalar: {v:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_single_record_and_a_group_answer_as_softiocpvx() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    std::fs::write(&db, DB).unwrap();
    // qsrv-rs binds the named port on every NIC; a loss is a retry only on
    // the console's evidence (`qsrv_named_ports`).
    let (group, first) = qsrv_named_ports::on_a_qsrv_port(
        &db,
        |tcp| vec!["--port".into(), tcp.to_string()],
        |_| vec![("EPICS_PVAS_BROADCAST_PORT", "0".into())],
        |tcp| async move {
            let server = SocketAddr::from((Ipv4Addr::LOCALHOST, tcp));
            let client = PvaClient::builder()
                .server_addr(server)
                .timeout(Duration::from_secs(2))
                .build();
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            let group = loop {
                match client.pvget_from("G", server).await {
                    Ok(v) => break v,
                    Err(_) if std::time::Instant::now() > deadline => return None,
                    Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                }
            };
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let sub = client
                .pvmonitor_handle_from(
                    "A",
                    server,
                    move |_, v| {
                        let _ = tx.send(v.clone());
                    },
                    |_| {},
                )
                .await
                .expect("monitor A");
            let first = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
            drop(sub);
            Some((group, first.expect("no monitor event on A").unwrap()))
        },
    )
    .await;

    assert_eq!(leaf(&first, "value"), &ScalarValue::Double(1.5));
    assert_eq!(leaf(&first, "alarm.status"), &ScalarValue::Int(2));
    assert_eq!(
        leaf(&first, "alarm.message"),
        &ScalarValue::String("UDF".into())
    );
    assert_eq!(
        leaf(&group, "record._options.atomic"),
        &ScalarValue::Boolean(true)
    );
    assert_eq!(leaf(&group, "a.value"), &ScalarValue::Double(1.5));
    assert_eq!(
        leaf(&group, "a.display.units"),
        &ScalarValue::String("mm".into())
    );
}
