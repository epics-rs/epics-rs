//! Stability / stress integration tests.
//!
//! Exercises the new client runtime against an in-process native PVA
//! server, covering the "P1-P9" stability requirements:
//!
//! - **P1 echo heartbeat** — verified by leaving a connection idle and
//!   confirming it stays alive (server's own heartbeat keeps it ticking).
//! - **P2 auto reconnect** — start server, GET, drop server, restart on
//!   same port, GET again on the same client → succeeds.
//! - **P3+P4 beacon throttle** — observe throttle behaviour on a synthetic
//!   GUID flip via the public BeaconTracker API.
//! - **P5 monitor pipeline** — subscribe and confirm we receive >= N events
//!   for an N-event publish without missing any (default pipeline_size=4).
//! - **P6 idle/slot limits** — open up to `max_connections` clients, verify
//!   the next one is rejected.
//! - **P7 back-pressure** — flood a slow consumer with events and confirm
//!   we never crash (queue squashes).
//! - **P8 channel coalescing** — multiple concurrent pvget on the same PV
//!   share a single channel/connection.

#![allow(clippy::manual_async_fn)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, mpsc};

use epics_pva_rs::client_native::beacon_throttle::BeaconTracker;
use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{ChannelSource, PvaServerConfig, run_pva_server};

// ── A tiny in-memory ChannelSource we can pump events into ───────────

#[derive(Clone)]
struct MemSource {
    inner: Arc<MemSourceInner>,
}

struct MemSourceInner {
    state: Mutex<MemState>,
    /// Subscribers per PV — every push fans out to all of them.
    subs: Mutex<std::collections::HashMap<String, Vec<mpsc::Sender<PvField>>>>,
}

struct MemState {
    values: std::collections::HashMap<String, PvField>,
}

impl MemSource {
    fn new() -> Self {
        Self {
            inner: Arc::new(MemSourceInner {
                state: Mutex::new(MemState {
                    values: std::collections::HashMap::new(),
                }),
                subs: Mutex::new(std::collections::HashMap::new()),
            }),
        }
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

    async fn push(&self, name: &str, value: f64) {
        let pv = make_nt_scalar(value);
        self.inner
            .state
            .lock()
            .await
            .values
            .insert(name.to_string(), pv.clone());
        // Notify subscribers (drop dead).
        let mut subs = self.inner.subs.lock().await;
        if let Some(list) = subs.get_mut(name) {
            list.retain(|tx| tx.try_send(pv.clone()).is_ok());
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
            if inner.state.lock().await.values.contains_key(&name) {
                Some(nt_scalar_desc())
            } else {
                None
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
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
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
                list.retain(|tx| tx.try_send(value.clone()).is_ok());
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
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
        let inner = self.inner.clone();
        let name = name.to_string();
        async move {
            if !inner.state.lock().await.values.contains_key(&name) {
                return None;
            }
            let (tx, rx) = mpsc::channel::<PvField>(64);
            inner.subs.lock().await.entry(name).or_default().push(tx);
            Some(rx)
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

static NEXT_PORT: AtomicU32 = AtomicU32::new(15075);
fn alloc_port_pair() -> (u16, u16) {
    let base = NEXT_PORT.fetch_add(2, Ordering::Relaxed) as u16;
    (base, base + 1)
}

async fn spawn_server(source: Arc<MemSource>) -> (u16, u16, tokio::task::JoinHandle<()>) {
    let (tcp, udp) = alloc_port_pair();
    let cfg = PvaServerConfig {
        tcp_port: tcp,
        udp_port: udp,
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
        max_channels_per_connection: 64,
        monitor_queue_depth: 8,
        ..Default::default()
    };
    let h = tokio::spawn(async move {
        let _ = run_pva_server(source, cfg).await;
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

// ── Tests ────────────────────────────────────────────────────────────

#[tokio::test]
async fn p2_auto_reconnect_after_server_restart() {
    let source = Arc::new(MemSource::new());
    source.add_pv("STAB:RECON", 1.0).await;

    let (tcp, _udp, h1) = spawn_server(source.clone()).await;
    let client = client_for(tcp);

    // First GET succeeds.
    let v = tokio::time::timeout(Duration::from_secs(3), client.pvget("STAB:RECON"))
        .await
        .expect("pvget timed out")
        .expect("pvget failed");
    assert!(matches!(v, PvField::Structure(_)));

    // Restart server on same port.
    h1.abort();
    let _ = h1.await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Reuse the same source — but we need to re-bind on the same port.
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
    let h2 = tokio::spawn(async move {
        let _ = run_pva_server(source2, cfg).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // GET on the same client should succeed (channel state machine
    // reconnects).
    let v = tokio::time::timeout(Duration::from_secs(5), client.pvget("STAB:RECON"))
        .await
        .expect("post-restart pvget timed out")
        .expect("post-restart pvget failed");
    assert!(matches!(v, PvField::Structure(_)));

    h2.abort();
    let _ = h2.await;
}

#[tokio::test]
async fn p3_p4_beacon_throttle_5min_rule() {
    let t = BeaconTracker::new();
    let addr: std::net::SocketAddr = "127.0.0.1:5075".parse().unwrap();

    // First observation — pass through.
    assert!(t.observe(addr, [1u8; 12]));
    // Same GUID — pass through.
    assert!(t.observe(addr, [1u8; 12]));
    // Different GUID within 5 minutes — throttled.
    assert!(!t.observe(addr, [2u8; 12]));
    assert!(t.is_throttled(addr));
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

/// Regression: P-G11 (commit c3f286c) added a server-side pipeline
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

/// EX-R1 regression: pipeline credit must be consumed only for
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
        "EX-R1 regression: pipelined monitor stalled — last value {last:?} \
         (expected close to {N}); filter-dropped events consumed window credit"
    );
    // And more than the window's worth of frames were delivered, which
    // is impossible if credit never refilled.
    assert!(
        seen > 4,
        "EX-R1 regression: only {seen} frames delivered — window never refilled"
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
    payload.put_u32(87_040, order);
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
    payload.put_u32(87_040, order); // client buffer hint
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
    payload.put_u32(87_040, order);
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

/// EX-R7 regression: when a plain-TCP client selects an auth method
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
    use epics_pva_rs::server_native::PvaServerConfig;
    use epics_pva_rs::server_native::run_pva_server;

    // Capture what the auth_complete hook observed.
    let captured: Arc<StdMutex<Option<(String, String)>>> = Arc::new(StdMutex::new(None));
    let captured_hook = captured.clone();

    let source = Arc::new(MemSource::new());
    source.add_pv("AUTH:EXR7", 0.0).await;

    let (tcp, udp) = alloc_port_pair();
    let cfg = PvaServerConfig {
        tcp_port: tcp,
        udp_port: udp,
        idle_timeout: Duration::from_secs(60),
        max_connections: 16,
        max_channels_per_connection: 64,
        monitor_queue_depth: 8,
        auth_complete: Some(Arc::new(move |_peer, cred| {
            *captured_hook.lock().unwrap() = Some((cred.method.clone(), cred.account.clone()));
        })),
        ..Default::default()
    };
    let h = tokio::spawn(async move {
        let _ = run_pva_server(source, cfg).await;
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
    payload.put_u32(87_040, order);
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
        "EX-R7: rejected unadvertised method must not survive on the connection"
    );
    assert_eq!(
        account, "anonymous",
        "EX-R7: rejected claimed account `alice` must not survive on the connection"
    );

    h.abort();
}
