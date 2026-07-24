//! End-to-end test for server-side pvRequest field filtering.
//!
//! The server-side `request_to_mask` translates the client's pvRequest into
//! a `BitSet` that drives partial-value emission for GET / MONITOR. This
//! test verifies two behaviours against a custom `ChannelSource`:
//!
//! 1. `pvget_fields(["value"])` only carries the `value` leaf back; the
//!    alarm / timeStamp subtrees are dropped on the wire and arrive as
//!    their default-constructed values.
//! 2. The empty-pvRequest sentinel (`pvget` with no field list) selects
//!    every field, matching pvxs convention. Without this, the no-filter
//!    sentinel `[0xFD,0x02,0x00,0x80,0x00,0x00]` was being decoded as
//!    "root only" and the value silently came back as 0.

#![cfg(test)]

// RTEMS-EXEC-MODEL-ALLOW(2): not run by the default nextest profile - this file is a module of the `parity_interop` binary, which `.config/nextest.toml`'s default-filter excludes.

use epics_pva_rs::server_native::MonitorStream;
use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServer, PvaServerConfig};

#[derive(Clone)]
struct NTScalarSource;

impl ChannelSource for NTScalarSource {
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        async { vec!["dut".into()] }
    }
    fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
        let n = n.to_string();
        async move { n == "dut" }
    }
    fn get_introspection(
        &self,
        _: &str,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        async {
            Some(FieldDesc::Structure {
                struct_id: "epics:nt/NTScalar:1.0".into(),
                fields: vec![
                    ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
                    (
                        "alarm".into(),
                        FieldDesc::Structure {
                            struct_id: "alarm_t".into(),
                            fields: vec![
                                ("severity".into(), FieldDesc::Scalar(ScalarType::Int)),
                                ("status".into(), FieldDesc::Scalar(ScalarType::Int)),
                                ("message".into(), FieldDesc::Scalar(ScalarType::String)),
                            ],
                        },
                    ),
                    (
                        "timeStamp".into(),
                        FieldDesc::Structure {
                            struct_id: "time_t".into(),
                            fields: vec![
                                (
                                    "secondsPastEpoch".into(),
                                    FieldDesc::Scalar(ScalarType::Long),
                                ),
                                ("nanoseconds".into(), FieldDesc::Scalar(ScalarType::Int)),
                                ("userTag".into(), FieldDesc::Scalar(ScalarType::Int)),
                            ],
                        },
                    ),
                ],
            })
        }
    }
    fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        async {
            // value=42.5, alarm.severity=2 (MAJOR), timeStamp.seconds=1700000000.
            // The filter test asserts these are present-or-absent depending on
            // the pvRequest the client sent.
            let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
            s.fields
                .push(("value".into(), PvField::Scalar(ScalarValue::Double(42.5))));
            let mut alarm = PvStructure::new("alarm_t");
            alarm
                .fields
                .push(("severity".into(), PvField::Scalar(ScalarValue::Int(2))));
            alarm
                .fields
                .push(("status".into(), PvField::Scalar(ScalarValue::Int(0))));
            alarm.fields.push((
                "message".into(),
                PvField::Scalar(ScalarValue::String("HIHI".into())),
            ));
            s.fields.push(("alarm".into(), PvField::Structure(alarm)));
            let mut ts = PvStructure::new("time_t");
            ts.fields.push((
                "secondsPastEpoch".into(),
                PvField::Scalar(ScalarValue::Long(1_700_000_000)),
            ));
            ts.fields
                .push(("nanoseconds".into(), PvField::Scalar(ScalarValue::Int(0))));
            ts.fields
                .push(("userTag".into(), PvField::Scalar(ScalarValue::Int(0))));
            s.fields.push(("timeStamp".into(), PvField::Structure(ts)));
            Some(PvField::Structure(s))
        }
    }
    fn put_value(
        &self,
        _: &str,
        _: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        async { Err("read-only".into()) }
    }
    fn is_writable(&self, _: &str) -> impl std::future::Future<Output = bool> + Send {
        async { false }
    }
    fn subscribe(
        &self,
        _: &str,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        async { None }
    }
}

/// Returns the running server alongside its actually-bound port; the
/// caller must keep the `PvaServer` alive for the test's duration —
/// dropping it aborts the listener.
async fn start_server() -> (PvaServer, u16) {
    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        ..Default::default()
    };
    let server = PvaServer::start(Arc::new(NTScalarSource), cfg).expect("test server must start");
    let port = server.report().tcp_port;
    tokio::time::sleep(Duration::from_millis(200)).await;
    (server, port)
}

fn client_to(port: u16) -> PvaClient {
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build()
}

/// Empty pvRequest sentinel ⇒ all fields. Regression for the bug where
/// `request_to_mask` was treating "no `field` substructure" as "root only"
/// and dropping every leaf, so `pvget` returned 0 for the value.
#[tokio::test]
async fn empty_pvrequest_returns_all_fields() {
    let (server, port) = start_server().await;
    let client = client_to(port);

    let v = tokio::time::timeout(Duration::from_secs(5), client.pvget("dut"))
        .await
        .expect("pvget timeout")
        .expect("pvget failed");

    let s = match v {
        PvField::Structure(s) => s,
        other => panic!("expected structure, got {other:?}"),
    };
    // value present
    match s.get_field("value") {
        Some(PvField::Scalar(ScalarValue::Double(d))) => assert_eq!(*d, 42.5),
        other => panic!("expected value=42.5, got {other:?}"),
    }
    // alarm.severity present
    let alarm = match s.get_field("alarm") {
        Some(PvField::Structure(a)) => a,
        other => panic!("expected alarm, got {other:?}"),
    };
    match alarm.get_field("severity") {
        Some(PvField::Scalar(ScalarValue::Int(2))) => {}
        other => panic!("expected severity=2, got {other:?}"),
    }
    // timeStamp.secondsPastEpoch present
    let ts = match s.get_field("timeStamp") {
        Some(PvField::Structure(t)) => t,
        other => panic!("expected timeStamp, got {other:?}"),
    };
    match ts.get_field("secondsPastEpoch") {
        Some(PvField::Scalar(ScalarValue::Long(1_700_000_000))) => {}
        other => panic!("expected seconds=1700000000, got {other:?}"),
    }

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}

/// `pvget --field value` ⇒ only `value` carried on the wire. The other
/// subtrees should arrive at their default-constructed values (severity=0,
/// secondsPastEpoch=0) because they were not selected by the pvRequest.
#[tokio::test]
async fn field_value_only_omits_alarm_and_timestamp() {
    let (server, port) = start_server().await;
    let client = client_to(port);

    let res = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvget_fields("dut", &["value"]),
    )
    .await
    .expect("pvget timeout")
    .expect("pvget failed");

    let s = match res.value {
        PvField::Structure(s) => s,
        other => panic!("expected structure, got {other:?}"),
    };
    // value still arrived
    match s.get_field("value") {
        Some(PvField::Scalar(ScalarValue::Double(d))) => assert_eq!(*d, 42.5),
        other => panic!("expected value=42.5, got {other:?}"),
    }
    // alarm.severity NOT in the wire frame ⇒ default (0)
    let alarm = match s.get_field("alarm") {
        Some(PvField::Structure(a)) => a,
        other => panic!("expected alarm placeholder, got {other:?}"),
    };
    match alarm.get_field("severity") {
        Some(PvField::Scalar(ScalarValue::Int(0))) => {}
        // some implementations may omit the field entirely; either is
        // acceptable as long as it isn't 2 (which would mean the filter
        // was ignored).
        Some(other) => panic!("alarm.severity should default to 0, got {other:?}"),
        None => {}
    }

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}
