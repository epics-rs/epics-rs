//! A `ts` value mode serves the type its `channelRegisterPost` probe ends
//! at (`ts.c:343-362`), and `getProperties` keeps `display.precision` only
//! for a float type (`dbAccess.c:341-348,387-394`). softIocPVX 1.5.3 on
//! `record(ai, "T") { field(VAL, "1") field(PINI, "YES") }`, `pvxget`:
//!
//! ```text
//! T.VAL{"ts":{"num":"dbl"}}     value double, display.precision int32_t = 0
//! T.VAL{"ts":{"num":"sec"}}     value uint32_t, display.limitLow uint32_t, no precision
//! T.VAL{"ts":{"num":"ts"}}      value uint32_t[] = {2}[sec, nsec], no precision
//! T.VAL{"ts":{"str":"epics"}}   value string = "2026-10-10 20:54:44.930843"
//! ```
//!
//! and a group `G` of `a` (sec, scalar), `b` (str epics, plain), `c` (ts,
//! any), `d` (dbl, scalar): `a.value uint32_t`, `b string`, `c-> uint32_t[]
//! = {2}`, `d.value double` with `d.display.precision`.

#![cfg(feature = "qsrv")]
#![cfg(tokio_backend)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use epics_bridge_rs::qsrv::{BridgeProvider, QsrvMount, QsrvPvStore};
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pvdata::{PvField, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};

const DB: &str = r#"record(ai, "T") { field(VAL, "1") field(PINI, "YES") }"#;

const GROUP: &str = r#"{"G": {
    "a": {"+channel": "T.VAL{\"ts\":{\"num\":\"sec\"}}", "+type": "scalar"},
    "b": {"+channel": "T.VAL{\"ts\":{\"str\":\"epics\"}}", "+type": "plain"},
    "c": {"+channel": "T.VAL{\"ts\":{\"num\":\"ts\"}}", "+type": "any"},
    "d": {"+channel": "T.VAL{\"ts\":{\"num\":\"dbl\"}}", "+type": "scalar"}
}}"#;

fn free_port(udp: bool) -> u16 {
    if udp {
        let s = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        s.local_addr().unwrap().port()
    } else {
        let s = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        s.local_addr().unwrap().port()
    }
}

fn value_at<'a>(name: &str, value: &'a PvField, path: &str) -> &'a PvField {
    path.split('.')
        .try_fold(value, |field, seg| match field {
            PvField::Structure(s) => s.get_field(seg),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{name}: no {path} in {value:?}"))
}

fn elements(name: &str, field: &PvField) -> Vec<ScalarValue> {
    match field {
        PvField::ScalarArray(v) => v.clone(),
        PvField::ScalarArrayTyped(v) => v.to_scalar_values(),
        other => panic!("{name}: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ts_value_mode_serves_its_probe_type_and_options() {
    let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(GROUP).expect("load group");
    assert_eq!(provider.process_groups().await, 1);

    let cfg = PvaServerConfig {
        tcp_port: free_port(false),
        udp_port: free_port(true),
        ..PvaServerConfig::isolated()
    };
    let addr = SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, cfg.tcp_port));
    let store = QsrvPvStore::new(
        epics_base_rs::runtime::task::Reactor::current().expect("the test enters a runtime"),
        provider,
    );
    let composite = epics_pva_rs::server_native::CompositeSource::new();
    QsrvMount {
        store: Arc::new(store),
        enabled: true,
        acf: epics_base_rs::server::access_security::new_acf_cell(None),
    }
    .install_channel_sources(&db, &composite)
    .expect("qsrv sources mount");
    let _server = PvaServer::start(composite, cfg).expect("qsrv server starts");
    let client = PvaClient::builder()
        .server_addr(addr)
        .timeout(Duration::from_secs(5))
        .build();

    let get = async |name: &str| {
        let read = client
            .pvget_marked(name)
            .await
            .unwrap_or_else(|e| panic!("get {name}: {e:?}"));
        let marked = read.marked.unwrap_or_default();
        (read.value, marked)
    };

    let name = r#"T.VAL{"ts":{"num":"sec"}}"#;
    let (value, marked) = get(name).await;
    let PvField::Scalar(ScalarValue::UInt(sec)) = *value_at(name, &value, "value") else {
        panic!("{name}: {value:?}");
    };
    assert!(matches!(
        value_at(name, &value, "display.limitLow"),
        PvField::Scalar(ScalarValue::UInt(_))
    ));
    assert!(
        !marked.iter().any(|p| p == "display.precision"),
        "{name}: {marked:?}"
    );

    let name = r#"T.VAL{"ts":{"num":"ts"}}"#;
    let (value, marked) = get(name).await;
    let ts = elements(name, value_at(name, &value, "value"));
    assert_eq!(ts.len(), 2, "{name}");
    assert_eq!(ts[0], ScalarValue::UInt(sec), "{name}");
    assert!(
        !marked.iter().any(|p| p == "display.precision"),
        "{name}: {marked:?}"
    );

    let name = r#"T.VAL{"ts":{"num":"dbl"}}"#;
    let (value, marked) = get(name).await;
    assert!(matches!(
        value_at(name, &value, "value"),
        PvField::Scalar(ScalarValue::Double(_))
    ));
    assert!(
        marked.iter().any(|p| p == "display.precision"),
        "{name}: {marked:?}"
    );

    let name = r#"T.VAL{"ts":{"str":"epics"}}"#;
    let (value, _) = get(name).await;
    let PvField::Scalar(ScalarValue::String(text)) = value_at(name, &value, "value") else {
        panic!("{name}: {value:?}");
    };
    assert_eq!(text.as_bytes()[10], b' ', "{name}: {text}");

    let (value, marked) = get("G").await;
    assert!(
        !marked.iter().any(|p| p == "a.display.precision"),
        "G: {marked:?}"
    );
    assert!(
        marked.iter().any(|p| p == "d.display.precision"),
        "G: {marked:?}"
    );
    assert!(matches!(
        value_at("G", &value, "a.value"),
        PvField::Scalar(ScalarValue::UInt(_))
    ));
    assert!(matches!(
        value_at("G", &value, "b"),
        PvField::Scalar(ScalarValue::String(_))
    ));
    let PvField::Variant(c) = value_at("G", &value, "c") else {
        panic!("G.c: {value:?}");
    };
    let ts = elements("G.c", &c.value);
    assert_eq!(ts.len(), 2, "G.c");
    assert!(matches!(ts[0], ScalarValue::UInt(_)), "G.c: {ts:?}");
    assert!(matches!(
        value_at("G", &value, "d.display.precision"),
        PvField::Scalar(ScalarValue::Int(_))
    ));
}
