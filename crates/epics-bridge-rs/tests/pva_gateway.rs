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

#![cfg(all(feature = "pva-gateway", tokio_backend))]

// The 25 cases below used to run under `EPICS_RS_BUILD_EXEC_BACKEND=thread
// --features pva-gateway` and passed there, which is what the census marker
// they carried recorded. They no longer compile in that combination:
// `PvaServer` moved from the target gate to `tokio_backend`, so the gateway
// this file stands does not exist on the reactor-free backend at all. The
// coverage they gave that combination is gone with it, not merely unaccounted
// for.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// The executor a test body spawns on. Every `#[tokio::test]` here is
/// already inside one; the gateway constructors now take it by value
/// instead of looking it up behind the caller's back.
fn test_reactor() -> epics_base_rs::runtime::task::Reactor {
    epics_base_rs::runtime::task::Reactor::current()
        .expect("this test body runs inside an executor")
}

use epics_bridge_rs::pva_gateway::{
    ChannelCache, GatewayChannelSource, MultiTenantPvaGatewayBuilder, PvaGateway, PvaGatewayConfig,
};
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::proto::BitSet;
use epics_pva_rs::pvdata::{
    FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, TypedScalarArray,
};
use epics_pva_rs::server_native::source::{AccessChecked, ChannelContext, OpError, SourceRead};
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
        control_acf_path: None,
        control_reload_acf_path: None,
    };
    let gw = PvaGateway::start(&test_reactor(), cfg).expect("gateway start");

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
        control_acf_path: None,
        control_reload_acf_path: None,
    };
    let gw = PvaGateway::start(&test_reactor(), cfg).expect("gateway start");

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

/// Wire-level single-seed through the gateway: a downstream MONITOR
/// START must deliver the connect-time value EXACTLY ONCE.
///
/// The gateway self-seeds: `subscribe_raw_inner` / `subscribe_inner`
/// used to push `entry.snapshot()` into the downstream stream before
/// forwarding upstream events, while the native server ALSO emitted its
/// own connect-time snapshot. A downstream monitor therefore saw the
/// current value twice at START.
///
/// The single MONITOR seed owner (`subscribe_seeded` /
/// `subscribe_raw_seeded`) returns the cached snapshot as the seed plus
/// an updates-only stream, captured atomically with the subscriber so
/// the broadcast's own copy of the snapshot is deduped out. The server
/// emits exactly one initial frame. Mirrors pva2pva, which copies one
/// `lastelem` per `start()` (`moncache.cpp:270-320`).
///
/// Pre-fix: two identical seed frames at START. Post-fix: exactly one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_155_monitor_seeds_current_value_once() {
    let (_us_server, us_addr, us_pv) = spawn_upstream("GW:SEED155:PV", 7.0);
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
        control_acf_path: None,
        control_reload_acf_path: None,
    };
    let gw = PvaGateway::start(&test_reactor(), cfg).expect("gateway start");
    let client = gw.client_config();

    let received = Arc::new(std::sync::Mutex::new(Vec::<f64>::new()));
    let cb = received.clone();
    let h = tokio::spawn(async move {
        let _ = client
            .pvmonitor("GW:SEED155:PV", move |value| {
                if let Some(d) = scalar_double(value) {
                    cb.lock().unwrap().push(d);
                }
            })
            .await;
    });

    // Wait until the seed has arrived (up to 3 s on loopback), then a
    // short extra settle so a regressed SECOND seed frame — emitted
    // back-to-back with the first — would also land before we count.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while received.lock().unwrap().is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        *received.lock().unwrap(),
        vec![7.0],
        "downstream MONITOR START must deliver the connect-time value exactly once \
         (double-seed regression delivers it twice)"
    );

    // One real upstream post delivers exactly one more frame.
    us_pv.try_post(nt_double_value(8.0));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while received.lock().unwrap().len() < 2 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        *received.lock().unwrap(),
        vec![7.0, 8.0],
        "after one upstream post the wire carries seed(7.0) then update(8.0) — no duplicate seed"
    );

    h.abort();
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
        control_acf_path: None,
        control_reload_acf_path: None,
    };
    let gw = PvaGateway::start(&test_reactor(), cfg).expect("gateway start");

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
        .start(&test_reactor())
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
        .start(&test_reactor())
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
        .start(&test_reactor())
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
        control_acf_path: None,
        control_reload_acf_path: None,
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
    let gw = PvaGateway::start(&test_reactor(), cfg).expect("read-only gateway start");

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
    let gw = PvaGateway::start(&test_reactor(), cfg).expect("acl gateway start");

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
    let gw = PvaGateway::start(&test_reactor(), cfg).expect("audit gateway start");

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
    // A plain SharedPV::new() rejects writes ("PUT not supported by this
    // PV", PutPolicy::Reject — pvxs has no implicit-writable SharedPV);
    // the gateway PUT must land in the upstream for the passthrough to be
    // observable, so the simulated upstream IOC is a writable mailbox.
    let pv = SharedPV::build_mailbox();
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
            PvField::ScalarArray(vec![ScalarValue::String("init".into())]),
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
    let cache = ChannelCache::new(test_reactor(), upstream_client, Duration::from_secs(60));
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
                ScalarValue::String("hello world".into()),
                ScalarValue::String("foo bar".into()),
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
            Some(PvField::ScalarArrayTyped(TypedScalarArray::String(arr))) => {
                Some(arr.iter().map(|s| s.to_string()).collect())
            }
            Some(PvField::ScalarArray(vals)) => Some(
                vals.iter()
                    .filter_map(|v| {
                        if let ScalarValue::String(s) = v {
                            Some(s.to_string())
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

/// `is_writable` must proxy the UPSTREAM channel's connection state, not the
/// gateway's local monitor-cache presence. pva2pva
/// `GWChannel::getAccessRights` delegates to the connected upstream channel
/// (p2pApp/channel.cpp:92-96), so a connectable PV is "writable" even before
/// any prior op populated the cache — the pre-fix peek-only impl returned
/// `false` here. A PV the gateway cannot connect to is not writable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_is_writable_proxies_upstream_connection_not_cache_presence() {
    let (_us_server, us_addr, _us_pv) = spawn_upstream("GW:WR:PV", 1.0);
    let upstream_client = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    let cache = ChannelCache::new(test_reactor(), upstream_client, Duration::from_secs(60));
    let mut src = GatewayChannelSource::new(cache);
    src.connect_timeout = Duration::from_millis(400);

    // Connectable upstream PV → writable, WITHOUT any prior op having cached a
    // monitor. The pre-fix `self.cache.peek(name).is_some()` returned false on
    // this cold-cache probe; the connection-state proxy returns true.
    assert!(
        src.is_writable("GW:WR:PV").await,
        "is_writable must proxy the connectable upstream channel (cold cache)"
    );

    // A PV no upstream serves cannot be connected, so it is not writable — the
    // probe fails within `connect_timeout` rather than reporting a stale flag.
    assert!(
        !src.is_writable("GW:NOSUCH:PV").await,
        "is_writable must be false for a PV the gateway cannot connect to"
    );
}

/// typed-subscribe fallback delivers live updates.
///
/// `GatewayChannelSource::subscribe` routes through `subscribe_inner`
/// which bridges a `broadcast::Receiver<PvField>` (from `entry.subscribe()`)
/// to an mpsc channel. Pre-fix the typed broadcast sender `tx_inner` was
/// dropped before the `pvmonitor_raw_frames_handle` callback ran
/// (`let _ = tx_inner; // typed broadcast retired in raw path`), so
/// `bcast_rx.recv()` blocked forever — no decoded update ever reached the
/// subscriber. Downstream monitors using a pvRequest that forces the
/// decoded fallback (masked fields, pipelined, filtered, or
/// EPICS_PVA_GW_RAW_FRAMES=NO) received nothing.
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
    let cache = ChannelCache::new(test_reactor(), upstream_client, Duration::from_secs(60));
    let mut src = GatewayChannelSource::new(cache);
    src.connect_timeout = Duration::from_secs(2);

    // subscribe() routes through subscribe_inner → typed broadcast
    // fallback path. Under the single-seed contract this ctx-less stream
    // is UPDATES-ONLY: the connect-time snapshot is delivered via
    // subscribe_seeded's seed (emitted by the native server), never
    // replayed into the stream. This test exercises the legacy stream
    // directly, so 1.0 is NOT delivered here — the regression guarded is
    // that live UPDATES flow through the typed broadcast.
    let mut rx = src
        .subscribe("BR:R41:PV")
        .await
        .expect("subscribe must return Some for a known PV");

    // Post an update upstream; the monitor callback fires, apply_monitor_event
    // decodes it, and (after fix) tx_inner sends it to the typed broadcast.
    let update_val = PvField::Structure({
        let mut s = PvStructure::new("");
        s.set("value", PvField::Scalar(ScalarValue::Double(42.0)));
        s
    });
    pv.try_post(update_val);

    // Drain until 42.0 arrives or 2 s elapses. The updates-only stream
    // carries no connect-time 1.0, so 42.0 is the first (and only) item.
    // Pre-fix: tx_inner was dropped → broadcast permanently empty → this
    // loop times out.
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

/// Regression: a downstream monitor forced onto the DECODED path
/// (pipelined / field-masked / filtered) must receive the same
/// subscription-boundary (MONITOR FINISH) on an upstream descriptor change
/// as a raw-path-eligible monitor — it must NOT keep serving values under
/// its stale INIT descriptor.
///
/// Pre-fix the gateway carried the type-change / disconnect boundary only on
/// the RAW fanout (`RawEvent.type_changed`, and `signal_disconnect_boundary`
/// emitting solely to `tx_raw`). A downstream monitor that negotiated a
/// pipeline window (or a field projection / server-side filter) is served by
/// the gateway's DECODED path (`subscribe_seeded` → `MonitorUpdate` stream),
/// which carried bare values with no boundary marker — so the boundary was
/// invisible to it. Post-fix `broadcast_boundary` emits on BOTH streams and
/// the decoded server loop turns the `MonitorUpdate::type_change()` into
/// MONITOR FINISH before encoding any value under the stale descriptor.
///
/// Upstream parity: pva2pva stops a monitor on a new upstream type
/// (`moncache.cpp:56-83`) and surfaces a lost upstream as downstream
/// *unlisten* (`moncache.cpp:212-235`); pvxs treats reconnect / type-change
/// as a subscription boundary (`ioc/pvalink_channel.cpp:342-351 onTypeChange()`).
///
/// Topology: two downstream clients on the SAME gateway PV. The raw-vs-
/// decoded split is driven entirely by the pvRequest's pipeline option,
/// because every high-level client monitor builds `MonitorFlow` from it:
///   * `field(value)` — no pipeline option ⟹ `MonitorFlow.pipeline == false`
///     ⟹ the server negotiates no credit window ⟹ `window.is_none()` and
///     (full mask, no filter) ⟹ `raw_path_eligible` ⟹ gateway serves it via
///     the RAW fanout (`subscribe_raw_seeded`).
///   * `record[pipeline=true,queueSize=4]` — `pipeline == true` ⟹
///     `window.is_some()` defeats the raw-path gate ⟹ DECODED path
///     (`subscribe_seeded`, the `MonitorUpdate` stream this finding fixes).
/// After the upstream SharedPV is closed (descriptor change: old descriptor
/// revoked), BOTH `pvmonitor_events` tasks must observe
/// `MonitorEvent::Finished` and complete. Pre-fix the pipelined one never
/// gets a boundary on the decoded stream and hangs until this test's timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn br_99_decoded_monitor_gets_finish_on_upstream_descriptor_change() {
    use epics_pva_rs::client_native::ops_v2::{MonitorEvent, MonitorEventMask};
    use epics_pva_rs::pv_request::PvRequestExpr;

    let (_us_server, us_addr, us_pv) = spawn_upstream("GW:BR99:PV", 1.0);
    let upstream_client = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    let gw =
        PvaGateway::start(&test_reactor(), gateway_cfg(upstream_client)).expect("gateway start");

    let c_raw = gw.client_config();
    let c_pipe = gw.client_config();

    // Each monitor signals once it observes a subscription boundary
    // (`Finished`, or `Disconnected` as a fallback boundary shape).
    let (fin_raw_tx, mut fin_raw_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (fin_pipe_tx, mut fin_pipe_rx) = tokio::sync::mpsc::channel::<()>(1);

    // pvxs default mask: maskConnected=true, maskDisconnected=false — so
    // both Finished and Disconnected reach the callback.
    let mask = MonitorEventMask::default();

    // Monitor #1: RAW-path eligible — `field(value)` is the full mask for
    // this single-field PV and carries no pipeline option, so the server
    // negotiates no credit window and routes it onto the raw fanout.
    let raw_req = PvRequestExpr::parse("field(value)").expect("parse pvRequest");
    let h_raw = tokio::spawn(async move {
        let _ = c_raw
            .pvmonitor_events("GW:BR99:PV", Some(&raw_req), mask, move |ev| {
                if matches!(ev, MonitorEvent::Finished | MonitorEvent::Disconnected) {
                    let _ = fin_raw_tx.try_send(());
                }
            })
            .await;
    });

    // Monitor #2: pipelined → DECODED path. `window.is_some()` makes
    // `raw_path_eligible` false in the native server, so this monitor is
    // served from the gateway's decoded `MonitorUpdate` stream.
    let pipe_req =
        PvRequestExpr::parse("record[pipeline=true,queueSize=4]").expect("parse pvRequest");
    let h_pipe = tokio::spawn(async move {
        let _ = c_pipe
            .pvmonitor_events("GW:BR99:PV", Some(&pipe_req), mask, move |ev| {
                if matches!(ev, MonitorEvent::Finished | MonitorEvent::Disconnected) {
                    let _ = fin_pipe_tx.try_send(());
                }
            })
            .await;
    });

    // Let both subscriptions establish; the gateway's shared upstream
    // monitor delivers the seed (1.0) and caches `latest_raw`, which arms
    // `signal_disconnect_boundary` (it no-ops without a cached snapshot).
    tokio::time::sleep(Duration::from_millis(600)).await;
    us_pv.try_post(nt_double_value(2.0));
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Upstream descriptor change: close() revokes the descriptor and drops
    // all subscribers. The upstream server sends MONITOR FINISH to the
    // gateway's upstream client; the gateway's upstream monitor task ends and
    // `signal_disconnect_boundary` fires the boundary on BOTH fanout streams.
    us_pv.close();

    // BOTH monitors must observe the boundary within the timeout. Pre-fix
    // the pipelined (decoded) monitor never does → its recv times out.
    tokio::time::timeout(Duration::from_secs(5), fin_raw_rx.recv())
        .await
        .expect(
            "raw-path monitor must observe a boundary on upstream descriptor change (timed out)",
        )
        .expect("raw-path monitor boundary channel must not close empty");
    tokio::time::timeout(Duration::from_secs(5), fin_pipe_rx.recv())
        .await
        .expect("decoded/pipelined monitor must observe a boundary on upstream descriptor change (pre-fix this hangs)")
        .expect("decoded/pipelined monitor boundary channel must not close empty");

    h_raw.abort();
    h_pipe.abort();
}

/// §12.10: a PLAIN upstream loss — the IOC dies, nothing is closed
/// gracefully — must revoke the gateway's cached value with a
/// monitor-unlisten boundary.
///
/// This is the third site of the "disconnect inferred from the subscription
/// future returning" family (commit f75f1e56 closed the two pvalink sites).
/// The gateway's upstream task learned about the upstream ONLY from
/// `handle.wait()` returning — and the client's raw-frames handle
/// re-subscribes INTERNALLY on
/// `MonitorEnd::ConnectionLost` (announce, sleep 200 ms, loop), so on a plain
/// upstream loss it never returns. `signal_disconnect_boundary` therefore
/// never fired, and every downstream monitor kept being served the dead IOC's
/// last value at NoAlarm — the exact failure the boundary exists to prevent.
///
/// Distinguishing it from `br_99_...` above matters: THAT test kills the
/// upstream with `SharedPV::close()`, a clean MONITOR FINISH that DOES end
/// the handle, so it passes with or without the fix. Only a plain circuit
/// loss (`drop(server)`) separates the two designs.
///
/// Post-fix the transition comes from the monitor handle's
/// `MonitorConnEvent` stream, so the downstream monitor observes a
/// subscription boundary while the upstream subscription is still alive and
/// retrying. Upstream parity: pva2pva surfaces a lost upstream as downstream
/// *unlisten* (`moncache.cpp:212-235`) rather than fabricating an INVALID
/// alarm value.
///
/// Proven by mutation: with the `conn_cb` wiring in
/// `channel_cache::spawn_upstream_monitor` removed, this times out at 10 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gw_plain_upstream_loss_fires_the_disconnect_boundary() {
    use epics_pva_rs::client_native::ops_v2::{MonitorEvent, MonitorEventMask};
    use epics_pva_rs::pv_request::PvRequestExpr;

    let (us_server, us_addr, us_pv) = spawn_upstream("GW:LOSS:PV", 1.0);
    let upstream_client = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    let gw =
        PvaGateway::start(&test_reactor(), gateway_cfg(upstream_client)).expect("gateway start");
    let c = gw.client_config();

    let (boundary_tx, mut boundary_rx) = tokio::sync::mpsc::channel::<()>(1);
    // `field(value)` is the full mask for this single-field PV and carries no
    // pipeline option, so the downstream monitor is raw-path eligible and is
    // served straight off the gateway's raw fanout — the stream
    // `signal_disconnect_boundary` revokes.
    let req = PvRequestExpr::parse("field(value)").expect("parse pvRequest");
    let mon = tokio::spawn(async move {
        let _ = c
            .pvmonitor_events(
                "GW:LOSS:PV",
                Some(&req),
                MonitorEventMask::default(),
                move |ev| {
                    if matches!(ev, MonitorEvent::Finished | MonitorEvent::Disconnected) {
                        let _ = boundary_tx.try_send(());
                    }
                },
            )
            .await;
    });

    // Let the downstream monitor establish and the gateway's shared upstream
    // monitor deliver a value: `signal_disconnect_boundary` is idempotent per
    // outage by no-opping without a cached snapshot, so the cache must be
    // armed before the loss for the boundary to mean anything.
    tokio::time::sleep(Duration::from_millis(600)).await;
    us_pv.try_post(nt_double_value(2.0));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // PLAIN upstream loss: the server value is dropped, which aborts its
    // listeners and interrupts every connection. No MONITOR FINISH, no
    // descriptor revocation — just a dead circuit. The gateway's upstream
    // subscription takes `MonitorEnd::ConnectionLost` and RE-SUBSCRIBES; its
    // handle does not return.
    drop(us_server);

    tokio::time::timeout(Duration::from_secs(10), boundary_rx.recv())
        .await
        .expect(
            "a plain upstream loss must revoke the cached value with a \
             monitor-unlisten boundary; pre-fix the gateway inferred the \
             disconnect from `handle.wait()` returning, which a re-subscribing \
             monitor never does, so this times out",
        )
        .expect("boundary channel must not close empty");

    mon.abort();
}

/// GW-60 lock (system-level false-positive regression).
///
/// The gateway's per-PV upstream fan-out uses a bounded broadcast ring
/// (`channel_cache.rs` `BROADCAST_CAPACITY`) that, read in isolation, drops
/// the OLDEST frame when a downstream forwarder lags — which a parity review
/// graded a wrong-data divergence from pvxs's coalesce-to-latest server queue.
/// But every wire-facing gateway monitor flows through the `epics-pva-rs`
/// server monitor queue (`tcp.rs` `push_squash_monitor`), which is sized to the
/// client's `queueSize` and SQUASHES to the tail (newest) on overflow while
/// eagerly draining the gateway source. End-to-end the client therefore
/// converges to the latest value, matching pvxs.
///
/// This drives the FULL pipeline into the exact GW-60 condition: a slow,
/// pipelined downstream consumer against a fast upstream burst. The slow
/// consumer stops ACKing, the credit window closes, the server's `queueSize`
/// `pending` fills and squashes, and the gateway source's broadcast ring laps
/// behind it. The client must still converge to the FINAL posted value, and
/// coalescing must actually have happened (fewer deliveries than posts). If the
/// server squash regressed to drop-oldest, or a stale trailing frame leaked as
/// the final delivery, `seen.last()` would not be the final post and this
/// fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gw60_slow_pipelined_downstream_converges_to_latest_under_overflow() {
    use epics_pva_rs::client_native::ops_v2::{MonitorEvent, MonitorEventMask};
    use epics_pva_rs::pv_request::PvRequestExpr;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn extract_double(v: &PvField) -> Option<f64> {
        if let PvField::Structure(s) = v {
            for (name, f) in &s.fields {
                if name == "value"
                    && let PvField::Scalar(ScalarValue::Double(d)) = f
                {
                    return Some(*d);
                }
            }
        }
        None
    }

    let (_us, us_addr, us_pv) = spawn_upstream("GW:GW60:PV", 0.0);
    let upstream_client = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    let gw =
        PvaGateway::start(&test_reactor(), gateway_cfg(upstream_client)).expect("gateway start");
    let c = gw.client_config();

    let seen = Arc::new(std::sync::Mutex::new(Vec::<f64>::new()));
    let seen_cb = seen.clone();
    let slow = Arc::new(AtomicBool::new(true));
    let slow_cb = slow.clone();
    let mask = MonitorEventMask::default();
    // pipeline=true → DECODED path with a credit window; queueSize=4 is a small
    // squash bound. A slow consumer starves the ACKs, closing the window so the
    // server's `pending` overflows into its squash path and the gateway source
    // broadcast ring laps behind it — the GW-60 condition end-to-end.
    let req = PvRequestExpr::parse("record[pipeline=true,queueSize=4]").expect("parse pvRequest");
    let h = tokio::spawn(async move {
        let _ = c
            .pvmonitor_events("GW:GW60:PV", Some(&req), mask, move |ev| {
                if let MonitorEvent::Data { value, .. } = ev {
                    if let Some(d) = extract_double(&value) {
                        seen_cb.lock().unwrap().push(d);
                    }
                    // Slow consumer: delay each delivery so ACKs lag and the
                    // server monitor queue overflows into its squash path.
                    if slow_cb.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            })
            .await;
    });

    // Subscription establishes; the seed (0.0) is delivered.
    tokio::time::sleep(Duration::from_millis(700)).await;

    // Fast burst, far faster than the 50 ms/event slow consumer ⟹ overflow.
    const N: i64 = 80;
    for i in 1..=N {
        us_pv.try_post(nt_double_value(i as f64));
    }

    // Let the overflow build and the slow consumer chew the coalesced backlog,
    // then stop slowing so it drains the squash queue down to the latest.
    tokio::time::sleep(Duration::from_secs(3)).await;
    slow.store(false, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_secs(2)).await;

    let seen_final = seen.lock().unwrap().clone();
    h.abort();

    assert!(
        !seen_final.is_empty(),
        "the monitor must deliver at least the seed plus some updates"
    );
    // Convergence: the LAST value delivered is the final posted value. A
    // drop-oldest ring that leaked a stale trailing frame as the final delivery
    // would fail here.
    assert_eq!(
        seen_final.last().copied(),
        Some(N as f64),
        "slow pipelined downstream must converge to the latest posted value \
         (GW-60): saw {seen_final:?}"
    );
    // Coalescing actually happened — the server squashed under overflow rather
    // than delivering every post (seed 0.0 + coalesced updates ⟹ < N + 1).
    assert!(
        seen_final.len() < (N as usize),
        "server must coalesce under overflow, not deliver every post: {} deliveries for {N} posts",
        seen_final.len()
    );
}

/// R18-25: one slow pipelined downstream must not starve its co-subscribers.
///
/// Two downstream monitors share ONE upstream monitor (same PV, same
/// credential). One is pipelined and never keeps up; the other is a plain
/// monitor that consumes everything instantly. The healthy one must keep
/// receiving updates while the slow one is stuck.
///
/// Pre-fix the gateway declared `monitor_watermarks = Some((0, 0))`, so the
/// slow op's every DATA emission drained its window to LOW and cast a *pause*
/// vote on the shared upstream. The aggregate rule ("pause iff every VOTING op
/// wants pause") counted only ops that had themselves crossed LOW — and a
/// non-pipelined op never crosses anything, so it never votes and its Resume
/// can never come. The slow client's single vote paused the upstream and the
/// healthy client stopped receiving: it saw the seed and nothing after.
///
/// pva2pva never throttles upstream (`moncache.cpp:133-137` polls the upstream
/// dry, with a bare `//TODO: flow control` where a throttle would go); a slow
/// downstream is absorbed in its own `overflowElement` and counted in
/// `ndropped` (`:151-174`). The gateway now does the same and declares no
/// watermarks at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r18_25_slow_pipelined_downstream_does_not_starve_co_subscribers() {
    use epics_pva_rs::client_native::ops_v2::{MonitorEvent, MonitorEventMask};
    use epics_pva_rs::pv_request::PvRequestExpr;

    fn extract_double(v: &PvField) -> Option<f64> {
        if let PvField::Structure(s) = v {
            for (name, f) in &s.fields {
                if name == "value"
                    && let PvField::Scalar(ScalarValue::Double(d)) = f
                {
                    return Some(*d);
                }
            }
        }
        None
    }

    let (_us, us_addr, us_pv) = spawn_upstream("GW:STARVE:PV", 0.0);
    let upstream_client = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    let gw =
        PvaGateway::start(&test_reactor(), gateway_cfg(upstream_client)).expect("gateway start");

    // The slow one: pipelined with a tiny window, and it never returns from its
    // callback quickly enough to ACK. Its window closes and stays closed.
    let slow_client = gw.client_config();
    let slow_req = PvRequestExpr::parse("record[pipeline=true,queueSize=2]").expect("pvRequest");
    let h_slow = tokio::spawn(async move {
        let _ = slow_client
            .pvmonitor_events(
                "GW:STARVE:PV",
                Some(&slow_req),
                MonitorEventMask::default(),
                move |ev| {
                    // Wedged for longer than the whole test: the window closes
                    // on its first delivery and never reopens.
                    if let MonitorEvent::Data { .. } = ev {
                        std::thread::sleep(Duration::from_secs(5));
                    }
                },
            )
            .await;
    });

    // The healthy one: a plain monitor on the same PV, consuming instantly.
    let fast_client = gw.client_config();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<f64>::new()));
    let seen_cb = seen.clone();
    let h_fast = tokio::spawn(async move {
        let _ = fast_client
            .pvmonitor_events(
                "GW:STARVE:PV",
                None,
                MonitorEventMask::default(),
                move |ev| {
                    if let MonitorEvent::Data { value, .. } = ev
                        && let Some(d) = extract_double(&value)
                    {
                        seen_cb.lock().unwrap().push(d);
                    }
                },
            )
            .await;
    });

    // Both subscriptions establish and take their seed; the slow one is now
    // wedged inside its first callback with a closed window.
    tokio::time::sleep(Duration::from_millis(900)).await;
    let before = seen.lock().unwrap().len();

    // Upstream keeps posting. Every one of these must reach the healthy
    // downstream — the slow one's backpressure is its own problem.
    for i in 1..=20i64 {
        us_pv.try_post(nt_double_value(i as f64));
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    tokio::time::sleep(Duration::from_millis(700)).await;

    let seen_final = seen.lock().unwrap().clone();
    h_slow.abort();
    h_fast.abort();

    assert!(
        seen_final.len() > before,
        "the healthy co-subscriber received nothing after the slow pipelined \
         client stalled: the gateway paused the SHARED upstream on that \
         client's vote (saw {seen_final:?}, {before} of them before the posts)"
    );
    assert_eq!(
        seen_final.last().copied(),
        Some(20.0),
        "the healthy co-subscriber must track the upstream to its latest value, \
         saw {seen_final:?}"
    );
}

// ---------------------------------------------------------------------
// PUT_GET leg: the gateway must forward a
// downstream PUT_GET as ONE upstream PUT_GET — preserving the downstream
// pvRequest — and return the *upstream* post-put readback, not a local
// put plus a cached GET.
// ---------------------------------------------------------------------

/// Upstream test source whose `put_get_checked` override (1) records the
/// pvRequest it received — proving the gateway forwarded the downstream
/// request — and (2) stores TWICE the put double, returning the doubled
/// value as the readback. A downstream PUT_GET that comes back doubled
/// therefore proves the gateway ran a real upstream PUT_GET (post-put
/// readback), since the gateway never cached a doubled value of its own.
#[derive(Clone)]
struct RecordingDoublingSource {
    pv_name: String,
    value: Arc<std::sync::Mutex<f64>>,
    captured_req: Arc<std::sync::Mutex<Option<PvField>>>,
}

impl RecordingDoublingSource {
    fn new(pv_name: &str, initial: f64) -> Self {
        Self {
            pv_name: pv_name.to_string(),
            value: Arc::new(std::sync::Mutex::new(initial)),
            captured_req: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

// ── BRIDGE-31: ChannelArray (cmd 14) gateway forwarding ─────────────────
//
// A downstream ChannelArray op must reach the upstream IOC through the
// gateway, preserving the downstream INIT pvRequest (pva2pva
// `GWChannel::createChannelArray` forwards `pvRequest` unchanged). The
// upstream here is a source that serves a real windowed double array; the
// gateway forwards getLength/getArray/putArray/setLength to it.

/// Upstream source backing one PV with an in-memory `Vec<f64>`, serving the
/// full ChannelArray surface plus the existence/introspection a gateway
/// `pvconnect` / `pvinfo` resolves a channel through.
struct UpstreamArraySource {
    pv: String,
    data: std::sync::Mutex<Vec<f64>>,
}

impl UpstreamArraySource {
    fn new(pv: &str, initial: Vec<f64>) -> Self {
        Self {
            pv: pv.to_string(),
            data: std::sync::Mutex::new(initial),
        }
    }
}

fn doubles_field(values: &[f64]) -> PvField {
    PvField::ScalarArray(values.iter().copied().map(ScalarValue::Double).collect())
}

/// Normalise a getArray reply (the wire-decode path yields either the
/// generic `ScalarArray` or the packed `ScalarArrayTyped`) to `Vec<f64>`.
fn array_doubles(field: &PvField) -> Vec<f64> {
    let items = match field {
        PvField::ScalarArray(items) => items.clone(),
        PvField::ScalarArrayTyped(arr) => arr.to_scalar_values(),
        other => panic!("expected ScalarArray, got {other:?}"),
    };
    items
        .iter()
        .map(|v| match v {
            ScalarValue::Double(d) => *d,
            other => panic!("expected Double element, got {other:?}"),
        })
        .collect()
}

impl ChannelSource for UpstreamArraySource {
    async fn list_pvs(&self) -> Vec<String> {
        vec![self.pv.clone()]
    }
    fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
        let matches = n == self.pv;
        async move { matches }
    }
    async fn get_introspection(&self, _: &str) -> Option<FieldDesc> {
        Some(FieldDesc::Structure {
            struct_id: "epics:nt/NTScalarArray:1.0".into(),
            fields: vec![("value".into(), FieldDesc::ScalarArray(ScalarType::Double))],
        })
    }
    async fn get_value(&self, _: &str) -> Option<PvField> {
        Some(doubles_field(&self.data.lock().unwrap()))
    }
    async fn put_value(&self, _: &str, _: PvField) -> Result<(), OpError> {
        Ok(())
    }
    async fn is_writable(&self, _: &str) -> bool {
        true
    }
    async fn subscribe(
        &self,
        _: &str,
    ) -> Option<epics_pva_rs::server_native::MonitorStream<PvField>> {
        None
    }

    async fn channel_array_init(&self, _: &str, _: ChannelContext) -> Result<FieldDesc, OpError> {
        Ok(FieldDesc::ScalarArray(ScalarType::Double))
    }
    async fn channel_array_get(
        &self,
        checked: AccessChecked,
        offset: u32,
        count: u32,
        stride: u32,
        _: ChannelContext,
    ) -> Result<PvField, OpError> {
        if !checked.allows_read() {
            return Err(OpError::denied("read denied"));
        }
        let data = self.data.lock().unwrap();
        let stride = stride.max(1) as usize;
        let want = count as usize; // 0 => to the end
        let mut out = Vec::new();
        let mut i = offset as usize;
        while i < data.len() && (want == 0 || out.len() < want) {
            out.push(ScalarValue::Double(data[i]));
            i += stride;
        }
        Ok(PvField::ScalarArray(out))
    }
    async fn channel_array_put(
        &self,
        checked: AccessChecked,
        offset: u32,
        stride: u32,
        value: PvField,
        _: ChannelContext,
    ) -> Result<(), OpError> {
        if !checked.allows_write() {
            return Err(OpError::denied("write denied"));
        }
        let new = array_doubles(&value);
        let stride = stride.max(1) as usize;
        let mut data = self.data.lock().unwrap();
        let mut idx = offset as usize;
        for d in new {
            if idx >= data.len() {
                data.resize(idx + 1, 0.0);
            }
            data[idx] = d;
            idx += stride;
        }
        Ok(())
    }
    async fn channel_array_set_length(
        &self,
        checked: AccessChecked,
        length: u32,
        _: ChannelContext,
    ) -> Result<(), OpError> {
        if !checked.allows_write() {
            return Err(OpError::denied("write denied"));
        }
        self.data.lock().unwrap().resize(length as usize, 0.0);
        Ok(())
    }
    async fn channel_array_get_length(
        &self,
        checked: AccessChecked,
        _: ChannelContext,
    ) -> Result<u32, OpError> {
        if !checked.allows_read() {
            return Err(OpError::denied("read denied"));
        }
        Ok(self.data.lock().unwrap().len() as u32)
    }
}

/// Spawn an upstream PvaServer serving `source` on a random loopback port;
/// return (server, addr). Mirrors `spawn_upstream`'s port-pick / isolation.
fn spawn_upstream_source<S: ChannelSource + 'static>(source: S) -> (PvaServer, SocketAddr) {
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
    let server = PvaServer::start(Arc::new(source), cfg).expect("upstream array server must start");
    (server, bound)
}

/// The full ChannelArray surface round-trips through the gateway to the
/// upstream array source: getLength, getArray (full + sliced + strided),
/// putArray (mutates the upstream `Vec`), setLength.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge31_gateway_array_round_trip_forwards_upstream() {
    let (_us, us_addr) = spawn_upstream_source(UpstreamArraySource::new(
        "GW:ARR:PV",
        vec![10.0, 20.0, 30.0, 40.0, 50.0],
    ));
    let upstream = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    let gw = PvaGateway::start(&test_reactor(), gateway_cfg(upstream)).expect("gateway start");
    let ds = gw.client_config();

    // getLength
    let len = tokio::time::timeout(Duration::from_secs(5), ds.pvarray_get_length("GW:ARR:PV"))
        .await
        .expect("getLength through gateway must not hang")
        .expect("getLength must succeed");
    assert_eq!(len, 5, "initial upstream length");

    // getArray full (count == 0 → to end)
    let (_d, full) =
        tokio::time::timeout(Duration::from_secs(5), ds.pvarray_get("GW:ARR:PV", 0, 0, 1))
            .await
            .expect("getArray must not hang")
            .expect("getArray must succeed");
    assert_eq!(array_doubles(&full), vec![10.0, 20.0, 30.0, 40.0, 50.0]);

    // getArray slice: offset 1, count 2, stride 1
    let (_d, slice) = ds
        .pvarray_get("GW:ARR:PV", 1, 2, 1)
        .await
        .expect("sliced getArray must succeed");
    assert_eq!(array_doubles(&slice), vec![20.0, 30.0]);

    // getArray strided: offset 0, count 0, stride 2
    let (_d, strided) = ds
        .pvarray_get("GW:ARR:PV", 0, 0, 2)
        .await
        .expect("strided getArray must succeed");
    assert_eq!(array_doubles(&strided), vec![10.0, 30.0, 50.0]);

    // putArray: write [99, 98] at offset 1 → upstream Vec becomes
    // [10, 99, 98, 40, 50]
    ds.pvarray_put("GW:ARR:PV", &doubles_field(&[99.0, 98.0]), 1, 1)
        .await
        .expect("putArray through gateway must succeed");
    let (_d, after_put) = ds
        .pvarray_get("GW:ARR:PV", 0, 0, 1)
        .await
        .expect("getArray after put must succeed");
    assert_eq!(
        array_doubles(&after_put),
        vec![10.0, 99.0, 98.0, 40.0, 50.0],
        "putArray must splice at offset on the upstream"
    );

    // setLength: shrink to 3 → [10, 99, 98]
    ds.pvarray_set_length("GW:ARR:PV", 3)
        .await
        .expect("setLength through gateway must succeed");
    let len = ds
        .pvarray_get_length("GW:ARR:PV")
        .await
        .expect("getLength after resize must succeed");
    assert_eq!(len, 3, "length after setLength on the upstream");
}

/// A `read_only` gateway forwards read-class array sub-ops (getLength,
/// getArray) but rejects write-class sub-ops (putArray, setLength) at the
/// `ReadOnly` layer — the same write-class refusal the layer applies to
/// PUT/PUT_GET/PROCESS/RPC, extended to the new array op family.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge31_read_only_gateway_rejects_array_write() {
    let (_us, us_addr) =
        spawn_upstream_source(UpstreamArraySource::new("GW:ROARR:PV", vec![1.0, 2.0, 3.0]));
    let upstream = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    let mut cfg = gateway_cfg(upstream);
    cfg.read_only = true;
    let gw = PvaGateway::start(&test_reactor(), cfg).expect("read-only gateway start");
    let ds = gw.client_config();

    // Read-class sub-ops still pass through.
    let len = tokio::time::timeout(Duration::from_secs(5), ds.pvarray_get_length("GW:ROARR:PV"))
        .await
        .expect("getLength must not hang")
        .expect("read-only gateway must still forward getLength");
    assert_eq!(len, 3);
    let (_d, full) = ds
        .pvarray_get("GW:ROARR:PV", 0, 0, 1)
        .await
        .expect("read-only gateway must still forward getArray");
    assert_eq!(array_doubles(&full), vec![1.0, 2.0, 3.0]);

    // putArray is rejected at the ReadOnly layer.
    let err = ds
        .pvarray_put("GW:ROARR:PV", &doubles_field(&[9.0]), 0, 1)
        .await
        .expect_err("putArray through a read-only gateway must be rejected");
    assert!(
        err.to_string().to_lowercase().contains("read-only"),
        "putArray rejection must come from the ReadOnly layer: {err}"
    );

    // setLength is rejected at the ReadOnly layer.
    let err = ds
        .pvarray_set_length("GW:ROARR:PV", 1)
        .await
        .expect_err("setLength through a read-only gateway must be rejected");
    assert!(
        err.to_string().to_lowercase().contains("read-only"),
        "setLength rejection must come from the ReadOnly layer: {err}"
    );
}

/// With a `control_prefix` set the gateway wraps the layered proxy source
/// in a `CompositeSource` (alongside the `ControlSource` diagnostic PVs).
/// A regular (non-control) array PV must still round-trip through the
/// composite to the upstream — proving `CompositeSource` forwards the
/// `channel_array_*` family rather than masking it with the trait default.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge31_gateway_array_through_composite_control_prefix() {
    let (_us, us_addr) = spawn_upstream_source(UpstreamArraySource::new(
        "GW:CMPARR:PV",
        vec![3.0, 6.0, 9.0],
    ));
    let upstream = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    // control_prefix "gw" inserts the CompositeSource + ControlSource; the
    // "GW:CMPARR:PV" name is not a control/diag PV, so the composite routes
    // it to the layered gateway source → upstream.
    let cfg = PvaGatewayConfig {
        control_prefix: Some("gw".into()),
        ..gateway_cfg(upstream)
    };
    let gw = PvaGateway::start(&test_reactor(), cfg).expect("control-prefix gateway start");
    let ds = gw.client_config();

    let len = tokio::time::timeout(
        Duration::from_secs(5),
        ds.pvarray_get_length("GW:CMPARR:PV"),
    )
    .await
    .expect("getLength through the composite must not hang")
    .expect("getLength through the composite must succeed");
    assert_eq!(len, 3, "initial upstream length via composite");

    let (_d, full) = ds
        .pvarray_get("GW:CMPARR:PV", 0, 0, 1)
        .await
        .expect("getArray through the composite must succeed");
    assert_eq!(array_doubles(&full), vec![3.0, 6.0, 9.0]);

    ds.pvarray_put("GW:CMPARR:PV", &doubles_field(&[60.0]), 1, 1)
        .await
        .expect("putArray through the composite must succeed");
    let (_d, after) = ds
        .pvarray_get("GW:CMPARR:PV", 0, 0, 1)
        .await
        .expect("getArray after put through the composite must succeed");
    assert_eq!(array_doubles(&after), vec![3.0, 60.0, 9.0]);
}

/// Extract the `.value` double from an NTScalar structure (or a bare
/// scalar).
fn double_of(field: &PvField) -> Option<f64> {
    match field {
        PvField::Structure(s) => s.fields.iter().find_map(|(k, v)| {
            (k == "value").then_some(v).and_then(|v| match v {
                PvField::Scalar(ScalarValue::Double(d)) => Some(*d),
                _ => None,
            })
        }),
        PvField::Scalar(ScalarValue::Double(d)) => Some(*d),
        _ => None,
    }
}

/// True iff the pvRequest carries a top-level `record` member. The
/// gateway's default value-only request (`field(value)`) has none, so a
/// captured request with `record` proves the *downstream* request — built
/// here with a record option — travelled through the gateway verbatim.
fn has_record_member(req: &PvField) -> bool {
    matches!(req, PvField::Structure(s) if s.fields.iter().any(|(k, _)| k == "record"))
}

impl ChannelSource for RecordingDoublingSource {
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        let n = self.pv_name.clone();
        async move { vec![n] }
    }
    fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
        let want = self.pv_name.clone();
        let got = n.to_string();
        async move { got == want }
    }
    async fn get_introspection(&self, _: &str) -> Option<FieldDesc> {
        Some(nt_double_desc())
    }
    fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let v = *self.value.lock().unwrap();
        async move { Some(nt_double_value(v)) }
    }
    fn put_value(
        &self,
        _: &str,
        value: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let store = self.value.clone();
        async move {
            let incoming =
                double_of(&value).ok_or_else(|| OpError::failed("no double .value field"))?;
            *store.lock().unwrap() = incoming * 2.0;
            Ok(())
        }
    }
    async fn is_writable(&self, _: &str) -> bool {
        true
    }
    async fn subscribe(
        &self,
        _: &str,
    ) -> Option<epics_pva_rs::server_native::MonitorStream<PvField>> {
        None
    }
    fn put_get_checked(
        &self,
        checked: AccessChecked,
        _desc: std::sync::Arc<FieldDesc>,
        _changed: BitSet,
        delta: &PvField,
        ctx: ChannelContext,
    ) -> impl std::future::Future<Output = Result<Option<SourceRead>, OpError>> + Send {
        let store = self.value.clone();
        let captured = self.captured_req.clone();
        async move {
            // Record the forwarded pvRequest, then apply the doubling put
            // and return the post-put readback as ONE operation.
            *captured.lock().unwrap() = ctx.pv_request.clone();
            if !checked.allows_write() {
                return Err(OpError::denied("write denied"));
            }
            let incoming =
                double_of(delta).ok_or_else(|| OpError::failed("no double .value field"))?;
            let doubled = incoming * 2.0;
            *store.lock().unwrap() = doubled;
            Ok(Some(SourceRead::from(nt_double_value(doubled))))
        }
    }
}

/// Build a 1-PV upstream PvaServer backed by a [`RecordingDoublingSource`].
fn spawn_recording_upstream(
    pv_name: &str,
    initial: f64,
) -> (PvaServer, SocketAddr, RecordingDoublingSource) {
    let src = RecordingDoublingSource::new(pv_name, initial);
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
    let server = PvaServer::start(Arc::new(src.clone()), cfg).expect("test server must start");
    (server, bound, src)
}

/// End-to-end: a downstream PUT_GET carrying a typed value AND a custom
/// pvRequest (with a `record` option) is forwarded by the gateway as ONE
/// upstream PUT_GET. The doubling upstream returns the post-put value, so
/// a readback of 42 proves the gateway returned the *upstream* PUT_GET
/// readback (not a cached snapshot — the gateway never holds a doubled
/// value), and the captured upstream pvRequest carrying `record` proves
/// the downstream request travelled verbatim rather than being dropped
/// for the gateway's default `field(value)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_put_get_forwards_pvrequest_and_returns_upstream_readback() {
    let (_us_server, us_addr, us_src) = spawn_recording_upstream("GW:PUTGET:PV", 1.0);
    let upstream_client = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    let cfg = gateway_cfg(upstream_client);
    let gw = PvaGateway::start(&test_reactor(), cfg).expect("gateway start");

    let ds = gw.client_config();

    // Custom downstream pvRequest: field(value) plus a record option, so
    // it is structurally distinct from the gateway's default request.
    let req = epics_pva_rs::pv_request::PvRequestBuilder::new()
        .field("value")
        .record("block", true)
        .build()
        .to_pv_field();

    let (_intro, readback) = tokio::time::timeout(
        Duration::from_secs(5),
        ds.pvput_get_pv_field_with_request_value("GW:PUTGET:PV", &req, &nt_double_value(21.0)),
    )
    .await
    .expect("downstream PUT_GET timed out")
    .expect("downstream PUT_GET failed");

    assert_eq!(
        double_of(&readback),
        Some(42.0),
        "PUT_GET readback must be the upstream post-put (doubled) value"
    );
    assert_eq!(
        *us_src.value.lock().unwrap(),
        42.0,
        "upstream source must hold the doubled value (a real upstream PUT_GET ran)"
    );

    let captured = us_src
        .captured_req
        .lock()
        .unwrap()
        .clone()
        .expect("upstream PUT_GET must have received a forwarded pvRequest");
    assert!(
        has_record_member(&captured),
        "the downstream pvRequest (carrying a record option) must reach upstream \
         verbatim, not be replaced by the gateway's default field(value): {captured:?}"
    );
}

/// Same atomic PUT_GET forward, but with `control_prefix` set so the
/// downstream source is a `CompositeSource(ControlSource, layered gateway)`.
/// A data-PV PUT_GET must resolve to the gateway owner and reach its
/// `put_get_checked` as ONE operation — proving `CompositeSource` forwards
/// PUT_GET rather than decomposing it into put_delta + cached get.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_put_get_forwards_through_composite_with_control_prefix() {
    let (_us_server, us_addr, us_src) = spawn_recording_upstream("GW:PUTGET:CMP", 1.0);
    let upstream_client = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    // gateway_cfg picks fresh ports; override control_prefix so the
    // CompositeSource + ControlSource wrap the layered gateway source.
    let cfg = PvaGatewayConfig {
        control_prefix: Some("gw".into()),
        ..gateway_cfg(upstream_client)
    };
    let gw = PvaGateway::start(&test_reactor(), cfg).expect("gateway start");

    let ds = gw.client_config();
    let req = epics_pva_rs::pv_request::PvRequestBuilder::new()
        .field("value")
        .record("block", true)
        .build()
        .to_pv_field();

    let (_intro, readback) = tokio::time::timeout(
        Duration::from_secs(5),
        ds.pvput_get_pv_field_with_request_value("GW:PUTGET:CMP", &req, &nt_double_value(21.0)),
    )
    .await
    .expect("downstream PUT_GET timed out")
    .expect("downstream PUT_GET failed");

    assert_eq!(
        double_of(&readback),
        Some(42.0),
        "PUT_GET through CompositeSource must return the upstream post-put (doubled) value"
    );
    let captured = us_src
        .captured_req
        .lock()
        .unwrap()
        .clone()
        .expect("upstream PUT_GET must have received a forwarded pvRequest via the composite");
    assert!(
        has_record_member(&captured),
        "the downstream pvRequest must reach upstream verbatim through CompositeSource: {captured:?}"
    );
}

/// An upstream source that assigns only a SUBSET of its NT on a read —
/// the shape every real QSRV PV has (`IOCSource::get` fills part of a
/// `cloneEmpty()`, so `getProperties` never assigns
/// `control.minStep` & co.). It declares that subset with
/// [`SourceRead::marked`], which is what the upstream server frames.
struct PartialReadSource {
    pv_name: String,
}

/// `{double value, double spare}` — `spare` is the leaf the upstream
/// never assigns, so it must never reach a downstream client.
fn partial_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![
            ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
            ("spare".into(), FieldDesc::Scalar(ScalarType::Double)),
        ],
    }
}

fn partial_value() -> PvField {
    let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
    s.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Double(7.5))));
    s.fields
        .push(("spare".into(), PvField::Scalar(ScalarValue::Double(0.0))));
    PvField::Structure(s)
}

impl ChannelSource for PartialReadSource {
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        let n = self.pv_name.clone();
        async move { vec![n] }
    }
    fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
        let want = self.pv_name.clone();
        let got = n.to_string();
        async move { got == want }
    }
    async fn get_introspection(&self, _: &str) -> Option<FieldDesc> {
        Some(partial_desc())
    }
    async fn get_value(&self, _: &str) -> Option<PvField> {
        Some(partial_value())
    }
    async fn read_checked(
        &self,
        _checked: AccessChecked,
        _ctx: ChannelContext,
    ) -> Option<SourceRead> {
        // Only `value` is assigned; `spare` is not.
        Some(SourceRead::marked(
            partial_value(),
            vec!["value".to_string()],
        ))
    }
    async fn put_value(&self, _: &str, _: PvField) -> Result<(), OpError> {
        Err(OpError::denied("read-only fixture"))
    }
    async fn is_writable(&self, _: &str) -> bool {
        false
    }
    async fn subscribe(
        &self,
        _: &str,
    ) -> Option<epics_pva_rs::server_native::MonitorStream<PvField>> {
        None
    }
}

/// R16-33: a GET through the gateway must frame the leaves the UPSTREAM
/// marked, not a full mask.
///
/// The gateway decodes the upstream reply into a fully-populated `PvField`
/// — the decoder zero-fills every leaf the upstream did not send — so
/// re-framing "everything" downstream would ship a `spare` the upstream
/// never assigned (pva2pva forwards the upstream reply's own bitset,
/// `p2pApp/channel.cpp:109-115`). The downstream changed-bitset must carry
/// `value` and NOT `spare`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_get_frames_upstream_marks_not_a_full_mask() {
    let pv = "GW:PARTIAL:PV";
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
    let us_addr = SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        us_cfg.tcp_port,
    );
    let _us = PvaServer::start(
        Arc::new(PartialReadSource {
            pv_name: pv.to_string(),
        }),
        us_cfg,
    )
    .expect("upstream must start");

    let upstream_client = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    let gw = PvaGateway::start(
        &test_reactor(),
        PvaGatewayConfig {
            upstream_client: Some(upstream_client.clone()),
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
            control_acf_path: None,
            control_reload_acf_path: None,
        },
    )
    .expect("gateway start");

    // The upstream itself frames only `value` — the fact the gateway has to
    // preserve.
    let up = upstream_client
        .pvget_marked(pv)
        .await
        .expect("upstream get must succeed");
    assert_eq!(
        up.marked,
        Some(vec!["value".to_string()]),
        "upstream must frame only the leaf it assigned"
    );

    let down = gw
        .client_config()
        .pvget_marked(pv)
        .await
        .expect("downstream get must succeed");
    assert_eq!(
        down.marked,
        Some(vec!["value".to_string()]),
        "the gateway must forward the upstream's marks, not a full mask: {:?}",
        down.marked
    );
}

/// An upstream source whose MONITOR seed assigns only a SUBSET of its NT —
/// the shape a real QSRV monitor has (its first post marks the fields the
/// record actually filled). The `updates` sender is retained so the stream
/// stays open with no post-seed events: the gateway's cache then holds
/// exactly ONE upstream event, the seed with bits `[value]`.
struct MarkedMonitorSource {
    pv_name: String,
    // Keeps the update streams alive (never posts) so the upstream monitor
    // does not FINISH out from under the gateway.
    updates: std::sync::Mutex<
        Vec<tokio::sync::mpsc::Sender<epics_pva_rs::server_native::MonitorUpdate>>,
    >,
}

impl ChannelSource for MarkedMonitorSource {
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        let n = self.pv_name.clone();
        async move { vec![n] }
    }
    fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
        let want = self.pv_name.clone();
        let got = n.to_string();
        async move { got == want }
    }
    async fn get_introspection(&self, _: &str) -> Option<FieldDesc> {
        Some(partial_desc())
    }
    async fn get_value(&self, _: &str) -> Option<PvField> {
        Some(partial_value())
    }
    /// The monitor seed: `value` assigned, `spare` not.
    async fn read_checked(
        &self,
        _checked: AccessChecked,
        _ctx: ChannelContext,
    ) -> Option<SourceRead> {
        Some(SourceRead::marked(
            partial_value(),
            vec!["value".to_string()],
        ))
    }
    async fn subscribe_checked_opts_marked(
        &self,
        _checked: AccessChecked,
        _ctx: ChannelContext,
        _opts: epics_pva_rs::server_native::MonitorOptions,
    ) -> Option<
        epics_pva_rs::server_native::MonitorStream<epics_pva_rs::server_native::MonitorUpdate>,
    > {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        self.updates.lock().unwrap().push(tx);
        Some(epics_pva_rs::server_native::MonitorStream::Channel(rx))
    }
    async fn put_value(&self, _: &str, _: PvField) -> Result<(), OpError> {
        Err(OpError::denied("read-only fixture"))
    }
    async fn is_writable(&self, _: &str) -> bool {
        false
    }
    async fn subscribe(
        &self,
        _: &str,
    ) -> Option<epics_pva_rs::server_native::MonitorStream<PvField>> {
        None
    }
}

/// R18-28: the gateway's MONITOR seed declares the WHOLE STRUCTURE, however
/// the upstream marked the events that built the cached snapshot.
///
/// pva2pva — the C++ gateway this port's cache is modelled on — copies the
/// merged element into the starting MonitorUser's buffer and sets the root
/// bit: `elem->changedBitSet->set(0); // indicate all changed`
/// (`moncache.cpp:304-312`). Our encoder cannot emit bit 0, so the decodable
/// equivalent is the canonical full leaf bitset, i.e. `marked: None`.
///
/// Wire shape reproduced: the upstream monitor's seed frames bits `[value]`
/// only; the gateway's cache decodes and merges it, then seeds its downstream
/// monitor from that snapshot. R17-32 (`c05a56f6`) mistook pva2pva's UPDATE
/// rule (`:142`, copy the upstream changed bitset) for its SEED rule and made
/// the seed carry the upstream mark union — a seed that can omit `alarm` and
/// `timeStamp`. This test fails against that commit (`marked == Some(["value"])`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r18_28_gateway_monitor_seed_declares_whole_structure() {
    let pv = "GW:MONMARK:PV";
    let (_us, us_addr) = spawn_upstream_source(MarkedMonitorSource {
        pv_name: pv.to_string(),
        updates: std::sync::Mutex::new(Vec::new()),
    });

    let upstream_client = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    let cache = ChannelCache::new(test_reactor(), upstream_client, Duration::from_secs(60));
    let mut src = GatewayChannelSource::new(cache);
    src.connect_timeout = Duration::from_secs(5);

    let ctx = ChannelContext {
        peer: "127.0.0.1:1234".parse().unwrap(),
        creds: std::sync::Arc::new(epics_pva_rs::server_native::config::ClientCredentials {
            account: "tester".to_string(),
            method: "anonymous".to_string(),
            host: "localhost".to_string(),
            authority: String::new(),
            roles: Vec::new(),
        }),
        pv_request: None,
        log: Default::default(),
    };
    let checked = src
        .revalidate_read(pv, ctx.clone())
        .await
        .expect("read must be allowed by the default gate");

    let seed = src
        .subscribe_seeded(
            checked,
            ctx,
            epics_pva_rs::server_native::MonitorOptions::default(),
        )
        .await
        .expect("the gateway must seed a monitor for a connectable upstream PV");
    let initial = seed
        .initial
        .expect("the cached upstream seed must be forwarded as the downstream seed");

    assert!(
        initial.marked.is_none(),
        "the monitor seed must declare the whole structure (pva2pva's root \
         bit, moncache.cpp:304-312); declaring only the upstream's marks \
         ({:?}) lets a seed omit alarm/timeStamp",
        initial.marked
    );
}

/// An upstream IOC that refuses the write with a `Status` of its own —
/// `Fatal`, with a message AND a stack. Everything the downstream client is
/// owed is in that Status; the gateway's only job is not to touch it.
struct RefusingUpstream;

impl RefusingUpstream {
    const PV: &'static str = "GW:R18_27:PV";
    const MESSAGE: &'static str = "record is locked by another client";
    const STACK: &'static str = "iocsource.cpp:397";

    fn refusal() -> epics_pva_rs::proto::Status {
        epics_pva_rs::proto::Status::Detailed {
            kind: epics_pva_rs::proto::StatusKind::Fatal,
            message: Self::MESSAGE.into(),
            stack: Self::STACK.into(),
        }
    }
}

impl ChannelSource for RefusingUpstream {
    async fn list_pvs(&self) -> Vec<String> {
        vec![Self::PV.into()]
    }
    fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
        let matches = n == Self::PV;
        async move { matches }
    }
    async fn get_introspection(&self, _: &str) -> Option<FieldDesc> {
        Some(nt_double_desc())
    }
    async fn get_value(&self, _: &str) -> Option<PvField> {
        Some(nt_double_value(1.0))
    }
    async fn is_writable(&self, _: &str) -> bool {
        true
    }
    async fn put_value(&self, _: &str, _: PvField) -> Result<(), OpError> {
        Err(OpError::remote(Self::refusal()))
    }
    async fn subscribe(
        &self,
        _: &str,
    ) -> Option<epics_pva_rs::server_native::MonitorStream<PvField>> {
        None
    }
}

/// R18-27: the gateway forwards the upstream `Status` VERBATIM.
///
/// The downstream client asked the upstream's question, so it is owed the
/// upstream's answer: kind `Fatal`, the upstream message, the upstream stack.
/// pva2pva cannot re-author it (the downstream requester is handed straight to
/// the upstream channel — `p2pApp/channel.cpp:117-127`); the Rust gateway has
/// two legs and must forward it explicitly.
///
/// Pre-fix the upstream reply was rendered with `format!("PUT INIT failed:
/// {:?}", init.status)` (`ops_v2.rs:1289`) and re-authored as a local `Error`,
/// so the wire carried a Rust `Debug` dump, the `Fatal` kind was downgraded to
/// `Error`, and the stack was dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r18_27_gateway_forwards_the_upstream_status_verbatim() {
    let (_us, us_addr) = spawn_upstream_source(RefusingUpstream);
    let upstream_client = Arc::new(
        PvaClient::builder()
            .server_addr(us_addr)
            .timeout(Duration::from_secs(2))
            .build(),
    );
    let gw =
        PvaGateway::start(&test_reactor(), gateway_cfg(upstream_client)).expect("gateway start");
    let client = gw.client_config();

    let err = client
        .pvput_pv_field(RefusingUpstream::PV, &nt_double_value(7.0))
        .await
        .expect_err("the upstream refuses this write, so the gateway PUT must fail");

    let status = match err {
        epics_pva_rs::error::PvaError::RemoteError(s) => s,
        other => panic!(
            "a non-success reply Status must reach the caller as RemoteError(Status), got: {other:?}"
        ),
    };
    assert_eq!(
        status,
        RefusingUpstream::refusal(),
        "the gateway must put the upstream's own Status on the downstream wire \
         (kind/message/stack intact), not a rendering of it"
    );
}
