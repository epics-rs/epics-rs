//! Integration tests for the PVA-to-PVA gateway.
//!
//! Topology:
//!
//! ```text
//!   [PvaClient] ─── PVA ───▶ [PvaGateway downstream]
//!                                 │
//!                                 ▼ (cache)
//!                          [PvaGateway upstream PvaClient]
//!                                 │
//!                                 ▼ PVA
//!                          [PvaServer with SharedPV]
//! ```
//!
//! Verifies: GET, MONITOR fan-out (single upstream subscription
//! shared across multiple downstream clients), and that
//! disappearing downstream subscribers don't abort the upstream
//! monitor task.

#![cfg(feature = "pva-gateway")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use epics_bridge_rs::pva_gateway::{
    ChannelCache, GatewayChannelSource, MultiTenantPvaGatewayBuilder, PvaGateway, PvaGatewayConfig,
};
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pvdata::{
    FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, TypedScalarArray,
};
use epics_pva_rs::server_native::{
    ChannelSource, PvaServer, PvaServerConfig, SharedPV, SharedSource,
};

/// `epics:nt/NTScalar:1.0` descriptor wrapping a `double value` —
/// the shape every real IOC / QSRV PV exposes. A bare top-level
/// `Scalar` is not a valid pvRequest narrowing target (`field(value)`
/// resolves to nothing — pvxs `request2mask` rejects it identically),
/// so the gateway integration fixture must serve a structured PV.
fn nt_double_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
    }
}

fn nt_double_value(v: f64) -> PvField {
    let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
    s.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Double(v))));
    PvField::Structure(s)
}

/// Build a 1-PV upstream PvaServer on a random loopback port and
/// return (server, addr, shared_pv).
fn spawn_upstream(pv_name: &str, initial: f64) -> (PvaServer, SocketAddr, SharedPV) {
    let pv = SharedPV::new();
    pv.open(nt_double_desc(), nt_double_value(initial)).unwrap();
    let source = SharedSource::new();
    source.add(pv_name, pv.clone());

    let pick = || {
        let l = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let pick_udp = || {
        let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let cfg = PvaServerConfig {
        tcp_port: pick(),
        udp_port: pick_udp(),
        ..PvaServerConfig::isolated()
    };
    let bound = SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        cfg.tcp_port,
    );
    let server = PvaServer::start(Arc::new(source), cfg).expect("test server must start");
    (server, bound, pv)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_get_forwards_upstream_value() {
    let (_us_server, us_addr, us_pv) = spawn_upstream("GW:GET:PV", 42.5);
    // Upstream client pinned at the test server.
    let upstream_client = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );

    let pick = || {
        let l = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let pick_udp = || {
        let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let server_config = PvaServerConfig {
        tcp_port: pick(),
        udp_port: pick_udp(),
        ..PvaServerConfig::isolated()
    };
    let cfg = PvaGatewayConfig {
        upstream_client: Some(upstream_client),
        server_config,
        cleanup_interval: Duration::from_secs(60),
        connect_timeout: Duration::from_secs(2),
        max_cache_entries: 1024,
        max_subscribers: 1024,
        control_prefix: None,
        read_only: false,
        acl: None,
        audit: None,
    };
    let gw = PvaGateway::start(cfg).expect("gateway start");

    // Downstream client pinned at the gateway.
    let ds = gw.client_config();
    let result = ds.pvget_full("GW:GET:PV").await.expect("downstream get");
    match result.value {
        PvField::Scalar(ScalarValue::Double(v)) => assert_eq!(v, 42.5),
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::Double(v))) => assert_eq!(*v, 42.5),
            other => panic!("unexpected NTScalar value: {other:?}"),
        },
        other => panic!("unexpected value shape: {other:?}"),
    }

    // Sanity: upstream PV was not touched (we only read).
    assert!(us_pv.is_open());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_monitor_fans_out_to_two_clients() {
    let (_us_server, us_addr, us_pv) = spawn_upstream("GW:MON:PV", 0.0);
    let upstream_client = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );

    let pick = || {
        let l = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let pick_udp = || {
        let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let server_config = PvaServerConfig {
        tcp_port: pick(),
        udp_port: pick_udp(),
        ..PvaServerConfig::isolated()
    };
    let cfg = PvaGatewayConfig {
        upstream_client: Some(upstream_client),
        server_config,
        cleanup_interval: Duration::from_secs(60),
        connect_timeout: Duration::from_secs(2),
        max_cache_entries: 1024,
        max_subscribers: 1024,
        control_prefix: None,
        read_only: false,
        acl: None,
        audit: None,
    };
    let gw = PvaGateway::start(cfg).expect("gateway start");

    // Two independent downstream clients, both pointed at gateway.
    let c1 = gw.client_config();
    let c2 = gw.client_config();

    let (tx1, mut rx1) = tokio::sync::mpsc::channel::<f64>(8);
    let (tx2, mut rx2) = tokio::sync::mpsc::channel::<f64>(8);

    let h1 = tokio::spawn(async move {
        let _ = c1
            .pvmonitor("GW:MON:PV", move |value| {
                if let Some(d) = scalar_double(value) {
                    let _ = tx1.try_send(d);
                }
            })
            .await;
    });
    let h2 = tokio::spawn(async move {
        let _ = c2
            .pvmonitor("GW:MON:PV", move |value| {
                if let Some(d) = scalar_double(value) {
                    let _ = tx2.try_send(d);
                }
            })
            .await;
    });

    // Let the subscriptions establish.
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Drain initial events (both clients must see the seed value).
    let initial1 = recv_within(&mut rx1, Duration::from_secs(2))
        .await
        .expect("client 1 initial");
    let initial2 = recv_within(&mut rx2, Duration::from_secs(2))
        .await
        .expect("client 2 initial");
    assert_eq!(initial1, 0.0);
    assert_eq!(initial2, 0.0);

    // Push three updates upstream; both downstream clients should see
    // each one. We treat "received the last value" as success since
    // an under-loaded test runner can squash to-latest.
    for v in [1.0_f64, 2.0, 3.0] {
        us_pv.try_post(nt_double_value(v));
        // tiny breather so the broadcast fan-out keeps up.
        tokio::time::sleep(Duration::from_millis(80)).await;
    }

    let last1 = drain_to_latest(&mut rx1, Duration::from_secs(3))
        .await
        .expect("client 1 saw an update");
    let last2 = drain_to_latest(&mut rx2, Duration::from_secs(3))
        .await
        .expect("client 2 saw an update");
    assert_eq!(last1, 3.0);
    assert_eq!(last2, 3.0);

    h1.abort();
    h2.abort();
}

fn scalar_double(field: &PvField) -> Option<f64> {
    match field {
        PvField::Scalar(ScalarValue::Double(d)) => Some(*d),
        PvField::Structure(s) => match s.get_field("value")? {
            PvField::Scalar(ScalarValue::Double(d)) => Some(*d),
            _ => None,
        },
        _ => None,
    }
}

async fn recv_within(rx: &mut tokio::sync::mpsc::Receiver<f64>, timeout: Duration) -> Option<f64> {
    tokio::time::timeout(timeout, rx.recv())
        .await
        .ok()
        .flatten()
}

async fn drain_to_latest(
    rx: &mut tokio::sync::mpsc::Receiver<f64>,
    timeout: Duration,
) -> Option<f64> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last: Option<f64> = None;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(150), rx.recv()).await {
            Ok(Some(v)) => last = Some(v),
            Ok(None) => break,
            Err(_) => {
                if last.is_some() {
                    break;
                }
            }
        }
    }
    last
}

/// when `control_prefix` is set, downstream clients should be
/// able to `pvget <prefix>:cacheSize` and read the live cache entry
/// count without that name being forwarded upstream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_control_prefix_cache_size() {
    let (_us_server, us_addr, _us_pv) = spawn_upstream("GW:CTRL:PV", 1.0);
    let upstream_client = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );

    let pick = || {
        let l = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let pick_udp = || {
        let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let server_config = PvaServerConfig {
        tcp_port: pick(),
        udp_port: pick_udp(),
        ..PvaServerConfig::isolated()
    };
    let cfg = PvaGatewayConfig {
        upstream_client: Some(upstream_client),
        server_config,
        cleanup_interval: Duration::from_secs(60),
        connect_timeout: Duration::from_secs(2),
        max_cache_entries: 1024,
        max_subscribers: 1024,
        control_prefix: Some("gw".to_string()),
        read_only: false,
        acl: None,
        audit: None,
    };
    let gw = PvaGateway::start(cfg).expect("gateway start");

    let ds = gw.client_config();

    // Initial cache is empty.
    let snap = ds.pvget_full("gw:cacheSize").await.expect("cacheSize get");
    let v = match snap.value {
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::Long(v))) => *v,
            other => panic!("unexpected cacheSize value shape: {other:?}"),
        },
        other => panic!("unexpected cacheSize wrapper: {other:?}"),
    };
    assert_eq!(v, 0, "fresh gateway cache is empty before any proxy GET");

    // Establish a downstream MONITOR to populate the cache, then re-read
    // cacheSize. A monitor (not a GET) warms the gateway monitor cache:
    // since the GET path forwards an upstream ChannelGet and no longer
    // touches the subscription-keyed monitor cache, only a monitor
    // inserts a cache entry. Keep the subscription alive across the read.
    let mon = gw.client_config();
    let (tx, _rx) = tokio::sync::mpsc::channel::<f64>(8);
    let mon_task = tokio::spawn(async move {
        let _ = mon
            .pvmonitor("GW:CTRL:PV", move |value| {
                if let Some(d) = scalar_double(value) {
                    let _ = tx.try_send(d);
                }
            })
            .await;
    });

    // Let the subscription establish so the gateway inserts a cache entry.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let snap = ds
        .pvget_full("gw:cacheSize")
        .await
        .expect("cacheSize get post-monitor");
    let v = match snap.value {
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::Long(v))) => *v,
            other => panic!("unexpected cacheSize value shape: {other:?}"),
        },
        other => panic!("unexpected cacheSize wrapper: {other:?}"),
    };
    assert!(v >= 1, "cacheSize should reflect the monitored PV; got {v}");

    mon_task.abort();
}

/// a multi-tenant gateway with two upstreams (each holding a
/// distinct PV) and one downstream that proxies both. Verifies
/// per-upstream isolation: PV "A:VAL" is only on upstream A, PV
/// "B:VAL" only on B, and a single downstream client reaches both
/// through the gateway.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_tenant_gateway_routes_to_correct_upstream() {
    let (_us_a, addr_a, _pv_a) = spawn_upstream("A:VAL", 1.0);
    let (_us_b, addr_b, _pv_b) = spawn_upstream("B:VAL", 2.0);

    let client_a = Arc::new(
        PvaClient::builder()
            .server_addr(addr_a)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    let client_b = Arc::new(
        PvaClient::builder()
            .server_addr(addr_b)
            .timeout(Duration::from_secs(2))
            .build(),
    );

    let pick = || {
        let l = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let pick_udp = || {
        let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let server_config = PvaServerConfig {
        tcp_port: pick(),
        udp_port: pick_udp(),
        ..PvaServerConfig::isolated()
    };

    let gw = MultiTenantPvaGatewayBuilder::new()
        .add_upstream("A", client_a)
        .add_upstream("B", client_b)
        .add_downstream("merged", server_config, &["A", "B"], None)
        .connect_timeout(Duration::from_secs(2))
        .start()
        .expect("multi-tenant start");

    assert_eq!(gw.upstream_count(), 2);
    assert_eq!(gw.downstream_count(), 1);

    // Build a downstream client pointed at the "merged" server.
    let server = gw.downstream("merged").expect("merged server present");
    let ds = server.client_config();

    let snap = ds.pvget_full("A:VAL").await.expect("A:VAL via gateway");
    let v = match snap.value {
        PvField::Scalar(ScalarValue::Double(v)) => v,
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::Double(v))) => *v,
            other => panic!("unexpected A:VAL shape: {other:?}"),
        },
        other => panic!("unexpected A:VAL wrapper: {other:?}"),
    };
    assert_eq!(v, 1.0);

    let snap = ds.pvget_full("B:VAL").await.expect("B:VAL via gateway");
    let v = match snap.value {
        PvField::Scalar(ScalarValue::Double(v)) => v,
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::Double(v))) => *v,
            other => panic!("unexpected B:VAL shape: {other:?}"),
        },
        other => panic!("unexpected B:VAL wrapper: {other:?}"),
    };
    assert_eq!(v, 2.0);
}

/// Regression: a per-downstream ACL deny list installed via
/// `downstream_access` must short-circuit a denied PV name before it
/// reaches *any* of that downstream's upstreams — and an allowed name
/// must still resolve. Pre-fix the multi-tenant builder wrapped each
/// proxy in NO middleware (`Arc::new(src)` straight into the
/// composite), so the deny list was silently inert and "SECRET:VAL"
/// resolved.
///
/// Boundary cases (one assertion each):
/// - denied name ("SECRET:VAL", upstream S) → must NOT resolve
/// - allowed name ("OPEN:VAL", upstream O)  → must resolve to 5.0
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_tenant_downstream_acl_denies_pv() {
    let (_us_s, addr_s, _pv_s) = spawn_upstream("SECRET:VAL", 3.0);
    let (_us_o, addr_o, _pv_o) = spawn_upstream("OPEN:VAL", 5.0);

    let client_s = Arc::new(
        PvaClient::builder()
            .server_addr(addr_s)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    let client_o = Arc::new(
        PvaClient::builder()
            .server_addr(addr_o)
            .timeout(Duration::from_secs(2))
            .build(),
    );

    let pick = || {
        let l = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let pick_udp = || {
        let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let server_config = PvaServerConfig {
        tcp_port: pick(),
        udp_port: pick_udp(),
        ..PvaServerConfig::isolated()
    };

    let acl = epics_bridge_rs::pva_gateway::AclConfig::default()
        .deny_regex(r"SECRET:.*")
        .unwrap();

    let gw = MultiTenantPvaGatewayBuilder::new()
        .add_upstream("S", client_s)
        .add_upstream("O", client_o)
        .add_downstream("restricted", server_config, &["S", "O"], None)
        .downstream_access(Some(acl), false, None)
        .connect_timeout(Duration::from_secs(2))
        .start()
        .expect("multi-tenant start");

    let server = gw
        .downstream("restricted")
        .expect("restricted server present");
    let ds = server.client_config();

    // Denied: the ACL short-circuits before reaching upstream S.
    let denied = tokio::time::timeout(Duration::from_secs(3), ds.pvget_full("SECRET:VAL")).await;
    match denied {
        Ok(Ok(snap)) => panic!("ACL-denied PV must not resolve: got {:?}", snap.value),
        Ok(Err(_)) | Err(_) => { /* denied / timed out — correct */ }
    }

    // Allowed: the same downstream still resolves an un-denied PV.
    let snap = ds
        .pvget_full("OPEN:VAL")
        .await
        .expect("OPEN:VAL via gateway");
    let v = match snap.value {
        PvField::Scalar(ScalarValue::Double(v)) => v,
        PvField::Structure(s) => match s.get_field("value") {
            Some(PvField::Scalar(ScalarValue::Double(v))) => *v,
            other => panic!("unexpected OPEN:VAL shape: {other:?}"),
        },
        other => panic!("unexpected OPEN:VAL wrapper: {other:?}"),
    };
    assert_eq!(v, 5.0);
}

/// Regression: a per-downstream `read_only` flag set via
/// `downstream_access` must reject every PUT on that downstream. Pre-fix
/// the multi-tenant builder inserted no `ReadOnlyLayer`, so a `read_only`
/// downstream silently forwarded PUTs upstream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_tenant_downstream_read_only_rejects_put() {
    let (_us, us_addr, us_pv) = spawn_upstream("MT:RO:PV", 7.0);
    let upstream = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );

    let pick = || {
        let l = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let pick_udp = || {
        let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let server_config = PvaServerConfig {
        tcp_port: pick(),
        udp_port: pick_udp(),
        ..PvaServerConfig::isolated()
    };

    let gw = MultiTenantPvaGatewayBuilder::new()
        .add_upstream("U", upstream)
        .add_downstream("ro", server_config, &["U"], None)
        .downstream_access(None, true, None)
        .connect_timeout(Duration::from_secs(2))
        .start()
        .expect("multi-tenant start");

    let server = gw.downstream("ro").expect("ro server present");
    let ds = server.client_config();
    let err = ds
        .pvput("MT:RO:PV", "99")
        .await
        .expect_err("PUT through a read-only downstream must be rejected");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("read-only"),
        "rejection must come from the ReadOnly layer: {msg}"
    );

    // The upstream PV value is untouched — the PUT never reached it.
    let current = us_pv.current();
    match current.as_ref().and_then(scalar_double) {
        Some(v) => assert_eq!(v, 7.0, "read-only downstream must not forward the PUT"),
        None => panic!("unexpected upstream value: {current:?}"),
    }
}

// ── gateway middleware (ReadOnly / ACL / Audit) wiring ──

/// Build a gateway config pinned at `upstream` with an isolated
/// random-port downstream server.
fn gateway_cfg(upstream: Arc<PvaClient>) -> PvaGatewayConfig {
    let pick = || {
        let l = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let pick_udp = || {
        let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    PvaGatewayConfig {
        upstream_client: Some(upstream),
        server_config: PvaServerConfig {
            tcp_port: pick(),
            udp_port: pick_udp(),
            ..PvaServerConfig::isolated()
        },
        cleanup_interval: Duration::from_secs(60),
        connect_timeout: Duration::from_secs(2),
        max_cache_entries: 1024,
        max_subscribers: 1024,
        control_prefix: None,
        read_only: false,
        acl: None,
        audit: None,
    }
}

/// Regression: a `read_only` gateway must reject every
/// downstream PUT. Pre-fix the `ReadOnlyLayer` was never inserted by
/// `PvaGateway::start`, so a `read_only` deployment silently
/// forwarded PUTs to the upstream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn critical1_read_only_gateway_rejects_put() {
    let (_us, us_addr, us_pv) = spawn_upstream("GW:RO:PV", 7.0);
    let upstream = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    let mut cfg = gateway_cfg(upstream);
    cfg.read_only = true;
    let gw = PvaGateway::start(cfg).expect("read-only gateway start");

    let ds = gw.client_config();
    let err = ds
        .pvput("GW:RO:PV", "99")
        .await
        .expect_err("PUT through a read-only gateway must be rejected");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("read-only"),
        "rejection must come from the ReadOnly layer: {msg}"
    );

    // The upstream PV value is untouched — the PUT never reached it.
    let current = us_pv.current();
    match current.as_ref().and_then(scalar_double) {
        Some(v) => assert_eq!(v, 7.0, "read-only gateway must not forward the PUT"),
        None => panic!("unexpected upstream value: {current:?}"),
    }
}

/// Regression: an ACL deny list installed on the gateway
/// config must short-circuit a denied PV name before it reaches the
/// upstream — `has_pv` / GET return "not found" at the `AclLayer`.
/// Pre-fix the `AclLayer` was never inserted, so the deny list was
/// silently inert.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn critical1_acl_layer_denies_pv() {
    let (_us, us_addr, _pv) = spawn_upstream("SECRET:PV", 3.0);
    let upstream = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    let mut cfg = gateway_cfg(upstream);
    cfg.acl = Some(
        epics_bridge_rs::pva_gateway::AclConfig::default()
            .deny_regex(r"SECRET:.*")
            .unwrap(),
    );
    let gw = PvaGateway::start(cfg).expect("acl gateway start");

    let ds = gw.client_config();
    // The ACL-denied PV must not resolve through the gateway.
    let result = tokio::time::timeout(Duration::from_secs(3), ds.pvget_full("SECRET:PV")).await;
    match result {
        Ok(Ok(snap)) => panic!("ACL-denied PV must not resolve: got {:?}", snap.value),
        Ok(Err(_)) | Err(_) => { /* denied / timed out — correct */ }
    }
}

/// Regression: an audit sink installed on the gateway
/// config receives a PUT audit event. The gateway is also `read_only`
/// here, which exercises layer ORDERING: `Audit` is outermost, so it
/// records the PUT even though the inner `ReadOnly` layer rejected it
/// — the event lands as `Denied`. Pre-fix the `AuditLayer` was never
/// inserted, so the PUT audit trail was silently empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn critical1_audit_layer_records_put() {
    use epics_bridge_rs::pva_gateway::{AuditEventKind, AuditResult, ClosureAudit};

    let (_us, us_addr, _pv) = spawn_upstream("GW:AUDIT:PV", 0.0);
    let upstream = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );

    let events: Arc<std::sync::Mutex<Vec<(String, AuditEventKind, AuditResult)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_events = events.clone();
    let mut cfg = gateway_cfg(upstream);
    cfg.read_only = true;
    cfg.audit = Some(Arc::new(ClosureAudit(move |ev| {
        sink_events
            .lock()
            .unwrap()
            .push((ev.pv, ev.event, ev.result));
    })));
    let gw = PvaGateway::start(cfg).expect("audit gateway start");

    let ds = gw.client_config();
    // PUT is rejected by the inner ReadOnly layer; the outer Audit
    // layer must still record it.
    let _ = ds.pvput("GW:AUDIT:PV", "12").await;

    let recorded = events.lock().unwrap();
    assert!(
        recorded.iter().any(|(pv, kind, res)| pv == "GW:AUDIT:PV"
            && *kind == AuditEventKind::Put
            && *res == AuditResult::Denied),
        "Audit layer (outermost) must record the ReadOnly-denied PUT \
         for GW:AUDIT:PV as Denied; got {recorded:?}"
    );
}

/// typed PUT pass-through. Pre-fix the gateway re-encoded every
/// upstream PUT through `pvfield_to_pvput_string`; the function recursed
/// into the "value" sub-field and joined String array elements with spaces
/// before passing to `pvput`, which splits on commas —
/// so `value: ["hello world", "foo bar"]` became `value: ["hello world foo bar"]`
/// (1 element instead of 2). After fix the gateway calls `pvput_pv_field`
/// (typed), forwarding the PvField as-is.
///
/// Upstream parity: pvxs/src/clientget.cpp:305 — `to_wire_valid(R, temp)`
/// (no string encoding in the pvxs PUT path).
///
/// Fails on main: `pvfield_to_pvput_string` recurses to "value", space-joins
/// the array → `pvput` → `build_put_value` splits on commas → 1 element stored.
/// Passes after fix: `pvput_pv_field` sends the typed structure intact.
///
/// Calls `GatewayChannelSource::put_value` directly to avoid the
/// per-credential upstream-client routing issue: when the downstream
/// client sends "ca" auth with a non-empty OS username, `upstream_client_for`
/// creates a new client without `server_addr`, which falls through to
/// `Resolver::Search` and times out against the isolated test server.
/// Testing via the source directly stays on the shared cache client
/// (Direct resolver → always works) and still exercises the exact
/// `put_value` code path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn br_r6_gateway_typed_put_passthrough() {
    // Upstream: a structure with a "value: String[]" sub-field.
    // request_to_mask succeeds for field(value) on this shape (server
    // finds the "value" entry), so the upstream monitor starts cleanly.
    let pv = SharedPV::new();
    let desc = FieldDesc::Structure {
        struct_id: "test:strarray/1.0".to_string(),
        fields: vec![(
            "value".to_string(),
            FieldDesc::ScalarArray(ScalarType::String),
        )],
    };
    let initial = PvField::Structure({
        let mut s = PvStructure::new("test:strarray/1.0");
        s.set(
            "value",
            PvField::ScalarArray(vec![ScalarValue::String("init".to_string())]),
        );
        s
    });
    pv.open(desc, initial).unwrap();
    let source = SharedSource::new();
    source.add("BR:R6:PV", pv.clone());

    let pick = || {
        let l = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let pick_udp = || {
        let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let us_cfg = PvaServerConfig {
        tcp_port: pick(),
        udp_port: pick_udp(),
        ..PvaServerConfig::isolated()
    };
    let us_addr = std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        us_cfg.tcp_port,
    );
    let _us = PvaServer::start(Arc::new(source), us_cfg).expect("upstream start");

    let upstream_client = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    // Use GatewayChannelSource directly — same put_value code path, no
    // downstream auth layer that would route to a per-credential client.
    let cache = ChannelCache::new(upstream_client, Duration::from_secs(60));
    let mut src = GatewayChannelSource::new(cache);
    src.connect_timeout = Duration::from_secs(2);

    // Two strings with spaces: pvfield_to_pvput_string (main) recurses into
    // "value", joins with spaces → "hello world foo bar", then pvput's
    // comma-split yields 1 element. pvput_pv_field (fix) preserves both.
    let put_value = PvField::Structure({
        let mut s = PvStructure::new("test:strarray/1.0");
        s.set(
            "value",
            PvField::ScalarArray(vec![
                ScalarValue::String("hello world".to_string()),
                ScalarValue::String("foo bar".to_string()),
            ]),
        );
        s
    });
    src.put_value("BR:R6:PV", put_value)
        .await
        .expect("typed structure PUT through gateway source must succeed");

    let stored = pv.current().expect("upstream PV must have a current value");
    // Decode the string array from whatever PvField variant the server
    // stores. The server decodes wire bytes into ScalarArrayTyped (Arc-backed);
    // ScalarArray (Vec-backed) is the client-side construction form.
    let stored_strs: Option<Vec<String>> = match stored {
        PvField::Structure(ref s) => match s.get_field("value") {
            Some(PvField::ScalarArrayTyped(TypedScalarArray::String(arr))) => Some(arr.to_vec()),
            Some(PvField::ScalarArray(vals)) => Some(
                vals.iter()
                    .filter_map(|v| {
                        if let ScalarValue::String(s) = v {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                    .collect(),
            ),
            other => panic!("unexpected value shape in structure: {other:?}"),
        },
        other => panic!("unexpected top-level PvField: {other:?}"),
    };
    assert_eq!(
        stored_strs,
        Some(vec!["hello world".to_string(), "foo bar".to_string()]),
        "upstream must hold both array elements intact \
         (pre-fix space-join collapses to 1 element)",
    );
}

/// typed-subscribe fallback delivers live updates.
///
/// `GatewayChannelSource::subscribe` routes through `subscribe_inner`
/// which bridges a `broadcast::Receiver<PvField>` (from `entry.subscribe()`)
/// to an mpsc channel. Pre-fix the typed broadcast sender `tx_inner` was
/// dropped before the `pvmonitor_raw_frames_handle` callback ran
/// (`let _ = tx_inner; // typed broadcast retired in raw path`), so
/// `bcast_rx.recv()` blocked forever after the initial snapshot. Downstream
/// monitors using a pvRequest that forces the decoded fallback (masked
/// fields, pipelined, filtered, or EPICS_PVA_GW_RAW_FRAMES=NO) received
/// only the first value — never further updates.
///
/// Fails on main: `tx_inner` dropped → second value never sent → timeout.
/// Passes after fix: `tx_inner` moves into callback, `tx_inner.send(val)`
/// fires after each decoded event → update reaches the subscriber.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn br_r41_typed_subscribe_delivers_updates() {
    // Upstream: Structure { value: Double } — field(value) pvRequest
    // succeeds for this shape, so the upstream monitor starts cleanly.
    let pv = SharedPV::new();
    let desc = FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![("value".to_string(), FieldDesc::Scalar(ScalarType::Double))],
    };
    let initial = PvField::Structure({
        let mut s = PvStructure::new("");
        s.set("value", PvField::Scalar(ScalarValue::Double(1.0)));
        s
    });
    pv.open(desc, initial).unwrap();
    let source = SharedSource::new();
    source.add("BR:R41:PV", pv.clone());

    let pick = || {
        let l = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let pick_udp = || {
        let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let us_cfg = PvaServerConfig {
        tcp_port: pick(),
        udp_port: pick_udp(),
        ..PvaServerConfig::isolated()
    };
    let us_addr = std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        us_cfg.tcp_port,
    );
    let _us = PvaServer::start(Arc::new(source), us_cfg).expect("upstream start");

    let upstream_client = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    let cache = ChannelCache::new(upstream_client, Duration::from_secs(60));
    let mut src = GatewayChannelSource::new(cache);
    src.connect_timeout = Duration::from_secs(2);

    // subscribe() uses subscribe_inner → typed broadcast fallback path.
    let mut rx = src
        .subscribe("BR:R41:PV")
        .await
        .expect("subscribe must return Some for a known PV");

    // First receive: initial snapshot from entry.snapshot().
    let snap = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("initial snapshot must arrive within 2s")
        .expect("channel must be open");
    assert_eq!(
        scalar_double(&snap),
        Some(1.0),
        "initial snapshot must be 1.0"
    );

    // Post an update upstream; the monitor callback fires, apply_monitor_event
    // decodes it, and (after fix) tx_inner sends it to the typed broadcast.
    let update_val = PvField::Structure({
        let mut s = PvStructure::new("");
        s.set("value", PvField::Scalar(ScalarValue::Double(42.0)));
        s
    });
    pv.try_post(update_val);

    // Drain until 42.0 arrives or 2 s elapses. The server's decoded
    // path seeds new subscribers with the current value AND sends an
    // explicit initial snapshot, so one extra 1.0 may arrive before
    // the 42.0 update. Pre-fix: tx_inner was dropped → broadcast
    // permanently empty → this loop times out.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let update = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let v = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("update must arrive within 2s (pre-fix tx_inner dropped → blocks forever)")
            .expect("channel must be open");
        if scalar_double(&v) == Some(42.0) {
            break v;
        }
    };
    assert_eq!(
        scalar_double(&update),
        Some(42.0),
        "typed subscribe must deliver the post-initial update",
    );
}
