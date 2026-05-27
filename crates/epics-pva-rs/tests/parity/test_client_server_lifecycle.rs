//! End-to-end coverage for the v0.10.4 lifecycle additions:
//! `PvaClient::{close, hurry_up, cache_clear, ignore_server_guids}`
//! and `PvaServer::{start, stop, wait}` (mirroring pvxs `Context`
//! and `Server` public surface).

#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServer, PvaServerConfig};

#[derive(Clone)]
struct ConstSource;

impl ChannelSource for ConstSource {
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
                fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Int))],
            })
        }
    }
    fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        async {
            let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
            s.fields
                .push(("value".into(), PvField::Scalar(ScalarValue::Int(7))));
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
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
        async { None }
    }
}

static NEXT_PORT: AtomicU16 = AtomicU16::new(48000);
fn alloc_port() -> (u16, u16) {
    let base = NEXT_PORT.fetch_add(2, Ordering::Relaxed);
    (base, base + 1)
}

fn client_to(port: u16) -> PvaClient {
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
    PvaClient::builder()
        .timeout(Duration::from_secs(3))
        .server_addr(server_addr)
        .build()
}

/// `PvaServer::stop` ends the listener so subsequent connect attempts
/// fail. Mirrors pvxs `Server::stop` at the "no new connections"
/// granularity.
#[tokio::test]
async fn pva_server_stop_ends_listener() {
    let (port, udp) = alloc_port();
    let cfg = PvaServerConfig {
        tcp_port: port,
        udp_port: udp,
        ..Default::default()
    };
    let server = PvaServer::start(Arc::new(ConstSource), cfg).expect("test server must start");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Healthy server: pvget succeeds.
    let client = client_to(port);
    let v = tokio::time::timeout(Duration::from_secs(3), client.pvget("dut"))
        .await
        .expect("first pvget timeout")
        .expect("first pvget failed");
    assert!(matches!(v, PvField::Structure(_)));

    // Stop and wait for both background tasks to finish (cancel paths
    // map to Ok, panics map to Err).
    server.stop();
    tokio::time::timeout(Duration::from_secs(2), server.wait())
        .await
        .expect("server.wait() timed out — stop did not complete")
        .expect("server.wait() returned Err");

    // Fresh client to the now-stopped port: TCP connect refuses (or
    // the test framework times out — either way, no successful pvget).
    let client2 = client_to(port);
    let res = tokio::time::timeout(Duration::from_millis(800), client2.pvget("dut")).await;
    assert!(
        matches!(res, Err(_) | Ok(Err(_))),
        "pvget should fail/timeout after stop, got {res:?}"
    );
}

/// `close()` clears the channel cache and the connection pool — a
/// subsequent `pvget` must re-resolve and re-connect. We verify by
/// checking that the second pvget still succeeds even after the
/// in-memory client state was nuked. Mirrors pvxs `Context::close`.
#[tokio::test]
async fn pva_client_close_then_reuse_succeeds() {
    let (port, udp) = alloc_port();
    let cfg = PvaServerConfig {
        tcp_port: port,
        udp_port: udp,
        ..Default::default()
    };
    let server = PvaServer::start(Arc::new(ConstSource), cfg).expect("test server must start");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = client_to(port);
    client.pvget("dut").await.expect("pre-close pvget");
    client.close(); // Drops cached channel + pool entry.

    // The exact same PvaClient handle still functions: it transparently
    // re-creates the channel + connection on the next op.
    let v = tokio::time::timeout(Duration::from_secs(3), client.pvget("dut"))
        .await
        .expect("post-close pvget timeout")
        .expect("post-close pvget failed");
    assert!(matches!(v, PvField::Structure(_)));

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server.wait()).await;
}

/// `hurry_up`, `cache_clear`, `ignore_server_guids` are all no-ops
/// when the client is in direct-server mode (no SearchEngine). They
/// must complete cleanly without panicking — pvxs `Context` API
/// stays callable in fixed-server deployments too.
#[tokio::test]
async fn lifecycle_methods_are_safe_in_direct_server_mode() {
    // No server running — direct-mode client just exercises the API
    // surface. None of these should panic or block.
    let client = PvaClient::builder()
        .server_addr(std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            1, // unused
        ))
        .build();

    client.hurry_up().await;
    client.cache_clear("nonexistent").await;
    client.ignore_server_guids(vec![[0xAB; 12]]).await;
    client.ignore_server_guids(Vec::new()).await; // clear list
    client.close();
}
