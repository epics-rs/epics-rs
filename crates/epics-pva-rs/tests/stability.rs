//! Stability / stress integration tests.
//!
//! Exercises the new client runtime against an in-process native PVA
//! server, covering the "P1-P9" stability requirements:
//!
//! - **P1 echo heartbeat** — verified by leaving a connection idle and
//!   confirming it stays alive (server's own heartbeat keeps it ticking).
//! - **P2 auto reconnect** — start server, GET, drop server, restart on
//!   same port, GET again on the same client → succeeds.
//! - **P5 monitor pipeline** — subscribe and confirm we receive >= N events
//!   for an N-event publish without missing any (default pipeline_size=4).
//! - **P6 idle/slot limits** — open up to `max_connections` clients, verify
//!   the next one is rejected.
//! - **P7 back-pressure** — flood a slow consumer with events and confirm
//!   we never crash (queue squashes).
//! - **P8 channel coalescing** — multiple concurrent pvget on the same PV
//!   share a single channel/connection.

#![allow(clippy::manual_async_fn)]

// RTEMS-EXEC-MODEL-ALLOW(26): checked - these run and pass in the feature-ON suite.
// (5 live client↔server monitor tests gated out feature-ON above; stage 3.)

use epics_pva_rs::server_native::MonitorStream;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, mpsc};

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServer, PvaServerConfig};
// Used only by the gated live client↔server monitor tests (stage 3).
#[cfg(not(feature = "rtems-exec-model"))]
use epics_pva_rs::server_native::{SharedPV, SharedSource};

// ── A tiny in-memory ChannelSource we can pump events into ───────────

#[derive(Clone)]
struct MemSource {
    inner: Arc<MemSourceInner>,
}

struct MemSourceInner {
    state: Mutex<MemState>,
    /// Subscribers per PV — every push fans out to all of them.
    subs: Mutex<std::collections::HashMap<String, Vec<mpsc::Sender<PvField>>>>,
    /// ordered log of every `notify_monitor_start(name, start)`
    /// the server fires, so a test can assert the Executing<->Idle edges.
    /// Sync mutex because `notify_monitor_start` is a sync trait method.
    monitor_starts: parking_lot::Mutex<Vec<(String, bool)>>,
}

struct MemState {
    values: std::collections::HashMap<String, PvField>,
}

/// Fan a value out to one PV's monitor subscribers, reaping only the dead.
///
/// A bounded `try_send` fails for two reasons and they are NOT the same:
/// `Closed` (the receiver is gone — reap it) and `Full` (the subscriber is
/// alive, it just has not drained yet). A source that treats `Full` as death
/// — `list.retain(|tx| tx.try_send(v).is_ok())` — silently unsubscribes a
/// live monitor the first time its channel backs up, and every later value
/// goes nowhere.
///
/// So this awaits room instead of dropping, which also keeps the source
/// LOSSLESS: the only place a value may be squashed is the server's
/// negotiated monitor queue. That cannot deadlock, because the server's
/// monitor pump drains this channel unconditionally — an exhausted credit
/// window gates the EMIT, never the DRAIN (`server_native/tcp.rs`: "`rx.recv()`
/// stays polled ... a stalled pipelined client coalesces at `limit`").
async fn fan_out(list: &mut Vec<mpsc::Sender<PvField>>, value: &PvField) {
    let mut live = Vec::with_capacity(list.len());
    for tx in list.drain(..) {
        // Err <=> receiver dropped. A full queue merely awaits room.
        if tx.send(value.clone()).await.is_ok() {
            live.push(tx);
        }
    }
    *list = live;
}

impl MemSource {
    fn new() -> Self {
        Self {
            inner: Arc::new(MemSourceInner {
                state: Mutex::new(MemState {
                    values: std::collections::HashMap::new(),
                }),
                subs: Mutex::new(std::collections::HashMap::new()),
                monitor_starts: parking_lot::Mutex::new(Vec::new()),
            }),
        }
    }

    /// snapshot the ordered start/stop edges recorded for a
    /// PV by [`ChannelSource::notify_monitor_start`].
    // Only consumers are the gated live-monitor tests, so it carries the same
    // predicate — otherwise dead code feature-ON (stage 3).
    #[cfg(not(feature = "rtems-exec-model"))]
    fn monitor_starts(&self, name: &str) -> Vec<bool> {
        self.inner
            .monitor_starts
            .lock()
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, s)| *s)
            .collect()
    }

    async fn add_pv(&self, name: &str, value: f64) {
        let pv = make_nt_scalar(value);
        self.inner
            .state
            .lock()
            .await
            .values
            .insert(name.to_string(), pv);
    }

    /// register an NTScalarArray PV holding a Double array.
    async fn add_array_pv(&self, name: &str, vals: &[f64]) {
        let pv = make_nt_double_array(vals);
        self.inner
            .state
            .lock()
            .await
            .values
            .insert(name.to_string(), pv);
    }

    /// Push a new Double-array value to an NTScalarArray PV.
    async fn push_array(&self, name: &str, vals: &[f64]) {
        let pv = make_nt_double_array(vals);
        self.inner
            .state
            .lock()
            .await
            .values
            .insert(name.to_string(), pv.clone());
        let mut subs = self.inner.subs.lock().await;
        if let Some(list) = subs.get_mut(name) {
            fan_out(list, &pv).await;
        }
    }

    async fn push(&self, name: &str, value: f64) {
        let pv = make_nt_scalar(value);
        self.inner
            .state
            .lock()
            .await
            .values
            .insert(name.to_string(), pv.clone());
        // Notify subscribers (reap only the dead — see `fan_out`).
        let mut subs = self.inner.subs.lock().await;
        if let Some(list) = subs.get_mut(name) {
            fan_out(list, &pv).await;
        }
    }
}

fn make_nt_scalar(v: f64) -> PvField {
    let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
    s.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Double(v))));
    PvField::Structure(s)
}

fn nt_scalar_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
    }
}

fn make_nt_double_array(vals: &[f64]) -> PvField {
    let mut s = PvStructure::new("epics:nt/NTScalarArray:1.0");
    s.fields.push((
        "value".into(),
        PvField::ScalarArray(vals.iter().map(|v| ScalarValue::Double(*v)).collect()),
    ));
    PvField::Structure(s)
}

fn nt_double_array_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalarArray:1.0".into(),
        fields: vec![("value".into(), FieldDesc::ScalarArray(ScalarType::Double))],
    }
}

impl ChannelSource for MemSource {
    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        let inner = self.inner.clone();
        async move {
            inner
                .state
                .lock()
                .await
                .values
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        }
    }
    fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let inner = self.inner.clone();
        let name = name.to_string();
        async move { inner.state.lock().await.values.contains_key(&name) }
    }
    fn get_introspection(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        let inner = self.inner.clone();
        let name = name.to_string();
        async move {
            // derive the descriptor from the stored value's
            // shape so an NTScalarArray PV reports an array descriptor.
            match inner.state.lock().await.values.get(&name) {
                Some(PvField::Structure(s))
                    if matches!(
                        s.fields.iter().find(|(k, _)| k == "value").map(|(_, v)| v),
                        Some(PvField::ScalarArray(_))
                    ) =>
                {
                    Some(nt_double_array_desc())
                }
                Some(_) => Some(nt_scalar_desc()),
                None => None,
            }
        }
    }
    fn get_value(&self, name: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let inner = self.inner.clone();
        let name = name.to_string();
        async move { inner.state.lock().await.values.get(&name).cloned() }
    }
    fn put_value(
        &self,
        name: &str,
        value: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let inner = self.inner.clone();
        let name = name.to_string();
        async move {
            inner
                .state
                .lock()
                .await
                .values
                .insert(name.clone(), value.clone());
            let mut subs = inner.subs.lock().await;
            if let Some(list) = subs.get_mut(&name) {
                fan_out(list, &value).await;
            }
            Ok(())
        }
    }
    fn is_writable(&self, _name: &str) -> impl std::future::Future<Output = bool> + Send {
        async { true }
    }
    fn subscribe(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        let inner = self.inner.clone();
        let name = name.to_string();
        async move {
            if !inner.state.lock().await.values.contains_key(&name) {
                return None;
            }
            let (tx, rx) = mpsc::channel::<PvField>(64);
            inner.subs.lock().await.entry(name).or_default().push(tx);
            Some(rx.into())
        }
    }
    fn notify_monitor_start(
        &self,
        name: &str,
        _ctx: &epics_pva_rs::server_native::source::ChannelContext,
        start: bool,
    ) {
        self.inner
            .monitor_starts
            .lock()
            .push((name.to_string(), start));
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

async fn spawn_server(source: Arc<MemSource>) -> (u16, u16, tokio::task::JoinHandle<()>) {
    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
        max_channels_per_connection: 64,
        monitor_queue_depth: 8,
        ..Default::default()
    };
    let server = PvaServer::start(source, cfg).expect("test server must start");
    let report = server.report();
    let (tcp, udp) = (report.tcp_port, report.udp_port);
    let h = tokio::spawn(async move {
        let _ = server.wait().await;
    });
    // Give the server a moment to bind.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (tcp, udp, h)
}

fn client_for(tcp_port: u16) -> PvaClient {
    let addr = std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        tcp_port,
    );
    PvaClient::builder()
        .timeout(Duration::from_secs(2))
        .server_addr(addr)
        .build()
}

/// a client that opts into a 1-byte inbound message-size cap.
/// Any real PVA frame (even the server's CONNECTION_VALIDATION) carries a
/// payload over 1 byte, so the reader drops the connection and every
/// operation fails — proving the opt-in cap is enforced.
fn capped_client_for(tcp_port: u16, cap: usize) -> PvaClient {
    let addr = std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        tcp_port,
    );
    PvaClient::builder()
        .timeout(Duration::from_secs(2))
        .server_addr(addr)
        .max_message_size(cap)
        .build()
}

/// spawn a server that opts into a tiny inbound cap. The
/// client's CONNECTION_VALIDATION reply exceeds `cap` bytes, so the
/// server drops the circuit during the handshake.
async fn spawn_server_capped(
    source: Arc<MemSource>,
    cap: usize,
) -> (u16, u16, tokio::task::JoinHandle<()>) {
    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
        max_channels_per_connection: 64,
        monitor_queue_depth: 8,
        max_message_size: Some(cap),
        ..Default::default()
    };
    let server = PvaServer::start(source, cfg).expect("test server must start");
    let report = server.report();
    let (tcp, udp) = (report.tcp_port, report.udp_port);
    let h = tokio::spawn(async move {
        let _ = server.wait().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (tcp, udp, h)
}

// ── Tests ────────────────────────────────────────────────────────────

#[tokio::test]
async fn p2_auto_reconnect_after_server_restart() {
    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:RECON", 1.0).await;

    // Started directly (not via `spawn_server`): this test needs the
    // first server's port genuinely freed before the second bind, and
    // `JoinHandle::abort()` on a task wrapping `wait()` does not give
    // that — it cancels the supervisor task without running `wait()`'s
    // own cross-abort logic, so the listener task is silently detached
    // and keeps holding the port. `stop()` + awaiting `wait()` to
    // completion is the API's actual shutdown contract.
    let cfg1 = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
        max_channels_per_connection: 64,
        monitor_queue_depth: 8,
        ..Default::default()
    };
    let server1 = PvaServer::start(source.clone(), cfg1).expect("test server must start");
    let tcp = server1.report().tcp_port;
    let client = client_for(tcp);

    // First GET succeeds.
    let v = tokio::time::timeout(Duration::from_secs(3), client.pvget("STAB:RECON"))
        .await
        .expect("pvget timed out")
        .expect("pvget failed");
    assert!(matches!(v, PvField::Structure(_)));

    // Restart server on same port.
    server1.stop();
    tokio::time::timeout(Duration::from_secs(2), server1.wait())
        .await
        .expect("server1.wait() timed out — stop did not complete")
        .expect("server1.wait() returned Err");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Reuse the same source — but we need to re-bind on the same port.
    // `PvaServer::start` requests the exact port `client` is already wired
    // to; if TIME_WAIT is still holding it, start() would silently fall
    // back to an ephemeral port (the defect this test suite closes), so
    // assert the readback instead of trusting the request.
    let source2 = source.clone();
    let cfg = PvaServerConfig {
        tcp_port: tcp,
        udp_port: tcp + 1,
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
        max_channels_per_connection: 64,
        monitor_queue_depth: 8,
        ..Default::default()
    };
    let server2 = PvaServer::start(source2, cfg).expect("server must restart on the freed port");
    assert_eq!(
        server2.report().tcp_port,
        tcp,
        "second server must rebind the exact same port the first one used"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    // GET on the same client should succeed (channel state machine
    // reconnects).
    let v = tokio::time::timeout(Duration::from_secs(5), client.pvget("STAB:RECON"))
        .await
        .expect("post-restart pvget timed out")
        .expect("post-restart pvget failed");
    assert!(matches!(v, PvField::Structure(_)));

    server2.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), server2.wait()).await;
}

/// the default client is **unbounded** (pvxs parity — no
/// RX message-size cap), while a client that opts into a tiny cap drops
/// the connection and fails every op. Same server, two clients: proves
/// both the new default and that the opt-in knob is still enforced.
#[tokio::test]
async fn sr9_client_default_unbounded_opt_in_cap_enforced() {
    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:SR9C", 3.5).await;

    let (tcp, _udp, h) = spawn_server(source.clone()).await;

    // Default client (cap None) — the GET succeeds.
    let ok = tokio::time::timeout(Duration::from_secs(3), client_for(tcp).pvget("STAB:SR9C"))
        .await
        .expect("default client pvget timed out");
    assert!(
        matches!(ok, Ok(PvField::Structure(_))),
        "default client must be unbounded and GET successfully, got {ok:?}"
    );

    // Opt-in 1-byte cap — the reader drops the connection, GET fails.
    let capped = tokio::time::timeout(
        Duration::from_secs(3),
        capped_client_for(tcp, 1).pvget("STAB:SR9C"),
    )
    .await
    .expect("capped client pvget hung past op timeout");
    assert!(
        capped.is_err(),
        "1-byte cap must reject the oversized inbound frame and fail the GET, got {capped:?}"
    );

    h.abort();
    let _ = h.await;
}

/// the server side of the opt-in cap. A server that opts into
/// a 1-byte cap drops the client during the handshake (its
/// CONNECTION_VALIDATION reply exceeds 1 byte), so the GET fails.
#[tokio::test]
async fn sr9_server_opt_in_cap_enforced() {
    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:SR9S", 7.0).await;

    let (tcp, _udp, h) = spawn_server_capped(source.clone(), 1).await;

    let res = tokio::time::timeout(Duration::from_secs(3), client_for(tcp).pvget("STAB:SR9S"))
        .await
        .expect("pvget against capped server hung past op timeout");
    assert!(
        res.is_err(),
        "server 1-byte cap must drop the oversized inbound handshake and fail the GET, got {res:?}"
    );

    h.abort();
    let _ = h.await;
}

#[tokio::test]
async fn p5_monitor_pipeline_does_not_drop() {
    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:MON", 0.0).await;

    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let client = client_for(tcp);

    let received = Arc::new(parking_lot::Mutex::new(Vec::<f64>::new()));
    let received_cb = received.clone();

    let monitor_handle = tokio::spawn({
        let client = client.clone();
        async move {
            let _ = client
                .pvmonitor("STAB:MON", move |value| {
                    if let PvField::Structure(s) = value
                        && let Some(ScalarValue::Double(v)) = s.get_value()
                    {
                        received_cb.lock().push(*v);
                    }
                })
                .await;
        }
    });

    // Allow subscription to settle (initial snapshot).
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Publish a known sequence.
    for i in 1..=10 {
        source.push("STAB:MON", i as f64).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    let got = received.lock().clone();
    // We expect at least one event including the initial snapshot. The
    // server may squash if back-pressure kicks in; verify that the *last*
    // value we observed reflects the latest publication.
    assert!(!got.is_empty(), "monitor received nothing");
    let last = *got.last().unwrap();
    assert!(
        (1.0..=10.0).contains(&last),
        "monitor delivered out-of-range value {last}"
    );

    monitor_handle.abort();
    h.abort();
}

/// Poll the recorded on_start edges for `name` until at least `want_len`
/// have landed (bounded ~2 s), then return them. Avoids fixed-sleep
/// flakiness for the "at least N edges" assertions.
#[cfg(not(feature = "rtems-exec-model"))]
async fn wait_starts(source: &MemSource, name: &str, want_len: usize) -> Vec<bool> {
    for _ in 0..100 {
        let s = source.monitor_starts(name);
        if s.len() >= want_len {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    source.monitor_starts(name)
}

/// a server-side monitor exposes pvxs `onStart(bool)` to the
/// source. MONITOR START fires `on_start(true)`; MONITOR PAUSE fires
/// `on_start(false)` without tearing the op down; MONITOR RESUME fires
/// `on_start(true)` once; DESTROY (handle drop) fires the terminal
/// `on_start(false)`. Mirrors pvxs `servermon.cpp:677-683` onStart edges.
// Live client ↔ server monitor over `tokio::net`; the client's connection
// tasks route through the reactor-less callback pool under `rtems-exec-model`.
// Reactor-dependent — gated out feature-ON (doc/pvalink-rtems-design.md §4.2).
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pva_fr_11_on_start_fires_across_start_pause_resume_destroy() {
    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:ONSTART", 0.0).await;
    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let client = client_for(tcp);

    let handle = client
        .pvmonitor_handle("STAB:ONSTART", move |_desc, _value| {}, |_| {})
        .await
        .expect("subscribe");

    // START → on_start(true).
    assert_eq!(
        wait_starts(&source, "STAB:ONSTART", 1).await,
        vec![true],
        "MONITOR START must fire on_start(true) exactly once"
    );

    // PAUSE → on_start(false); the subscription stays alive.
    handle.pause().await;
    assert_eq!(
        wait_starts(&source, "STAB:ONSTART", 2).await,
        vec![true, false],
        "MONITOR PAUSE must fire on_start(false) without destroying the op"
    );

    // RESUME → on_start(true) once.
    handle.resume().await;
    assert_eq!(
        wait_starts(&source, "STAB:ONSTART", 3).await,
        vec![true, false, true],
        "MONITOR RESUME must fire on_start(true) exactly once"
    );

    // DESTROY (handle drop sends CMD_DESTROY_REQUEST) → terminal
    // on_start(false) via MonitorStartControl::drop.
    drop(handle);
    assert_eq!(
        wait_starts(&source, "STAB:ONSTART", 4).await,
        vec![true, false, true, false],
        "DESTROY must fire the terminal on_start(false) once"
    );

    h.abort();
}

/// DESTROY after a prior PAUSE must NOT double-fire
/// `on_start(false)`. The op is already Idle, so
/// `MonitorStartControl::drop` sees `executing == false` and stays
/// silent — closing the dual-fire edge the single-owner design exists
/// to prevent.
// Live client ↔ server monitor over `tokio::net`; the client's connection
// tasks route through the reactor-less callback pool under `rtems-exec-model`.
// Reactor-dependent — gated out feature-ON (doc/pvalink-rtems-design.md §4.2).
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pva_fr_11_destroy_after_pause_does_not_double_fire() {
    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:ONSTART2", 0.0).await;
    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let client = client_for(tcp);

    let handle = client
        .pvmonitor_handle("STAB:ONSTART2", move |_desc, _value| {}, |_| {})
        .await
        .expect("subscribe");
    assert_eq!(wait_starts(&source, "STAB:ONSTART2", 1).await, vec![true]);

    handle.pause().await;
    assert_eq!(
        wait_starts(&source, "STAB:ONSTART2", 2).await,
        vec![true, false]
    );

    // DESTROY while already paused (Idle). Give the server ample time to
    // process the DESTROY, then assert NO second `false` was appended.
    drop(handle);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        source.monitor_starts("STAB:ONSTART2"),
        vec![true, false],
        "DESTROY after PAUSE must not double-fire on_start(false)"
    );

    h.abort();
}

/// pausing a server monitor must HOLD the latest value posted
/// while paused (squash) and deliver it on resume — not drop it (the
/// pre-fix floor-drop). Mirrors pvxs queue-while-Idle +
/// drain-on-START.
// Live client ↔ server monitor over `tokio::net`; the client's connection
// tasks route through the reactor-less callback pool under `rtems-exec-model`.
// Reactor-dependent — gated out feature-ON (doc/pvalink-rtems-design.md §4.2).
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pva_fr_8_pause_holds_latest_then_resume_delivers() {
    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:PAUSE", 0.0).await;
    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let client = client_for(tcp);

    let received = Arc::new(parking_lot::Mutex::new(Vec::<f64>::new()));
    let cb = received.clone();
    let handle = client
        .pvmonitor_handle(
            "STAB:PAUSE",
            move |_desc, value| {
                if let PvField::Structure(s) = value
                    && let Some(ScalarValue::Double(v)) = s.get_value()
                {
                    cb.lock().push(*v);
                }
            },
            |_| {},
        )
        .await
        .expect("subscribe");

    // Initial snapshot settles.
    tokio::time::sleep(Duration::from_millis(250)).await;

    handle.pause().await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let before = received.lock().len();

    // Two values posted while paused — the server must hold them,
    // squashing to the latest (22.0), delivering nothing yet.
    source.push("STAB:PAUSE", 11.0).await;
    source.push("STAB:PAUSE", 22.0).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        received.lock().len(),
        before,
        "paused monitor must not deliver events"
    );

    // Resume → the held latest value is delivered.
    handle.resume().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let got = received.lock().clone();
    assert!(
        got.len() > before,
        "resume must deliver the value held during the pause"
    );
    assert_eq!(
        *got.last().unwrap(),
        22.0,
        "resume delivers the latest value squashed during the pause"
    );

    handle.stop();
    h.abort();
}

/// the client report lists each live server connection with
/// its byte counters, and `report_zeroed(true)` resets them so the next
/// report is a delta (pvxs `Context::report(bool zero)`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pva_fr_2_client_report_has_connection_byte_counters() {
    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:REP", 1.5).await;
    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let client = client_for(tcp);

    // A GET establishes a connection and moves bytes both ways.
    let _ = tokio::time::timeout(Duration::from_secs(3), client.pvget("STAB:REP"))
        .await
        .expect("get did not time out");

    let r = client.report();
    assert_eq!(r.connections.len(), 1, "one server connection after a GET");
    let c = &r.connections[0];
    assert!(
        c.bytes_rx > 0 && c.bytes_tx > 0,
        "bytes flowed on the connection (rx={}, tx={})",
        c.bytes_rx,
        c.bytes_tx
    );

    // report(true) zeros the counters; the next report shows a delta
    // strictly smaller than the GET traffic just cleared.
    let _ = client.report_zeroed(true);
    let after = client.report();
    assert!(
        after.connections[0].bytes_rx < c.bytes_rx,
        "report_zeroed(true) must reset the rx counter (was {}, now {})",
        c.bytes_rx,
        after.connections[0].bytes_rx
    );

    h.abort();
}

/// Two PVs monitored through ONE shared client share a single server
/// connection — the property the `pvmonitor-rs` command relies on after
/// it was changed to build one client for the whole command instead of
/// one `PvaClient` per PV. pvxs starts every subscription from a single
/// `client::Context` whose `connByAddr` reuses one connection per server
/// (clientconn.cpp:44-56). Before the CLI fix, two PVs on the same IOC
/// opened two clients and could open two connections.
// Live client ↔ server monitor over `tokio::net`; the client's connection
// tasks route through the reactor-less callback pool under `rtems-exec-model`.
// Reactor-dependent — gated out feature-ON (doc/pvalink-rtems-design.md §4.2).
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pva_fr_166_shared_client_one_connection_for_two_monitors() {
    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:MON166A", 1.0).await;
    source.add_pv("STAB:MON166B", 2.0).await;
    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let client = client_for(tcp);

    // Both monitors come from the SAME client (as the CLI now clones one
    // shared client into every task).
    let a = client
        .pvmonitor_handle("STAB:MON166A", |_d, _v| {}, |_| {})
        .await
        .expect("subscribe A");
    let b = client
        .pvmonitor_handle("STAB:MON166B", |_d, _v| {}, |_| {})
        .await
        .expect("subscribe B");

    // Let both subscriptions reach Active on the shared connection.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let r = client.report();
    assert_eq!(
        r.connections.len(),
        1,
        "two PVs on one server via one client must share one connection, got {}",
        r.connections.len()
    );
    assert_eq!(
        r.connections[0].channels.len(),
        2,
        "both monitored channels ride the single shared connection"
    );

    a.stop();
    b.stop();
    h.abort();
}

/// A `pvmonitor_events` subscription opened with both masks cleared
/// surfaces the connect lifecycle the `pvmonitor-rs` CLI now prints: a
/// `Connected` event carrying the server peer, followed by the initial
/// `Data` snapshot. pvxs monitors with maskConnected=false,
/// maskDisconnected=false and reports `Connected to <peer>`
/// (tools/monitor.cpp:111-152). Before the CLI fix the value-only
/// callback could not observe any lifecycle event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pva_fr_25_monitor_events_surface_connected_with_peer_then_data() {
    use epics_pva_rs::client_native::ops_v2::{MonitorEvent, MonitorEventMask};

    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:EVT25", 7.0).await;
    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let client = client_for(tcp);
    let expected_peer =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), tcp);

    let events = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let ev = events.clone();
    let client_task = client.clone();
    let mon = tokio::spawn(async move {
        let mask = MonitorEventMask {
            mask_connected: false,
            mask_disconnected: false,
        };
        let _ = client_task
            .pvmonitor_events("STAB:EVT25", None, mask, move |event| match event {
                MonitorEvent::Connected { peer } => ev.lock().push(format!("connected:{peer}")),
                MonitorEvent::Data { .. } => ev.lock().push("data".to_string()),
                MonitorEvent::Disconnected => ev.lock().push("disconnected".to_string()),
                MonitorEvent::Finished => ev.lock().push("finished".to_string()),
            })
            .await;
    });

    tokio::time::sleep(Duration::from_millis(400)).await;
    let got = events.lock().clone();
    assert_eq!(
        got.first().map(String::as_str),
        Some(format!("connected:{expected_peer}").as_str()),
        "first event must be Connected carrying the server peer, got {got:?}"
    );
    assert!(
        got.iter().any(|s| s == "data"),
        "an initial Data event must follow Connected, got {got:?}"
    );

    mon.abort();
    h.abort();
}

/// Regression: commit c3f286c added a server-side pipeline
/// credit window unconditionally for every Monitor op, but pvxs only
/// applies flow control when the client's pvRequest explicitly
/// negotiates `record[pipeline=true]`. Default `pvmonitor` callers
/// don't, so the always-on 4-credit window stalled after ~5 frames
/// (initial snapshot + 4) waiting for an ACK refill that was
/// happening at the wire level but not closing the cycle in time —
/// regressing pre-v0.10.5 behaviour where the producer ran freely.
/// The fix: gate the window on the actual pipeline=true option.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p5_monitor_default_pvrequest_streams_freely() {
    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:MON:LONG", 0.0).await;

    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let client = client_for(tcp);

    let last_seen = Arc::new(parking_lot::Mutex::new(None::<f64>));
    let last_seen_cb = last_seen.clone();

    let monitor_handle = tokio::spawn({
        let client = client.clone();
        async move {
            let _ = client
                .pvmonitor("STAB:MON:LONG", move |value| {
                    if let PvField::Structure(s) = value
                        && let Some(ScalarValue::Double(v)) = s.get_value()
                    {
                        *last_seen_cb.lock() = Some(*v);
                    }
                })
                .await;
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // 50 events at 20 ms = 1 s of traffic. With default pipeline_size=4
    // this is ~12 ACK refill cycles. Pre-fix: stalls after ~5 events.
    const N: i32 = 50;
    for i in 1..=N {
        source.push("STAB:MON:LONG", i as f64).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let last = last_seen.lock();
    assert_eq!(
        *last,
        Some(N as f64),
        "monitor stalled mid-stream — last seen value did not reach final publish"
    );

    monitor_handle.abort();
    h.abort();
}

/// Companion to the regression above: when the client *does* opt in
/// via `record[pipeline=true,queueSize=N]`, the server-side window
/// must still gate emission and the ACK refill loop must keep
/// running. Verifies the pipeline path is preserved, not removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p5_monitor_explicit_pipeline_window_works() {
    use epics_pva_rs::pv_request::PvRequestExpr;

    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:MON:PIPE", 0.0).await;

    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let client = client_for(tcp);

    let last_seen = Arc::new(parking_lot::Mutex::new(None::<f64>));
    let last_seen_cb = last_seen.clone();

    let monitor_handle = tokio::spawn({
        let client = client.clone();
        async move {
            let req =
                PvRequestExpr::parse("record[pipeline=true,queueSize=4]").expect("parse pvRequest");
            let _ = client
                .pvmonitor_with_request("STAB:MON:PIPE", &req, move |value| {
                    if let PvField::Structure(s) = value
                        && let Some(ScalarValue::Double(v)) = s.get_value()
                    {
                        *last_seen_cb.lock() = Some(*v);
                    }
                })
                .await;
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    const N: i32 = 30;
    for i in 1..=N {
        source.push("STAB:MON:PIPE", i as f64).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let last = last_seen.lock();
    assert_eq!(
        *last,
        Some(N as f64),
        "explicit-pipeline monitor stalled — ACK/window cycle broken"
    );

    monitor_handle.abort();
    h.abort();
}

/// pvxs `servermon.cpp:537-540` parity: a pipelined monitor whose
/// PRESENT `queueSize` is invalid (`< 2`) must be REJECTED at INIT
/// (`ctrl->error(...)` + `return`), not silently downgraded to a
/// non-pipeline monitor. The high-level monitor op surfaces the INIT
/// error as a fatal `Err`, so the call returns rather than streaming.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipeline_invalid_queue_size_rejects_monitor_init() {
    use epics_pva_rs::pv_request::PvRequestExpr;

    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:MON:BADQ", 0.0).await;

    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let client = client_for(tcp);

    let req = PvRequestExpr::parse("record[pipeline=true,queueSize=1]").expect("parse pvRequest");
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        client.pvmonitor_with_request("STAB:MON:BADQ", &req, |_| {}),
    )
    .await
    .expect("MONITOR INIT must resolve (reject), not hang");

    assert!(
        result.is_err(),
        "pipeline + queueSize<2 must fail the MONITOR INIT, got {result:?}"
    );

    h.abort();
}

#[tokio::test]
async fn p8_channel_coalesces_concurrent_pvget() {
    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:COAL", 7.0).await;

    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let client = client_for(tcp);

    // Fire 10 concurrent pvget on the same PV. They should all succeed
    // quickly, sharing a single underlying ServerConn.
    let mut handles = Vec::new();
    for _ in 0..10 {
        let client = client.clone();
        handles.push(tokio::spawn(async move { client.pvget("STAB:COAL").await }));
    }
    for h in handles {
        let v = tokio::time::timeout(Duration::from_secs(3), h)
            .await
            .expect("pvget timed out")
            .expect("task join")
            .expect("pvget");
        assert!(matches!(v, PvField::Structure(_)));
    }

    h.abort();
}

/// PR #205 server-side filter PVA wire-through: a client that sets
/// `record._options._filter` in its pvRequest must see the chain
/// applied on the server side. Decimate by 3 — only every third
/// pushed value passes through to the callback. Without the wire-
/// through every push arrived; with it, ~1 in 3 do.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_side_filter_pva_dec_wire_through() {
    use epics_pva_rs::pv_request::PvRequestBuilder;

    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:FILT:DEC", 0.0).await;

    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let client = client_for(tcp);

    let seen = Arc::new(parking_lot::Mutex::new(Vec::<f64>::new()));
    let seen_cb = seen.clone();

    let req = PvRequestBuilder::new()
        .record("_filter", r#"{"dec":{"n":3}}"#)
        .build();

    let monitor_handle = tokio::spawn({
        let client = client.clone();
        async move {
            let _ = client
                .pvmonitor_with_request("STAB:FILT:DEC", &req, move |value| {
                    if let PvField::Structure(s) = value
                        && let Some(ScalarValue::Double(v)) = s.get_value()
                    {
                        seen_cb.lock().push(*v);
                    }
                })
                .await;
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Push 9 values. With dec n=3, the filter passes the 1st, 4th,
    // 7th of each window after the initial snapshot.
    const N: i32 = 9;
    for i in 1..=N {
        source.push("STAB:FILT:DEC", i as f64).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    let observed = seen.lock().clone();
    // Initial snapshot + decimated pushes. The initial snapshot has
    // value 0 from add_pv; the post-init pushes get filtered. We
    // don't pin an exact count (the filter counter includes the
    // initial frame), just that the wire-through actively dropped
    // events — without it we'd see all 10 (initial + 9 pushes).
    assert!(
        observed.len() < (N as usize) + 1,
        "filter wire-through did not drop any events; observed all {} of {} \
         pushes plus the initial snapshot",
        observed.len(),
        N + 1
    );
    // And at least one event made it through (proving the filter
    // isn't dropping everything).
    assert!(
        !observed.is_empty(),
        "filter dropped every event — chain misconfigured"
    );

    monitor_handle.abort();
    h.abort();
}

/// Regression: a server-side TRANSFORMATION filter (`arr`)
/// must actually change the emitted monitor value. Before the fix
/// the PVA monitor emit loop called `FilterChain::apply` only to
/// check `is_none()` for pass/drop and then built the wire payload
/// from the ORIGINAL value, so a client requesting an array slice
/// received the full unsliced array.
///
/// Here the `arr` filter selects every other element
/// (`s=0,i=2,e=-1`) of an 8-element array. After the fix the client
/// must receive the 4-element sliced array, not the original 8.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ex_r12_server_side_arr_filter_slices_monitor_value() {
    use epics_pva_rs::pv_request::PvRequestBuilder;

    let source = Arc::new(MemSource::new());
    let full: Vec<f64> = (0..8).map(|i| i as f64).collect();
    source.add_array_pv("STAB:EXR12:ARR", &full).await;

    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let client = client_for(tcp);

    let seen = Arc::new(parking_lot::Mutex::new(Vec::<Vec<f64>>::new()));
    let seen_cb = seen.clone();

    // `arr` slice: start 0, increment 2, end -1 → indices 0,2,4,6.
    let req = PvRequestBuilder::new()
        .record("_filter", r#"{"arr":{"s":0,"i":2,"e":-1}}"#)
        .build();

    let monitor_handle = tokio::spawn({
        let client = client.clone();
        async move {
            let _ = client
                .pvmonitor_with_request("STAB:EXR12:ARR", &req, move |value| {
                    // The wire decoder delivers arrays as the typed
                    // refcount-shared variant.
                    if let PvField::Structure(s) = value
                        && let Some(arr_field) = s
                            .fields
                            .iter()
                            .find_map(|(k, v)| (k == "value").then_some(v))
                    {
                        let arr: Vec<f64> = match arr_field {
                            PvField::ScalarArrayTyped(t) => t
                                .to_scalar_values()
                                .iter()
                                .map(|sv| match sv {
                                    ScalarValue::Double(d) => *d,
                                    _ => f64::NAN,
                                })
                                .collect(),
                            PvField::ScalarArray(items) => items
                                .iter()
                                .map(|sv| match sv {
                                    ScalarValue::Double(d) => *d,
                                    _ => f64::NAN,
                                })
                                .collect(),
                            _ => return,
                        };
                        seen_cb.lock().push(arr);
                    }
                })
                .await;
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;
    // Push a fresh 8-element array — this event flows through the
    // monitor emit loop where the filter chain runs.
    source
        .push_array(
            "STAB:EXR12:ARR",
            &[10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0, 17.0],
        )
        .await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    let observed = seen.lock().clone();
    assert!(
        observed.len() >= 2,
        "expected the initial snapshot plus the pushed event, got {}",
        observed.len()
    );
    // Finding #2: the INITIAL snapshot must be sliced too. Pre-fix it went
    // straight to `build_monitor_payload`, bypassing the chain, so the
    // first frame carried the full unsliced array. The arr slice of the
    // seeded `[0..7]` is indices 0,2,4,6 = `[0,2,4,6]`.
    let initial = observed.first().unwrap();
    assert_eq!(
        initial,
        &vec![0.0, 2.0, 4.0, 6.0],
        "finding #2: initial monitor frame must be arr-sliced like updates, not the full \
         array — got {initial:?}"
    );
    // The steady-state pushed event flows through the emit loop's
    // filter chain and MUST be the sliced array (indices 0,2,4,6 of
    // [10..17] = [10,12,14,16]). Before the fix the wire payload was
    // built from the original unsliced value, so this frame carried
    // all 8 elements.
    let pushed = observed.last().unwrap();
    assert_eq!(
        pushed,
        &vec![10.0, 12.0, 14.0, 16.0],
        "arr filter did not slice the emitted monitor value — got {pushed:?}"
    );
    assert_eq!(
        pushed.len(),
        4,
        "arr-sliced monitor value must have 4 elements"
    );

    monitor_handle.abort();
    h.abort();
}

/// Regression: pipeline credit must be consumed only for
/// monitor DATA frames actually sent to the client. A pipelined
/// monitor (`pipeline=true,queueSize=N`) combined with a server-side
/// `dec` filter that drops most events must still stream the events
/// that pass the filter.
///
/// Before the fix the emit loop decremented the pipeline window
/// BEFORE the pause/filter gates, so every filter-dropped event
/// consumed a window slot without sending a frame. With queueSize=4
/// and `dec n=3`, the first 4 events are all dropped by the filter;
/// they exhausted the window with no DATA frame, hence no client ACK
/// to refill it — the stream stalled and the events that should have
/// passed the filter never arrived.
///
/// After the fix the window decrements only for events that produce
/// a frame, so the passing events stream through and `last_seen`
/// reaches the final pushed value.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ex_r1_pipeline_credit_not_consumed_by_filtered_events() {
    use epics_pva_rs::pv_request::PvRequestBuilder;

    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:EXR1:PIPE", 0.0).await;

    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let client = client_for(tcp);

    let last_seen = Arc::new(parking_lot::Mutex::new(None::<f64>));
    let count = Arc::new(parking_lot::Mutex::new(0usize));
    let last_cb = last_seen.clone();
    let count_cb = count.clone();

    // Pipelined monitor with a small window AND a decimate-by-3
    // server-side filter. The filter drops ~2 of every 3 events.
    let req = PvRequestBuilder::new()
        .record("pipeline", "true")
        .record("queueSize", "4")
        .record("_filter", r#"{"dec":{"n":3}}"#)
        .build();

    let monitor_handle = tokio::spawn({
        let client = client.clone();
        async move {
            let _ = client
                .pvmonitor_with_request("STAB:EXR1:PIPE", &req, move |value| {
                    if let PvField::Structure(s) = value
                        && let Some(ScalarValue::Double(v)) = s.get_value()
                    {
                        *last_cb.lock() = Some(*v);
                        *count_cb.lock() += 1;
                    }
                })
                .await;
        }
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Push 30 values. With dec n=3 the filter drops far more than the
    // 4-deep window, so a credit-on-drop bug stalls the stream within
    // the first handful of pushes.
    const N: i32 = 30;
    for i in 1..=N {
        source.push("STAB:EXR1:PIPE", i as f64).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;

    let last = *last_seen.lock();
    let seen = *count.lock();
    // The stream must not have stalled: the last value the client
    // observed must be a late push, proving credit kept flowing past
    // the filter-dropped events.
    assert!(
        last.is_some_and(|v| v >= (N as f64) - 6.0),
        "Regression: pipelined monitor stalled — last value {last:?} \
         (expected close to {N}); filter-dropped events consumed window credit"
    );
    // And more than the window's worth of frames were delivered, which
    // is impossible if credit never refilled.
    assert!(
        seen > 4,
        "Regression: only {seen} frames delivered — window never refilled"
    );

    monitor_handle.abort();
    h.abort();
}

/// pvxs `serverchan.cpp:269-358` parity: a single CREATE_CHANNEL
/// frame can carry `count` (cid, name) pairs and the server must
/// emit one reply per pair, in arrival order. Rust used to consume
/// the first pair and silently drop the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_channel_multi_name_emits_one_reply_per_name() {
    use std::io::Write;

    use epics_pva_rs::codec::CMD_CREATE_CHANNEL;
    use epics_pva_rs::proto::encode_string_into;
    use epics_pva_rs::proto::{ByteOrder, Command, PvaHeader, ReadExt, Status, WriteExt};

    let source = Arc::new(MemSource::new());
    source.add_pv("MULTI:NAME:A", 1.0).await;
    source.add_pv("MULTI:NAME:B", 2.0).await;
    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), tcp);

    // Complete the handshake by replying with an anonymous
    // CONNECTION_VALIDATION.
    let mut sock = read_handshake_prelude(server_addr);
    let order = ByteOrder::Little;
    let mut payload: Vec<u8> = Vec::new();
    payload.put_u32(0x10000, order);
    payload.put_u16(32_767, order);
    payload.put_u16(0, order);
    encode_string_into("anonymous", order, &mut payload);
    payload.put_u8(0xFF);
    let h_req = PvaHeader::application(
        false,
        order,
        Command::ConnectionValidation.code(),
        payload.len() as u32,
    );
    let mut req = Vec::new();
    h_req.write_into(&mut req);
    req.extend_from_slice(&payload);
    sock.write_all(&req).unwrap();
    let mut reader = FrameReader::new();
    let _validated = reader.read(&mut sock);

    // Build a single CREATE_CHANNEL frame carrying TWO (cid, name)
    // pairs — pvxs format.
    let mut body = Vec::new();
    body.put_u16(2, order); // count
    body.put_u32(101, order);
    encode_string_into("MULTI:NAME:A", order, &mut body);
    body.put_u32(202, order);
    encode_string_into("MULTI:NAME:B", order, &mut body);
    let h_req = PvaHeader::application(false, order, CMD_CREATE_CHANNEL, body.len() as u32);
    let mut frame_bytes = Vec::new();
    h_req.write_into(&mut frame_bytes);
    frame_bytes.extend_from_slice(&body);
    sock.write_all(&frame_bytes).unwrap();

    // Read two CREATE_CHANNEL response frames back. Order = arrival
    // order = A then B.
    let resp_a = reader.read(&mut sock);
    assert_eq!(resp_a.header.command, CMD_CREATE_CHANNEL);
    let mut cur = resp_a.cursor();
    let cid_a = cur.get_u32(order).unwrap();
    let _sid_a = cur.get_u32(order).unwrap();
    let status_a = Status::decode(&mut cur, order).unwrap();
    assert_eq!(cid_a, 101);
    assert!(status_a.is_success(), "first reply failed: {status_a:?}");

    let resp_b = reader.read(&mut sock);
    assert_eq!(resp_b.header.command, CMD_CREATE_CHANNEL);
    let mut cur = resp_b.cursor();
    let cid_b = cur.get_u32(order).unwrap();
    let _sid_b = cur.get_u32(order).unwrap();
    let status_b = Status::decode(&mut cur, order).unwrap();
    assert_eq!(cid_b, 202);
    assert!(status_b.is_success(), "second reply failed: {status_b:?}");

    h.abort();
}

/// PVA-126 / pvxs `serverchan.cpp:328-351` parity: a CREATE_CHANNEL for
/// an unclaimed (missing) PV is a *refused* channel. pvxs replies with
/// `sid = -1` (0xFFFFFFFF) and a Fatal status "Refused to create Channel"
/// (trace "pvx:serv:refusechan:"), not a recoverable Error. A direct-TCP
/// client that bypasses UDP SEARCH must see the Fatal refusal class.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_channel_missing_pv_refused_fatal() {
    use std::io::Write;

    use epics_pva_rs::codec::CMD_CREATE_CHANNEL;
    use epics_pva_rs::proto::encode_string_into;
    use epics_pva_rs::proto::{
        ByteOrder, Command, PvaHeader, ReadExt, Status, StatusKind, WriteExt,
    };

    // Source hosts one PV; we will ask for a different, missing one.
    let source = Arc::new(MemSource::new());
    source.add_pv("PRESENT:PV", 1.0).await;
    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), tcp);

    let mut sock = read_handshake_prelude(server_addr);
    let order = ByteOrder::Little;
    let mut payload: Vec<u8> = Vec::new();
    payload.put_u32(0x10000, order);
    payload.put_u16(32_767, order);
    payload.put_u16(0, order);
    encode_string_into("anonymous", order, &mut payload);
    payload.put_u8(0xFF);
    let h_req = PvaHeader::application(
        false,
        order,
        Command::ConnectionValidation.code(),
        payload.len() as u32,
    );
    let mut req = Vec::new();
    h_req.write_into(&mut req);
    req.extend_from_slice(&payload);
    sock.write_all(&req).unwrap();
    let mut reader = FrameReader::new();
    let _validated = reader.read(&mut sock);

    // CREATE_CHANNEL for a PV the source does not host.
    let mut body = Vec::new();
    body.put_u16(1, order); // count
    body.put_u32(303, order);
    encode_string_into("ABSENT:PV", order, &mut body);
    let h_req = PvaHeader::application(false, order, CMD_CREATE_CHANNEL, body.len() as u32);
    let mut frame_bytes = Vec::new();
    h_req.write_into(&mut frame_bytes);
    frame_bytes.extend_from_slice(&body);
    sock.write_all(&frame_bytes).unwrap();

    let resp = reader.read(&mut sock);
    assert_eq!(resp.header.command, CMD_CREATE_CHANNEL);
    let mut cur = resp.cursor();
    let cid = cur.get_u32(order).unwrap();
    let sid = cur.get_u32(order).unwrap();
    let status = Status::decode(&mut cur, order).unwrap();
    assert_eq!(cid, 303);
    assert_eq!(
        sid,
        u32::MAX,
        "refused channel must use the no-channel SID sentinel"
    );
    match status {
        Status::Detailed {
            kind,
            message,
            stack,
        } => {
            assert_eq!(
                kind,
                StatusKind::Fatal,
                "refused channel must be Fatal, not Error"
            );
            assert_eq!(message, "Refused to create Channel");
            assert_eq!(stack, "pvx:serv:refusechan:");
        }
        other => panic!("expected a detailed Fatal refusal, got {other:?}"),
    }

    h.abort();
}

/// pvxs `servermon.cpp:691-708` parity: the MONITOR command's destroy bit
/// (`subcmd & 0x10`) tears the op down in any non-INIT MONITOR frame, the
/// same as the dedicated `DESTROY_REQUEST` command. Before the fix the
/// data-phase MONITOR branch had no `0x10` handler, so the op stayed live
/// and a later INIT reusing the same IOID was rejected as a duplicate
/// (connection-fatal, tcp.rs:4396-4399). This drives MONITOR INIT → START
/// → `0x10` and asserts the IOID is re-INITable afterward.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn monitor_command_destroy_subcmd_frees_op_for_reinit() {
    use std::io::Write;

    use epics_pva_rs::codec::{CMD_CREATE_CHANNEL, CMD_MONITOR, PvaCodec};
    use epics_pva_rs::proto::encode_string_into;
    use epics_pva_rs::proto::{ByteOrder, Command, PvaHeader, ReadExt, Status, WriteExt};

    let source = Arc::new(MemSource::new());
    source.add_pv("MON:DESTROY:PV", 1.0).await;
    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), tcp);

    let mut sock = read_handshake_prelude(server_addr);
    let order = ByteOrder::Little;

    // CONNECTION_VALIDATION (anonymous).
    let mut payload: Vec<u8> = Vec::new();
    payload.put_u32(0x10000, order);
    payload.put_u16(32_767, order);
    payload.put_u16(0, order);
    encode_string_into("anonymous", order, &mut payload);
    payload.put_u8(0xFF);
    let hv = PvaHeader::application(
        false,
        order,
        Command::ConnectionValidation.code(),
        payload.len() as u32,
    );
    let mut req = Vec::new();
    hv.write_into(&mut req);
    req.extend_from_slice(&payload);
    sock.write_all(&req).unwrap();
    let mut reader = FrameReader::new();
    let _validated = reader.read(&mut sock);

    // CREATE_CHANNEL for the hosted PV → sid.
    let mut body = Vec::new();
    body.put_u16(1, order);
    body.put_u32(303, order);
    encode_string_into("MON:DESTROY:PV", order, &mut body);
    let hc = PvaHeader::application(false, order, CMD_CREATE_CHANNEL, body.len() as u32);
    let mut frame_bytes = Vec::new();
    hc.write_into(&mut frame_bytes);
    frame_bytes.extend_from_slice(&body);
    sock.write_all(&frame_bytes).unwrap();
    let resp = reader.read(&mut sock);
    assert_eq!(resp.header.command, CMD_CREATE_CHANNEL);
    let mut cur = resp.cursor();
    let _cid = cur.get_u32(order).unwrap();
    let sid = cur.get_u32(order).unwrap();
    assert_ne!(sid, u32::MAX, "channel for a hosted PV must resolve");

    let codec = PvaCodec { big_endian: false };
    let pv_req = epics_pva_rs::pv_request::build_pv_request_value_only(false);
    let ioid = 77u32;

    // Read a MONITOR INIT reply (`ioid + subcmd + Status`) and assert it is
    // a successful INIT echoing the 0x08 INIT subcmd.
    fn assert_init_ok(
        reader: &mut FrameReader,
        sock: &mut std::net::TcpStream,
        order: ByteOrder,
        ioid: u32,
    ) {
        let f = reader.read(sock);
        assert_eq!(
            f.header.command, CMD_MONITOR,
            "expected a MONITOR INIT reply"
        );
        let mut c = f.cursor();
        assert_eq!(c.get_u32(order).unwrap(), ioid);
        let sub = c.get_u8().unwrap();
        assert!(
            sub & 0x08 != 0,
            "INIT reply must echo subcmd 0x08, got {sub:#x}"
        );
        let st = Status::decode(&mut c, order).unwrap();
        assert!(st.is_success(), "MONITOR INIT must succeed, got {st:?}");
    }

    // 1. MONITOR INIT.
    sock.write_all(&codec.build_monitor_init(sid, ioid, &pv_req, None))
        .unwrap();
    assert_init_ok(&mut reader, &mut sock, order, ioid);

    // 2. MONITOR START → drain the initial data frame (exercises teardown
    //    of a *started* subscriber, not just an idle INIT'd op).
    sock.write_all(&codec.build_monitor_start(sid, ioid))
        .unwrap();
    let data = reader.read(&mut sock);
    assert_eq!(
        data.header.command, CMD_MONITOR,
        "START should yield the initial MONITOR data frame"
    );

    // 3. Destroy via the MONITOR command's 0x10 bit (no reply expected).
    sock.write_all(&codec.build_monitor_destroy(sid, ioid))
        .unwrap();

    // 4. Re-INIT the SAME IOID. Without the 0x10 handler this is a
    //    duplicate-live-op fault that tears the connection down; with it,
    //    the IOID is free and the fresh INIT succeeds.
    sock.write_all(&codec.build_monitor_init(sid, ioid, &pv_req, None))
        .unwrap();
    assert_init_ok(&mut reader, &mut sock, order, ioid);

    h.abort();
}

/// Connect to a freshly started PVA server and drain the server's
/// SET_BYTE_ORDER + CONNECTION_VALIDATION prologue. Polls for up to
/// one second so the per-thread spawn race doesn't surface as a
/// `WouldBlock` on a freshly accepted socket.
fn read_handshake_prelude(server_addr: std::net::SocketAddr) -> std::net::TcpStream {
    use std::io::Read;
    let mut sock = std::net::TcpStream::connect(server_addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    sock.set_nodelay(true).ok();
    let mut prelude = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    // The server emits 8B SetByteOrder control frame + a CONN_VALID
    // request frame whose total length depends on the advertised
    // auth method list (~50 B for ["ca","anonymous"]). Keep reading
    // until we see at least the second frame.
    while std::time::Instant::now() < deadline {
        let mut chunk = [0u8; 256];
        match sock.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                prelude.extend_from_slice(&chunk[..n]);
                if prelude.len() >= 16 {
                    break; // we have at least one control frame and part of the next
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => panic!("prelude read failed: {e}"),
        }
    }
    assert!(
        !prelude.is_empty(),
        "server did not emit prelude within deadline"
    );
    sock
}

/// A persistent rx buffer for the hand-spoken test sockets. Holds
/// leftover bytes across [`read_one_frame_buf`] calls so a frame
/// burst (e.g. multiple CREATE_CHANNEL responses in one syscall)
/// doesn't get truncated after the first parse.
struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn read(
        &mut self,
        sock: &mut std::net::TcpStream,
    ) -> epics_pva_rs::client_native::decode::Frame {
        use epics_pva_rs::client_native::decode::try_parse_frame;
        use std::io::Read;

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(Some((frame, n))) = try_parse_frame(&self.buf) {
                self.buf.drain(..n);
                return frame;
            }
            if std::time::Instant::now() >= deadline {
                panic!("did not receive a complete frame within deadline");
            }
            let mut chunk = [0u8; 512];
            match sock.read(&mut chunk) {
                Ok(0) => continue,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => panic!("frame read failed: {e}"),
            }
        }
    }
}

/// Shorthand for tests that read exactly one frame and discard the
/// caller-side buffer (handshake-style "read the next reply").
fn read_one_frame(sock: &mut std::net::TcpStream) -> epics_pva_rs::client_native::decode::Frame {
    FrameReader::new().read(sock)
}

/// pvxs `serverconn.cpp:238-241` parity: when the client picks an
/// auth method we never advertised (e.g. "x509" against our
/// `["ca","anonymous"]` advertisement), the server's
/// `CONNECTION_VALIDATED` frame must carry `Status::Error` ("Client
/// selects unadvertised auth"). The connection stays open — pvxs
/// keeps it alive and just denies elevated rights — so anonymous
/// access still works downstream.
///
/// Multi-thread runtime is required because the test does blocking
/// `std::net::TcpStream` reads on the same task that drives the
/// server's spawn; current-thread starves the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_method_unadvertised_returns_status_error() {
    use std::io::Write;

    use epics_pva_rs::codec::CMD_CONNECTION_VALIDATED;
    use epics_pva_rs::proto::encode_string_into;
    use epics_pva_rs::proto::{ByteOrder, Command, PvaHeader, Status, WriteExt};

    let source = Arc::new(MemSource::new());
    source.add_pv("AUTH:UNADV", 0.0).await;
    let (tcp, _udp, h) = spawn_server(source.clone()).await;

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), tcp);

    // Hand-speak the handshake so we can dictate the exact `selected`
    // string. Using `PvaClient` would always pick "anonymous".
    // Brief poll loop: the server task spawns asynchronously after
    // `spawn_server` returns, and the SetByteOrder + ConnValidation
    // request frames can take a few ms to appear on a freshly
    // accepted socket. Read with a generous timeout and retry until
    // at least both prologue frames have arrived.
    let mut sock = read_handshake_prelude(server_addr);

    // Build CONNECTION_VALIDATION reply with method="x509" — never
    // advertised by the server.
    let order = ByteOrder::Little;
    let mut payload: Vec<u8> = Vec::new();
    payload.put_u32(0x10000, order); // client buffer hint (match pvxs 0x10000)
    payload.put_u16(32_767, order); // intro registry size
    payload.put_u16(0, order); // qos
    encode_string_into("x509", order, &mut payload);
    payload.put_u8(0xFF); // null variant — no AuthZ block
    let h_req = PvaHeader::application(
        false,
        order,
        Command::ConnectionValidation.code(),
        payload.len() as u32,
    );
    let mut req = Vec::new();
    h_req.write_into(&mut req);
    req.extend_from_slice(&payload);
    sock.write_all(&req).unwrap();

    // Server's CONNECTION_VALIDATED reply should arrive with
    // Status::Error (not Status::Ok).
    let frame = read_one_frame(&mut sock);
    assert_eq!(
        frame.header.command, CMD_CONNECTION_VALIDATED,
        "expected CONNECTION_VALIDATED, got cmd=0x{:02X}",
        frame.header.command
    );
    let mut cur = frame.cursor();
    let status = Status::decode(&mut cur, order).expect("status");
    assert!(
        !status.is_success(),
        "server accepted unadvertised auth method `x509`: {status:?}"
    );

    // Companion case: an advertised method (`anonymous`) on a fresh
    // connection must still return Status::Ok so the existing
    // anonymous flow doesn't regress.
    let mut sock2 = read_handshake_prelude(server_addr);
    let mut payload: Vec<u8> = Vec::new();
    payload.put_u32(0x10000, order);
    payload.put_u16(32_767, order);
    payload.put_u16(0, order);
    encode_string_into("anonymous", order, &mut payload);
    payload.put_u8(0xFF);
    let h_req = PvaHeader::application(
        false,
        order,
        Command::ConnectionValidation.code(),
        payload.len() as u32,
    );
    let mut req = Vec::new();
    h_req.write_into(&mut req);
    req.extend_from_slice(&payload);
    sock2.write_all(&req).unwrap();
    let frame = read_one_frame(&mut sock2);
    let mut cur = frame.cursor();
    let status = Status::decode(&mut cur, order).expect("status");
    assert!(
        status.is_success(),
        "anonymous (advertised) handshake was rejected: {status:?}"
    );

    h.abort();
}

/// Regression: when a plain-TCP client selects an auth method
/// the server never advertised (`x509`) and includes a non-empty
/// `user` in its auth body, the server returns `Status::Error` AND
/// must revert the connection credential to anonymous. Before the fix
/// the parsed `x509`/`user="alice"` claim was installed into `cred`
/// before the advertised-method check, so the `auth_complete` hook
/// (and every later ACF-gated operation) saw an identity the server
/// had just rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ex_r7_unadvertised_auth_reverts_credential_to_anonymous() {
    use std::io::Write;
    use std::sync::Mutex as StdMutex;

    use epics_pva_rs::codec::CMD_CONNECTION_VALIDATED;
    use epics_pva_rs::proto::encode_string_into;
    use epics_pva_rs::proto::{ByteOrder, Command, PvaHeader, Status, WriteExt};
    use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
    use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};

    // Capture what the auth_complete hook observed.
    let captured: Arc<StdMutex<Option<(String, String)>>> = Arc::new(StdMutex::new(None));
    let captured_hook = captured.clone();

    let source = Arc::new(MemSource::new());
    source.add_pv("AUTH:EXR7", 0.0).await;

    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
        max_channels_per_connection: 64,
        monitor_queue_depth: 8,
        auth_complete: Some(Arc::new(move |_peer, cred| {
            *captured_hook.lock().unwrap() = Some((cred.method.clone(), cred.account.clone()));
        })),
        ..Default::default()
    };
    let server = PvaServer::start(source, cfg).expect("test server must start");
    let tcp = server.report().tcp_port;
    let h = tokio::spawn(async move {
        let _ = server.wait().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), tcp);
    let mut sock = read_handshake_prelude(server_addr);

    // CONNECTION_VALIDATION with method="x509" (unadvertised on plain
    // TCP) and an auth Value structure carrying `user="alice"`.
    let order = ByteOrder::Little;
    let auth_desc = FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![("user".into(), FieldDesc::Scalar(ScalarType::String))],
    };
    let mut auth_struct = PvStructure::new("");
    auth_struct.fields.push((
        "user".into(),
        PvField::Scalar(ScalarValue::String("alice".into())),
    ));
    let auth_val = PvField::Structure(auth_struct);

    let mut payload: Vec<u8> = Vec::new();
    payload.put_u32(0x10000, order);
    payload.put_u16(32_767, order);
    payload.put_u16(0, order);
    encode_string_into("x509", order, &mut payload);
    epics_pva_rs::pvdata::encode::encode_type_desc(&auth_desc, order, &mut payload);
    epics_pva_rs::pvdata::encode::encode_pv_field(&auth_val, &auth_desc, order, &mut payload);
    let h_req = PvaHeader::application(
        false,
        order,
        Command::ConnectionValidation.code(),
        payload.len() as u32,
    );
    let mut req = Vec::new();
    h_req.write_into(&mut req);
    req.extend_from_slice(&payload);
    sock.write_all(&req).unwrap();

    // CONNECTION_VALIDATED reply must carry Status::Error.
    let frame = read_one_frame(&mut sock);
    assert_eq!(
        frame.header.command, CMD_CONNECTION_VALIDATED,
        "expected CONNECTION_VALIDATED"
    );
    let mut cur = frame.cursor();
    let status = Status::decode(&mut cur, order).expect("status");
    assert!(
        !status.is_success(),
        "server accepted unadvertised x509 method: {status:?}"
    );

    // The auth_complete hook must have seen ANONYMOUS credentials —
    // not the rejected `x509`/`alice` claim.
    let observed = captured.lock().unwrap().clone();
    let (method, account) = observed.expect("auth_complete hook must have fired");
    assert_eq!(
        method, "anonymous",
        "rejected unadvertised method must not survive on the connection"
    );
    assert_eq!(
        account, "anonymous",
        "rejected claimed account `alice` must not survive on the connection"
    );

    h.abort();
}

/// Regression: pvxs clients send `method="anonymous"` (non-empty
/// string) + a null auth body (`0xFF`). Pre-fix Rust produced
/// `account=""` for this case because the `is_empty()` early-return
/// only triggered on a truly empty method string, not on "anonymous".
/// Post-fix `method="anonymous"` returns `Ok(None)`, preserving the
/// pre-initialised `ClientCredentials::anonymous()` (account="anonymous").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r70_anonymous_method_yields_account_anonymous() {
    use std::io::Write;
    use std::sync::Mutex as StdMutex;

    use epics_pva_rs::codec::CMD_CONNECTION_VALIDATED;
    use epics_pva_rs::proto::encode_string_into;
    use epics_pva_rs::proto::{ByteOrder, Command, PvaHeader, Status, WriteExt};
    use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};

    let captured: Arc<StdMutex<Option<(String, String)>>> = Arc::new(StdMutex::new(None));
    let captured_hook = captured.clone();

    let source = Arc::new(MemSource::new());
    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
        max_channels_per_connection: 64,
        monitor_queue_depth: 8,
        auth_complete: Some(Arc::new(move |_peer, cred| {
            *captured_hook.lock().unwrap() = Some((cred.method.clone(), cred.account.clone()));
        })),
        ..Default::default()
    };
    let server = PvaServer::start(source, cfg).expect("test server must start");
    let tcp = server.report().tcp_port;
    let h = tokio::spawn(async move {
        let _ = server.wait().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), tcp);
    let mut sock = read_handshake_prelude(server_addr);

    // pvxs-style anonymous handshake: method="anonymous" + 0xFF null body.
    let order = ByteOrder::Little;
    let mut payload: Vec<u8> = Vec::new();
    payload.put_u32(0x10000, order);
    payload.put_u16(32_767, order);
    payload.put_u16(0, order);
    encode_string_into("anonymous", order, &mut payload);
    payload.push(0xFF); // null auth body
    let h_req = PvaHeader::application(
        false,
        order,
        Command::ConnectionValidation.code(),
        payload.len() as u32,
    );
    let mut req = Vec::new();
    h_req.write_into(&mut req);
    req.extend_from_slice(&payload);
    sock.write_all(&req).unwrap();

    let frame = read_one_frame(&mut sock);
    assert_eq!(
        frame.header.command, CMD_CONNECTION_VALIDATED,
        "expected CONNECTION_VALIDATED"
    );
    let mut cur = frame.cursor();
    let status = Status::decode(&mut cur, order).expect("status");
    assert!(
        status.is_success(),
        "anonymous method must succeed: {status:?}"
    );

    let observed = captured.lock().unwrap().clone();
    let (method, account) = observed.expect("auth_complete hook must have fired");
    assert_eq!(method, "anonymous", "method");
    assert_eq!(
        account, "anonymous",
        "anonymous method must produce account=\"anonymous\", not \"\""
    );

    h.abort();
}

/// Regression: `method="ca"` with no `user` field in the auth body
/// must fall back to anonymous credentials (matches pvxs
/// serverconn.cpp:223-231 — the ca lambda only fires when user is present;
/// without it C->method stays empty → anonymous fallback).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r70_ca_without_user_falls_back_to_anonymous() {
    use std::io::Write;
    use std::sync::Mutex as StdMutex;

    use epics_pva_rs::codec::CMD_CONNECTION_VALIDATED;
    use epics_pva_rs::proto::encode_string_into;
    use epics_pva_rs::proto::{ByteOrder, Command, PvaHeader, Status, WriteExt};
    use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
    use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};

    let captured: Arc<StdMutex<Option<(String, String)>>> = Arc::new(StdMutex::new(None));
    let captured_hook = captured.clone();

    let source = Arc::new(MemSource::new());
    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
        max_channels_per_connection: 64,
        monitor_queue_depth: 8,
        auth_complete: Some(Arc::new(move |_peer, cred| {
            *captured_hook.lock().unwrap() = Some((cred.method.clone(), cred.account.clone()));
        })),
        ..Default::default()
    };
    let server = PvaServer::start(source, cfg).expect("test server must start");
    let tcp = server.report().tcp_port;
    let h = tokio::spawn(async move {
        let _ = server.wait().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), tcp);
    let mut sock = read_handshake_prelude(server_addr);

    // ca auth body with only a `host` field (no `user`).
    let order = ByteOrder::Little;
    let auth_desc = FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![("host".into(), FieldDesc::Scalar(ScalarType::String))],
    };
    let mut auth_struct = PvStructure::new("");
    auth_struct.fields.push((
        "host".into(),
        PvField::Scalar(ScalarValue::String("somehost".into())),
    ));
    let auth_val = PvField::Structure(auth_struct);

    let mut payload: Vec<u8> = Vec::new();
    payload.put_u32(0x10000, order);
    payload.put_u16(32_767, order);
    payload.put_u16(0, order);
    encode_string_into("ca", order, &mut payload);
    epics_pva_rs::pvdata::encode::encode_type_desc(&auth_desc, order, &mut payload);
    epics_pva_rs::pvdata::encode::encode_pv_field(&auth_val, &auth_desc, order, &mut payload);
    let h_req = PvaHeader::application(
        false,
        order,
        Command::ConnectionValidation.code(),
        payload.len() as u32,
    );
    let mut req = Vec::new();
    h_req.write_into(&mut req);
    req.extend_from_slice(&payload);
    sock.write_all(&req).unwrap();

    let frame = read_one_frame(&mut sock);
    assert_eq!(
        frame.header.command, CMD_CONNECTION_VALIDATED,
        "expected CONNECTION_VALIDATED"
    );
    let mut cur = frame.cursor();
    let status = Status::decode(&mut cur, order).expect("status");
    assert!(
        status.is_success(),
        "ca-without-user is not an error (accepted but downgraded): {status:?}"
    );

    let observed = captured.lock().unwrap().clone();
    let (method, account) = observed.expect("auth_complete hook must have fired");
    assert_eq!(
        method, "anonymous",
        "ca without user field must fall back to method=\"anonymous\""
    );
    assert_eq!(
        account, "anonymous",
        "ca without user field must fall back to account=\"anonymous\""
    );

    h.abort();
}

/// Regression: PVA `pvget_many` warm path.
///
/// `pvget_many` initializes every result slot to `Err(PvaError::Timeout)`.
/// The first call per channel pays the cold INIT+GET cost and warms the
/// `cached_get` slot. The second call takes the warm path: it sends a
/// single GET frame per channel and awaits the response.
///
/// The bug: Phase 3 used `results[idx].is_err()` as the skip predicate.
/// Because the slot is still the initial `Err(Timeout)`, EVERY warm
/// request was skipped — the function sent valid warm GET frames and
/// then ignored its own response receivers, returning timeout errors
/// even though the server replied.
///
/// Before the fix the second `pvget_many` call returns `Err` for the
/// warm PVs; after the fix it returns `Ok` with the current value.
#[tokio::test]
async fn ex_r4_pvget_many_warm_path_returns_responses() {
    let source = Arc::new(MemSource::new());
    source.add_pv("EXR4:A", 1.0).await;
    source.add_pv("EXR4:B", 2.0).await;

    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let client = client_for(tcp);

    // First call: cold path warms each channel's cached_get slot.
    let first = tokio::time::timeout(
        Duration::from_secs(3),
        client.pvget_many(&["EXR4:A", "EXR4:B"]),
    )
    .await
    .expect("first pvget_many timed out");
    assert!(first[0].is_ok(), "first EXR4:A: {:?}", first[0]);
    assert!(first[1].is_ok(), "first EXR4:B: {:?}", first[1]);

    // Mutate the source so the warm GET must actually read fresh data.
    source.push("EXR4:A", 11.0).await;
    source.push("EXR4:B", 22.0).await;

    // Second call: warm path. The fix makes Phase 3 await every
    // successfully sent warm request instead of skipping all of them.
    let second = tokio::time::timeout(
        Duration::from_secs(3),
        client.pvget_many(&["EXR4:A", "EXR4:B"]),
    )
    .await
    .expect("second pvget_many timed out");

    let val_a = second[0]
        .as_ref()
        .unwrap_or_else(|e| panic!("warm EXR4:A returned error: {e}"));
    let val_b = second[1]
        .as_ref()
        .unwrap_or_else(|e| panic!("warm EXR4:B returned error: {e}"));

    fn scalar_double(f: &PvField) -> f64 {
        match f {
            PvField::Structure(s) => {
                for (name, sub) in &s.fields {
                    if name == "value"
                        && let PvField::Scalar(ScalarValue::Double(d)) = sub
                    {
                        return *d;
                    }
                }
                panic!("no double `value` field");
            }
            _ => panic!("expected NTScalar structure"),
        }
    }
    assert_eq!(scalar_double(val_a), 11.0, "warm EXR4:A read stale value");
    assert_eq!(scalar_double(val_b), 22.0, "warm EXR4:B read stale value");

    h.abort();
}

/// Parity: pvxs consults `ignoreAddrs` only on the UDP SEARCH admission
/// path (`Server::Pvt::onSearch`, server.cpp:654-670); the TCP accept
/// callback registers a `ServerConn` without any ignore-list check
/// (serverconn.cpp:461-467). The Rust server previously rejected TCP
/// accepts from an ignore-list peer, turning a discovery filter into a
/// transport ACL. Here a server that ignores `127.0.0.1` must still
/// accept a direct loopback TCP connection and emit the handshake
/// prelude (SET_BYTE_ORDER + CONNECTION_VALIDATION).
///
/// Multi-thread runtime: `read_handshake_prelude` does a blocking
/// `std::net` read that would starve the server tasks on a
/// current-thread runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ignore_addr_list_does_not_block_direct_tcp_connect() {
    let source = Arc::new(MemSource::new());
    source.add_pv("IGN:PV", 1.0).await;

    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
        max_channels_per_connection: 64,
        monitor_queue_depth: 8,
        // Loopback is on the UDP-search ignore list…
        ignore_addrs: vec![(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0)],
        ..Default::default()
    };
    let server = PvaServer::start(source, cfg).expect("test server must start");
    let tcp = server.report().tcp_port;
    let h = tokio::spawn(async move {
        let _ = server.wait().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), tcp);
    // …yet a direct TCP connect from loopback must still complete the
    // handshake. `read_handshake_prelude` panics if no prelude arrives,
    // which is exactly the regression: pre-fix the accept loop dropped
    // the stream and the prelude never came.
    let _sock = read_handshake_prelude(server_addr);

    h.abort();
}

/// Wire-level single-seed: a `SharedPV` MONITOR START must deliver the
/// connect-time value EXACTLY ONCE.
///
/// `SharedPV` self-seeds: its subscription inbox used to queue the
/// current value before the first update. The server ALSO emitted its
/// own connect-time snapshot via `get_value_checked`. A native monitor
/// over a `SharedPV` therefore delivered the current value twice at
/// START — two identical DATA frames before any `post()`. Clients that
/// treat monitor events as edges (alarms, scans, archive samples,
/// command handling) double-counted the first value.
///
/// The single MONITOR seed owner (`subscribe_seeded`) returns the
/// connect-time value as the seed plus an updates-only stream, so the
/// server emits exactly one initial frame. Mirrors pvxs `SharedPV`,
/// which posts the current value once at attach (`sharedpv.cpp:69-92`)
/// and has no second server-side GET seed.
///
/// Pre-fix: two `1.0` frames at START. Post-fix: exactly one.
// Live client ↔ server monitor over `tokio::net`; the client's connection
// tasks route through the reactor-less callback pool under `rtems-exec-model`.
// Reactor-dependent — gated out feature-ON (doc/pvalink-rtems-design.md §4.2).
#[cfg(not(feature = "rtems-exec-model"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pva_rs_155_shared_pv_monitor_seeds_current_value_once() {
    let pv = SharedPV::new();
    pv.open(nt_scalar_desc(), make_nt_scalar(1.0))
        .expect("open SharedPV");
    let source = SharedSource::new();
    source.add("STAB:SEED155", pv.clone());
    let server = PvaServer::isolated(Arc::new(source)).expect("isolated test server must start");
    let client = server.client_config();

    let received = Arc::new(parking_lot::Mutex::new(Vec::<f64>::new()));
    let cb = received.clone();
    let handle = client
        .pvmonitor_handle(
            "STAB:SEED155",
            move |_desc, value| {
                if let PvField::Structure(s) = value
                    && let Some(ScalarValue::Double(v)) = s.get_value()
                {
                    cb.lock().push(*v);
                }
            },
            |_| {},
        )
        .await
        .expect("subscribe");

    // Generous settle with NO post: a correct seed delivers one frame;
    // the regressed double-seed delivers a second identical frame, which
    // this window would also capture.
    tokio::time::sleep(Duration::from_millis(600)).await;
    {
        let got = received.lock().clone();
        assert_eq!(
            got,
            vec![1.0],
            "MONITOR START must deliver the connect-time value exactly once \
             (double-seed regression delivers it twice)"
        );
    }

    // A real post delivers exactly one more frame — the connect-time
    // value is not re-sent.
    pv.try_post(make_nt_scalar(2.0));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let got = received.lock().clone();
    assert_eq!(
        got,
        vec![1.0, 2.0],
        "after one post the wire carries seed(1.0) then update(2.0) — no duplicate seed"
    );

    handle.stop();
}

// ─────────────────────────────────────────────────────────────────────
// ChannelArray data-phase pre-source failures must reply with a CMD_ARRAY
// error status, not drop silently.
//
// pvAccessCPP is the only ChannelArray server reference (pvxs has no ARRAY
// handler): a data-phase frame whose IOID was never INIT'd draws
// `badIOIDStatus` ("bad request id"), and a second sub-op while one is still
// executing draws `otherRequestPendingStatus` ("other request pending")
// (responseHandlers.cpp:2157,2164). The server used to `return Ok(())` for
// both, so a conforming client blocked until timeout. These tests drive raw
// frames and assert the error reply arrives instead of silence.
// ─────────────────────────────────────────────────────────────────────

use epics_pva_rs::server_native::ChannelContext;
use epics_pva_rs::server_native::source::AccessChecked;

/// An NTScalarArray source whose `channel_array_get` blocks on a semaphore
/// until the test releases a permit. This keeps the first sub-op
/// `Executing` so a second concurrent sub-op deterministically hits the
/// "already executing" path rather than racing the handler's completion.
#[derive(Clone)]
struct GatedArraySource {
    gate: Arc<tokio::sync::Semaphore>,
}

impl ChannelSource for GatedArraySource {
    async fn list_pvs(&self) -> Vec<String> {
        vec!["GATED:ARR".into()]
    }
    fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
        let n = n.to_string();
        async move { n == "GATED:ARR" }
    }
    async fn get_introspection(&self, _: &str) -> Option<FieldDesc> {
        Some(nt_double_array_desc())
    }
    async fn get_value(&self, _: &str) -> Option<PvField> {
        Some(make_nt_double_array(&[1.0, 2.0, 3.0]))
    }
    async fn put_value(&self, _: &str, _: PvField) -> Result<(), OpError> {
        Ok(())
    }
    async fn is_writable(&self, _: &str) -> bool {
        true
    }
    async fn subscribe(&self, _: &str) -> Option<MonitorStream<PvField>> {
        None
    }
    async fn channel_array_init(&self, _: &str, _: ChannelContext) -> Result<FieldDesc, OpError> {
        Ok(FieldDesc::ScalarArray(ScalarType::Double))
    }
    async fn channel_array_get(
        &self,
        _checked: AccessChecked,
        _offset: u32,
        _count: u32,
        _stride: u32,
        _ctx: ChannelContext,
    ) -> Result<PvField, OpError> {
        // Block until the test releases a permit; the op stays `Executing`.
        let _permit = self.gate.acquire().await.expect("array gate closed");
        Ok(PvField::ScalarArray(vec![
            ScalarValue::Double(1.0),
            ScalarValue::Double(2.0),
            ScalarValue::Double(3.0),
        ]))
    }
}

/// Hand-speak the anonymous CONNECTION_VALIDATION reply, mirroring the
/// `create_channel_*` tests above.
fn send_anonymous_validation(
    sock: &mut std::net::TcpStream,
    order: epics_pva_rs::proto::ByteOrder,
) {
    use std::io::Write;

    use epics_pva_rs::proto::{Command, PvaHeader, WriteExt, encode_string_into};

    let mut payload: Vec<u8> = Vec::new();
    payload.put_u32(0x10000, order);
    payload.put_u16(32_767, order);
    payload.put_u16(0, order);
    encode_string_into("anonymous", order, &mut payload);
    payload.put_u8(0xFF);
    let h = PvaHeader::application(
        false,
        order,
        Command::ConnectionValidation.code(),
        payload.len() as u32,
    );
    let mut req = Vec::new();
    h.write_into(&mut req);
    req.extend_from_slice(&payload);
    sock.write_all(&req).unwrap();
}

/// CREATE_CHANNEL one PV and return the resolved server-side `sid`.
fn create_one_channel(
    sock: &mut std::net::TcpStream,
    reader: &mut FrameReader,
    order: epics_pva_rs::proto::ByteOrder,
    cid: u32,
    name: &str,
) -> u32 {
    use std::io::Write;

    use epics_pva_rs::codec::CMD_CREATE_CHANNEL;
    use epics_pva_rs::proto::{PvaHeader, ReadExt, Status, WriteExt, encode_string_into};

    let mut body = Vec::new();
    body.put_u16(1, order);
    body.put_u32(cid, order);
    encode_string_into(name, order, &mut body);
    let h = PvaHeader::application(false, order, CMD_CREATE_CHANNEL, body.len() as u32);
    let mut frame = Vec::new();
    h.write_into(&mut frame);
    frame.extend_from_slice(&body);
    sock.write_all(&frame).unwrap();

    let resp = reader.read(sock);
    let mut cur = resp.cursor();
    let _cid = cur.get_u32(order).unwrap();
    let sid = cur.get_u32(order).unwrap();
    let status = Status::decode(&mut cur, order).unwrap();
    assert!(
        status.is_success(),
        "CREATE_CHANNEL must succeed: {status:?}"
    );
    sid
}

/// Boundary — op absent. A getArray data-phase frame on an IOID that was
/// never INIT'd must draw a CMD_ARRAY "bad request id" error reply, not a
/// silent drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn array_data_phase_unknown_ioid_replies_error_not_silent() {
    use std::io::Write;

    use epics_pva_rs::proto::{
        ByteOrder, Command, PvaHeader, QosFlags, ReadExt, Status, WriteExt, encode_size_into,
    };

    let source = Arc::new(MemSource::new());
    source.add_pv("ARR:UNKIOID", 1.0).await;
    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), tcp);

    let order = ByteOrder::Little;
    let mut sock = read_handshake_prelude(server_addr);
    send_anonymous_validation(&mut sock, order);
    let mut reader = FrameReader::new();
    let _validated = reader.read(&mut sock);

    let sid = create_one_channel(&mut sock, &mut reader, order, 55, "ARR:UNKIOID");

    // getArray data-phase frame for an IOID we never INIT'd.
    let bad_ioid = 0xDEAD_BEEF_u32;
    let mut body = Vec::new();
    body.put_u32(sid, order);
    body.put_u32(bad_ioid, order);
    body.put_u8(QosFlags::GET);
    encode_size_into(0, order, &mut body); // offset
    encode_size_into(0, order, &mut body); // count (to end)
    encode_size_into(1, order, &mut body); // stride
    let fh = PvaHeader::application(false, order, Command::Array.code(), body.len() as u32);
    let mut frame = Vec::new();
    fh.write_into(&mut frame);
    frame.extend_from_slice(&body);
    sock.write_all(&frame).unwrap();

    // Pre-fix: silent drop → the reader times out (panic). Post-fix: a
    // CMD_ARRAY error frame arrives.
    let resp = reader.read(&mut sock);
    assert_eq!(
        resp.header.command,
        Command::Array.code(),
        "reply must be a CMD_ARRAY frame"
    );
    let mut cur = resp.cursor();
    let ioid = cur.get_u32(order).unwrap();
    let _subcmd = cur.get_u8().unwrap();
    let status = Status::decode(&mut cur, order).unwrap();
    assert_eq!(
        ioid, bad_ioid,
        "ARRAY error reply must echo the offending IOID"
    );
    assert!(
        !status.is_success(),
        "unknown-IOID data phase must reply with an error, got {status:?}"
    );
    assert_eq!(
        status.message(),
        Some("bad request id"),
        "must mirror pvAccessCPP badIOIDStatus"
    );

    h.abort();
}

/// Boundary — op executing. A second sub-op arriving while the first is
/// still in flight must draw a CMD_ARRAY "other request pending" error
/// reply, not a silent drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn array_concurrent_subop_replies_error_not_silent() {
    use std::io::Write;

    use epics_pva_rs::proto::{
        ByteOrder, Command, PvaHeader, QosFlags, ReadExt, Status, WriteExt, encode_size_into,
    };

    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let src = Arc::new(GatedArraySource { gate: gate.clone() });
    let cfg = PvaServerConfig {
        tcp_port: 0,
        udp_port: 0,
        idle_timeout: Duration::from_secs(60),
        ..Default::default()
    };
    let server = PvaServer::start(src, cfg).expect("test server must start");
    let tcp = server.report().tcp_port;
    let h = tokio::spawn(async move {
        let _ = server.wait().await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), tcp);

    let order = ByteOrder::Little;
    let mut sock = read_handshake_prelude(server_addr);
    send_anonymous_validation(&mut sock, order);
    let mut reader = FrameReader::new();
    let _validated = reader.read(&mut sock);

    let sid = create_one_channel(&mut sock, &mut reader, order, 66, "GATED:ARR");

    // ARRAY INIT (ioid = 1) using the default value-only pvRequest.
    let ioid = 1u32;
    let pv_req = epics_pva_rs::pv_request::build_pv_request_value_only(false);
    let mut ibody = Vec::new();
    ibody.put_u32(sid, order);
    ibody.put_u32(ioid, order);
    ibody.put_u8(QosFlags::INIT);
    ibody.extend_from_slice(&pv_req);
    let ih = PvaHeader::application(false, order, Command::Array.code(), ibody.len() as u32);
    let mut iframe = Vec::new();
    ih.write_into(&mut iframe);
    iframe.extend_from_slice(&ibody);
    sock.write_all(&iframe).unwrap();

    let iresp = reader.read(&mut sock);
    assert_eq!(iresp.header.command, Command::Array.code());
    {
        let mut cur = iresp.cursor();
        let _ioid = cur.get_u32(order).unwrap();
        let _subcmd = cur.get_u8().unwrap();
        let istatus = Status::decode(&mut cur, order).unwrap();
        assert!(istatus.is_success(), "ARRAY INIT must succeed: {istatus:?}");
    }

    // Two getArray EXEC frames on ioid=1 in one burst. The first blocks in
    // the gated handler (op stays Executing); the second must draw an error.
    let mut burst = Vec::new();
    for _ in 0..2 {
        let mut b = Vec::new();
        b.put_u32(sid, order);
        b.put_u32(ioid, order);
        b.put_u8(QosFlags::GET);
        encode_size_into(0, order, &mut b);
        encode_size_into(0, order, &mut b);
        encode_size_into(1, order, &mut b);
        let hh = PvaHeader::application(false, order, Command::Array.code(), b.len() as u32);
        hh.write_into(&mut burst);
        burst.extend_from_slice(&b);
    }
    sock.write_all(&burst).unwrap();

    // Pre-fix: the second sub-op is silently dropped and the first is
    // blocked → no reply → the reader times out (panic). Post-fix: an
    // "other request pending" error frame arrives for the second.
    let resp = reader.read(&mut sock);
    assert_eq!(resp.header.command, Command::Array.code());
    let mut cur = resp.cursor();
    let r_ioid = cur.get_u32(order).unwrap();
    let _subcmd = cur.get_u8().unwrap();
    let status = Status::decode(&mut cur, order).unwrap();
    assert_eq!(
        r_ioid, ioid,
        "concurrent-op error reply must echo the op IOID"
    );
    assert!(
        !status.is_success(),
        "second concurrent sub-op must reply with an error, got {status:?}"
    );
    assert_eq!(
        status.message(),
        Some("other request pending"),
        "must mirror pvAccessCPP otherRequestPendingStatus"
    );

    // Release the blocked first sub-op so the server task winds down cleanly.
    gate.add_permits(1);
    h.abort();
}

/// R12-33 — an exhausted pipeline window must stop the REPLY, not the DRAIN.
///
/// pvxs `MonitorOp::maybeReply` simply does not fire while `op->window == 0`
/// (`servermon.cpp:79-83`, and `doReply` bails at `:143`), but
/// `ServerMonitorControl::doPost` keeps running on every post and keeps
/// SQUASHING into the negotiated queue (`:270-283`). So a client that stops
/// ACKing sees, on resume, at most `queueSize` frames whose tail carries the
/// LATEST value — everything past the limit coalesced into the queue tail.
///
/// The port used to `await` the credit INSIDE the event loop's `select!`, so
/// while the window was empty `rx.recv()` was never polled: the source backed
/// up in the mpsc (capacity 64) instead of squashing, and on resume the client
/// was handed ~`64 + limit` distinct historical updates.
///
/// Drives it raw: pipeline with `queueSize=4`, initial nack 1 (so exactly one
/// DATA frame is emitted and the window then sits at 0), push 40 updates with
/// NO ACK, then grant credit and count what comes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r12_33_stalled_pipeline_squashes_at_the_negotiated_limit() {
    use std::io::Write;

    use epics_pva_rs::codec::{CMD_CREATE_CHANNEL, CMD_MONITOR, PvaCodec};
    use epics_pva_rs::proto::encode_string_into;
    use epics_pva_rs::proto::{ByteOrder, Command, PvaHeader, ReadExt, Status, WriteExt};
    use epics_pva_rs::pv_request::PvRequestBuilder;

    const QUEUE_SIZE: usize = 4;
    const BURST: i32 = 200;

    let source = Arc::new(MemSource::new());
    source.add_pv("MON:PIPE:STALL", 0.0).await;
    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), tcp);

    let mut sock = read_handshake_prelude(server_addr);
    let order = ByteOrder::Little;

    // CONNECTION_VALIDATION (anonymous).
    let mut payload: Vec<u8> = Vec::new();
    payload.put_u32(0x10000, order);
    payload.put_u16(32_767, order);
    payload.put_u16(0, order);
    encode_string_into("anonymous", order, &mut payload);
    payload.put_u8(0xFF);
    let hv = PvaHeader::application(
        false,
        order,
        Command::ConnectionValidation.code(),
        payload.len() as u32,
    );
    let mut req = Vec::new();
    hv.write_into(&mut req);
    req.extend_from_slice(&payload);
    sock.write_all(&req).unwrap();
    let mut reader = FrameReader::new();
    let _validated = reader.read(&mut sock);

    // CREATE_CHANNEL → sid.
    let mut body = Vec::new();
    body.put_u16(1, order);
    body.put_u32(909, order);
    encode_string_into("MON:PIPE:STALL", order, &mut body);
    let hc = PvaHeader::application(false, order, CMD_CREATE_CHANNEL, body.len() as u32);
    let mut frame_bytes = Vec::new();
    hc.write_into(&mut frame_bytes);
    frame_bytes.extend_from_slice(&body);
    sock.write_all(&frame_bytes).unwrap();
    let resp = reader.read(&mut sock);
    let mut cur = resp.cursor();
    let _cid = cur.get_u32(order).unwrap();
    let sid = cur.get_u32(order).unwrap();
    assert_ne!(sid, u32::MAX, "channel for a hosted PV must resolve");

    // MONITOR INIT: pipeline, queueSize=4, initial nack = 1 credit.
    let codec = PvaCodec { big_endian: false };
    let pv_req = PvRequestBuilder::new()
        .record("pipeline", "true")
        .record("queueSize", QUEUE_SIZE.to_string())
        .build()
        .encode(false);
    let ioid = 91u32;
    sock.write_all(&codec.build_monitor_init(sid, ioid, &pv_req, Some(1)))
        .unwrap();
    let f = reader.read(&mut sock);
    assert_eq!(f.header.command, CMD_MONITOR);
    let mut c = f.cursor();
    assert_eq!(c.get_u32(order).unwrap(), ioid);
    assert!(c.get_u8().unwrap() & 0x08 != 0, "INIT reply");
    assert!(
        Status::decode(&mut c, order).unwrap().is_success(),
        "pipeline MONITOR INIT must succeed"
    );

    // START → the seed DATA frame spends the single initial credit. The
    // window is now 0 and stays there: we send no ACK.
    sock.write_all(&codec.build_monitor_start(sid, ioid))
        .unwrap();
    let seed = reader.read(&mut sock);
    assert_eq!(seed.header.command, CMD_MONITOR, "START yields the seed");

    // Burst well past both the queue limit and the source channel capacity
    // (64) while the client owes credit.
    for i in 1..=BURST {
        source.push("MON:PIPE:STALL", i as f64).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Grant far more credit than the queue can hold, then read until the
    // server goes quiet.
    sock.write_all(&codec.build_monitor_ack(sid, ioid, 1000))
        .unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(400)))
        .unwrap();

    let mut values: Vec<f64> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut idle_since = std::time::Instant::now();
    while std::time::Instant::now() < deadline {
        use epics_pva_rs::client_native::decode::try_parse_frame;
        use std::io::Read;
        if let Ok(Some((frame, n))) = try_parse_frame(&buf) {
            buf.drain(..n);
            idle_since = std::time::Instant::now();
            if frame.header.command == CMD_MONITOR {
                let mut c = frame.cursor();
                let _ioid = c.get_u32(order).unwrap();
                let subcmd = c.get_u8().unwrap();
                if subcmd == 0x00 {
                    // changed bitset + value + overrun
                    let changed =
                        epics_pva_rs::proto::BitSet::decode(&mut c, order).expect("changed bitset");
                    let v = epics_pva_rs::pvdata::encode::decode_pv_field_with_bitset(
                        &nt_scalar_desc(),
                        &changed,
                        0,
                        &mut c,
                        order,
                    )
                    .expect("decode monitor value");
                    if let PvField::Structure(s) = v
                        && let Some(ScalarValue::Double(d)) = s.get_value()
                    {
                        values.push(*d);
                    }
                }
            }
            continue;
        }
        let mut chunk = [0u8; 1024];
        match sock.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Server quiet for a while → the backlog is drained.
                if idle_since.elapsed() > Duration::from_millis(600) {
                    break;
                }
            }
            Err(e) => panic!("read failed: {e}"),
        }
    }

    assert!(
        !values.is_empty(),
        "the resumed pipeline must deliver the backlog"
    );
    assert_eq!(
        values.last().copied(),
        Some(BURST as f64),
        "the queue tail must carry the LATEST value ({BURST}), got {values:?}"
    );
    assert!(
        values.len() <= QUEUE_SIZE,
        "one negotiated limit governs the squash: a stalled pipeline must \
         coalesce everything past queueSize={QUEUE_SIZE} into the queue tail, \
         but {} distinct updates were delivered: {values:?}",
        values.len()
    );

    h.abort();
}

/// R12-34 adjudication lock: a MONITOR INIT whose pvRequest selects no
/// existing field is an **op-level** error and the circuit stays up.
///
/// `request2mask()` throws `"pvRequest must select at least one field"`
/// (`pvrequest.cpp:61-62`), but it runs inside `ServerMonitorSetup::connect()`
/// (`servermon.cpp:402`) — i.e. inside the *source's* connect callback, not in
/// the protocol handler. `servermon.cpp:591-592` calls `chan->onSubscribe(...)`
/// unguarded, so who catches the throw is the source's choice, and pvxs's own
/// hosting source catches it: `SharedPV::Impl::connectSub`
/// (`sharedpv.cpp:76,94-101`) wraps `conn->connect()` and calls
/// `conn->error(msg)` ("not re-throwing for consistency") — an op-level Status
/// reply, circuit intact. pvxs's regression for this very throw
/// (`test/testget.cpp:380-393`, SharedPV mailbox, `.field("invalid")`) asserts
/// exactly that remote error.
///
/// The circuit reset one can observe against a C QSRV IOC comes from QSRV's
/// sources alone (`ioc/singlesource.cpp:147`, `ioc/groupsource.cpp:399` call
/// `connect()` bare, so the throw unwinds through `servermon.cpp:592` into
/// `conn.cpp:277-282`'s `bev.reset()`), which drops the shared TCP circuit
/// carrying every other channel on it. That is an upstream defect, not the
/// contract; this test pins the SharedPV behaviour so nobody "fixes" the
/// server into resetting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r12_34_monitor_empty_mask_is_an_op_error_not_a_circuit_reset() {
    use std::io::Write;

    use epics_pva_rs::codec::{CMD_CREATE_CHANNEL, CMD_MONITOR, PvaCodec};
    use epics_pva_rs::proto::encode_string_into;
    use epics_pva_rs::proto::{ByteOrder, Command, PvaHeader, ReadExt, Status, WriteExt};
    use epics_pva_rs::pv_request::PvRequestBuilder;

    let source = Arc::new(MemSource::new());
    source.add_pv("MON:EMPTY:MASK", 1.0).await;
    let (tcp, _udp, h) = spawn_server(source.clone()).await;
    let server_addr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), tcp);

    let mut sock = read_handshake_prelude(server_addr);
    let order = ByteOrder::Little;

    // CONNECTION_VALIDATION (anonymous).
    let mut payload: Vec<u8> = Vec::new();
    payload.put_u32(0x10000, order);
    payload.put_u16(32_767, order);
    payload.put_u16(0, order);
    encode_string_into("anonymous", order, &mut payload);
    payload.put_u8(0xFF);
    let hv = PvaHeader::application(
        false,
        order,
        Command::ConnectionValidation.code(),
        payload.len() as u32,
    );
    let mut req = Vec::new();
    hv.write_into(&mut req);
    req.extend_from_slice(&payload);
    sock.write_all(&req).unwrap();
    let mut reader = FrameReader::new();
    let _validated = reader.read(&mut sock);

    // CREATE_CHANNEL → sid.
    let mut body = Vec::new();
    body.put_u16(1, order);
    body.put_u32(808, order);
    encode_string_into("MON:EMPTY:MASK", order, &mut body);
    let hc = PvaHeader::application(false, order, CMD_CREATE_CHANNEL, body.len() as u32);
    let mut frame_bytes = Vec::new();
    hc.write_into(&mut frame_bytes);
    frame_bytes.extend_from_slice(&body);
    sock.write_all(&frame_bytes).unwrap();
    let resp = reader.read(&mut sock);
    let mut cur = resp.cursor();
    let _cid = cur.get_u32(order).unwrap();
    let sid = cur.get_u32(order).unwrap();
    assert_ne!(sid, u32::MAX, "channel for a hosted PV must resolve");

    let codec = PvaCodec { big_endian: false };

    // MONITOR INIT selecting a field the NTScalar prototype does not have.
    let bad_req = PvRequestBuilder::new()
        .field("noSuchField")
        .build()
        .encode(false);
    let bad_ioid = 71u32;
    sock.write_all(&codec.build_monitor_init(sid, bad_ioid, &bad_req, None))
        .unwrap();
    let f = reader.read(&mut sock);
    assert_eq!(
        f.header.command, CMD_MONITOR,
        "the empty-mask INIT must be answered on the MONITOR command"
    );
    let mut c = f.cursor();
    assert_eq!(c.get_u32(order).unwrap(), bad_ioid);
    assert!(
        c.get_u8().unwrap() & 0x08 != 0,
        "the error must arrive on the INIT subcmd"
    );
    let st = Status::decode(&mut c, order).unwrap();
    assert!(
        !st.is_success(),
        "field(noSuchField) selects nothing in the NTScalar prototype — pvxs's \
         request2mask throws and SharedPV turns it into an op error, got {st:?}"
    );

    // …and the circuit is still usable: a second MONITOR INIT with a
    // wildcard pvRequest, on the SAME socket and SID, must be answered.
    let good_req = PvRequestBuilder::new().build().encode(false);
    let good_ioid = 72u32;
    sock.write_all(&codec.build_monitor_init(sid, good_ioid, &good_req, None))
        .unwrap();
    let f = reader.read(&mut sock);
    assert_eq!(
        f.header.command, CMD_MONITOR,
        "the circuit was dropped after an empty-mask MONITOR INIT — pvxs's SharedPV \
         keeps it up (sharedpv.cpp:94-101)"
    );
    let mut c = f.cursor();
    assert_eq!(c.get_u32(order).unwrap(), good_ioid);
    assert!(c.get_u8().unwrap() & 0x08 != 0, "INIT reply");
    let st = Status::decode(&mut c, order).unwrap();
    assert!(
        st.is_success(),
        "a valid MONITOR INIT on the surviving circuit must succeed, got {st:?}"
    );

    h.abort();
}
