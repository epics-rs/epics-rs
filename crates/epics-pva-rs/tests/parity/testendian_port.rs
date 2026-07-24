//! Port of pvxs's `test/testendian.cpp`.
//!
//! Verifies the BE/LE wire-byte-order matrix between server and client.
//! pvxs has 4 cases (server×client = LE×LE, LE×BE, BE×LE, BE×BE). Our
//! client honours whatever the server picks in its SET_BYTE_ORDER
//! control message, so we only need 2 server configurations; the
//! client handshake uses the same order in either direction.

#![cfg(test)]

// RTEMS-EXEC-MODEL-ALLOW(2): not run by the default nextest profile - this file is a module of the `parity_interop` binary, which `.config/nextest.toml`'s default-filter excludes.

use epics_pva_rs::server_native::MonitorStream;
use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::proto::ByteOrder;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServer, PvaServerConfig};

#[derive(Clone)]
struct UInt32Source;

impl ChannelSource for UInt32Source {
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
                fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::UInt))],
            })
        }
    }
    fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        async {
            let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
            s.fields
                .push(("value".into(), PvField::Scalar(ScalarValue::UInt(42))));
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

async fn run_endian_case(server_be: bool) {
    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        wire_byte_order: if server_be {
            ByteOrder::Big
        } else {
            ByteOrder::Little
        },
        ..Default::default()
    };
    let server = PvaServer::start(Arc::new(UInt32Source), cfg).expect("test server must start");
    let port = server.report().tcp_port;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    let client = PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build();

    let v = tokio::time::timeout(Duration::from_secs(5), client.pvget("dut"))
        .await
        .expect("pvget timeout")
        .expect("pvget failed");
    if let PvField::Structure(s) = v {
        match s.get_value() {
            Some(ScalarValue::UInt(n)) => assert_eq!(*n, 42, "server_be={server_be}"),
            other => panic!("expected UInt32, got {other:?}"),
        }
    } else {
        panic!("expected NTScalar structure");
    }

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}

#[tokio::test]
async fn pvxs_test_endian_le_server() {
    run_endian_case(false).await;
}

#[tokio::test]
async fn pvxs_test_endian_be_server() {
    run_endian_case(true).await;
}
