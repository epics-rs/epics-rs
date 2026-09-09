//! CREATE_CHANNEL resolution is bounded per connection.
//!
//! pvxs resolves every CREATE_CHANNEL name inline on the connection's read
//! path (`serverchan.cpp:298` `onCreate`), so a peer that sends names faster
//! than the source answers is held by TCP backpressure. The Rust server
//! resolves on worker tasks; before this test, every frame spawned its own
//! resolver task with nothing bounding how many ran at once.
//!
//! The source below holds every `has_pv` on a closed gate so resolution
//! cannot complete until the test opens it, while a raw peer writes a burst
//! of single-name CREATE_CHANNEL frames.

#![cfg(tokio_backend)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use epics_pva_rs::proto::{ByteOrder, Command, PvaHeader, ReadExt, WriteExt, encode_string_into};
use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarType};
use epics_pva_rs::server_native::{
    ChannelSource, MonitorStream, OpError, PvaServer, PvaServerConfig,
};

const ORDER: ByteOrder = ByteOrder::Little;
/// Frames the peer writes in one burst. Far above the server's request
/// queue, so a loop that keeps reading is visible in the peer report.
const BURST: usize = 1000;
/// `CREATE_CHANNEL_RESOLVE_CONCURRENCY` (16): the most names the server
/// may have inside `has_pv` at once for one connection.
const RESOLVE_CONCURRENCY: usize = 16;
/// `CREATE_CHANNEL_QUEUE_DEPTH` (64) queued, one per worker in flight, one
/// decoded and waiting for queue space.
const MAX_CONSUMED_FRAMES: usize = 64 + RESOLVE_CONCURRENCY + 1;

/// Every `has_pv` waits until `gate` is open. The server wraps the source in
/// a composite whose `resolve_owner` runs `has_pv` a second time per name,
/// so the gate is a level, not a count of admissions.
struct GatedSource {
    gate: watch::Sender<bool>,
    in_has_pv: AtomicUsize,
    max_in_has_pv: AtomicUsize,
    entered: AtomicUsize,
}

impl GatedSource {
    fn new() -> Self {
        Self {
            gate: watch::Sender::new(false),
            in_has_pv: AtomicUsize::new(0),
            max_in_has_pv: AtomicUsize::new(0),
            entered: AtomicUsize::new(0),
        }
    }
}

impl ChannelSource for GatedSource {
    async fn list_pvs(&self) -> Vec<String> {
        Vec::new()
    }
    async fn has_pv(&self, _: &str) -> bool {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let now = self.in_has_pv.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_has_pv.fetch_max(now, Ordering::SeqCst);
        self.gate
            .subscribe()
            .wait_for(|open| *open)
            .await
            .expect("gate sender outlives the server");
        self.in_has_pv.fetch_sub(1, Ordering::SeqCst);
        true
    }
    async fn get_introspection(&self, _: &str) -> Option<FieldDesc> {
        Some(FieldDesc::Scalar(ScalarType::Double))
    }
    async fn get_value(&self, _: &str) -> Option<PvField> {
        None
    }
    async fn put_value(&self, _: &str, _: PvField) -> Result<(), OpError> {
        Err("read-only".into())
    }
    async fn is_writable(&self, _: &str) -> bool {
        false
    }
    async fn subscribe(&self, _: &str) -> Option<MonitorStream<PvField>> {
        None
    }
}

/// One client-direction CREATE_CHANNEL frame carrying a single
/// `(cid, name)` pair. Fixed-width names keep every frame the same size so
/// the peer report's byte count converts to a frame count exactly.
fn create_channel_frame(cid: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.put_u16(1, ORDER);
    payload.put_u32(cid, ORDER);
    encode_string_into(&format!("GATE:{cid:04}"), ORDER, &mut payload);
    let mut out = Vec::new();
    PvaHeader::application(
        false,
        ORDER,
        Command::CreateChannel.code(),
        payload.len() as u32,
    )
    .write_into(&mut out);
    out.extend_from_slice(&payload);
    out
}

/// Bytes the server's read loop has consumed from `peer`, per its report.
fn bytes_in(server: &PvaServer, peer: std::net::SocketAddr) -> u64 {
    server
        .report()
        .peers
        .iter()
        .find(|(addr, _)| *addr == peer)
        .map(|(_, snap)| snap.bytes_in)
        .unwrap_or(0)
}

/// Read application frames until one carries CREATE_CHANNEL, returning the
/// `cid` from its payload (control frames — the server's SET_BYTE_ORDER and
/// echo heartbeat — are skipped).
async fn next_create_channel_cid(sock: &mut tokio::net::TcpStream) -> u32 {
    loop {
        let mut hdr = [0u8; PvaHeader::SIZE];
        sock.read_exact(&mut hdr).await.expect("read frame header");
        let h = PvaHeader::decode(&mut std::io::Cursor::new(&hdr[..])).expect("decode header");
        if h.flags.is_control() {
            continue;
        }
        let mut body = vec![0u8; h.payload_length as usize];
        sock.read_exact(&mut body).await.expect("read frame body");
        if h.command == Command::CreateChannel.code() {
            let mut cur = std::io::Cursor::new(&body[..]);
            return cur.get_u32(h.flags.byte_order()).expect("reply cid");
        }
    }
}

/// Wait until the gated burst has parked the read loop: a worker is inside
/// `has_pv`, and the bytes consumed from `peer` have held still for a
/// while (with the queue full the loop is parked on `reserve()` and reads
/// nothing more). Returns the settled byte count.
async fn wait_for_read_pause(
    source: &GatedSource,
    server: &PvaServer,
    peer: std::net::SocketAddr,
) -> u64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while source.entered.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the resolver never entered has_pv"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut last = bytes_in(server, peer);
    let mut stable_since = tokio::time::Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let now = bytes_in(server, peer);
        if now != last {
            last = now;
            stable_since = tokio::time::Instant::now();
        } else if stable_since.elapsed() >= Duration::from_millis(400) {
            return last;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "bytes_in never settled while the source was gated"
        );
    }
}

/// Read every CREATE_CHANNEL reply the burst is owed, sorted by cid.
async fn read_burst_replies(sock: &mut tokio::net::TcpStream) -> Vec<u32> {
    let mut replies = tokio::time::timeout(Duration::from_secs(20), async {
        let mut cids = Vec::with_capacity(BURST);
        for _ in 0..BURST {
            cids.push(next_create_channel_cid(sock).await);
        }
        cids
    })
    .await
    .expect("not every CREATE_CHANNEL reply arrived — resolver and read loop wedged");
    replies.sort_unstable();
    replies
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_channel_burst_is_backpressured() {
    let source = Arc::new(GatedSource::new());
    let server = PvaServer::isolated(source.clone()).expect("isolated test server must start");
    let port = server.report().tcp_port;

    let mut sock = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .expect("connect to test server");
    let me = sock.local_addr().expect("local addr");

    // The server accepts CREATE_CHANNEL straight after its own
    // SET_BYTE_ORDER, so no CONNECTION_VALIDATION exchange is needed.
    let frame_len = create_channel_frame(0).len();
    let burst: Vec<u8> = (0..BURST as u32).flat_map(create_channel_frame).collect();
    sock.write_all(&burst).await.expect("write burst");

    let last = wait_for_read_pause(&source, &server, me).await;
    let consumed = last as usize / frame_len;
    assert!(
        consumed <= MAX_CONSUMED_FRAMES,
        "read loop consumed {consumed} CREATE_CHANNEL frames while the source was blocked; \
         pvxs parity bounds this at the request queue ({MAX_CONSUMED_FRAMES}) — a loop that \
         keeps reading spawns unbounded resolution work per connection"
    );
    let in_flight = source.max_in_has_pv.load(Ordering::SeqCst);
    assert!(
        in_flight <= RESOLVE_CONCURRENCY,
        "{in_flight} names were inside has_pv at once; the worker pool bounds this at \
         {RESOLVE_CONCURRENCY}"
    );

    // Release everything. Every queued name resolves, the completion
    // queue fills while the read loop is still waiting for request-queue
    // space, and every reply must still drain.
    source.gate.send_replace(true);
    let replies = read_burst_replies(&mut sock).await;
    let expected: Vec<u32> = (0..BURST as u32).collect();
    assert_eq!(replies, expected, "every cid must be answered exactly once");
    let in_flight = source.max_in_has_pv.load(Ordering::SeqCst);
    assert!(
        in_flight <= RESOLVE_CONCURRENCY,
        "{in_flight} names were inside has_pv at once during the release; bound is \
         {RESOLVE_CONCURRENCY}"
    );
    assert!(
        source.entered.load(Ordering::SeqCst) >= BURST,
        "every name reached has_pv at least once"
    );

    drop(sock);
    drop(server);
}

/// The read-stall watchdog (`op_timeout`) measures peer silence while the
/// loop is reading. A resolver pause is the loop's own choice — the peer's
/// frames sit unread in the socket — so it must not count as silence and
/// tear down a connection whose open channels are healthy. Once reads
/// resume and the peer really is silent, the watchdog runs again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stall_watchdog_skips_a_resolver_pause_and_runs_once_reads_resume() {
    const OP_TIMEOUT: Duration = Duration::from_secs(1);
    let source = Arc::new(GatedSource::new());
    let config = PvaServerConfig {
        op_timeout: OP_TIMEOUT,
        ..PvaServerConfig::isolated()
    };
    let server = PvaServer::start(source.clone(), config).expect("isolated test server must start");
    let port = server.report().tcp_port;

    let mut sock = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .expect("connect to test server");
    let me = sock.local_addr().expect("local addr");
    // Consume the server's SET_BYTE_ORDER so the socket holds nothing the
    // liveness peek below could mistake for a live connection.
    let mut hdr = [0u8; PvaHeader::SIZE];
    sock.read_exact(&mut hdr)
        .await
        .expect("read SET_BYTE_ORDER");
    let h = PvaHeader::decode(&mut std::io::Cursor::new(&hdr[..])).expect("decode header");
    assert!(
        h.flags.is_control(),
        "first frame from the server is a control frame"
    );
    let burst: Vec<u8> = (0..BURST as u32).flat_map(create_channel_frame).collect();
    sock.write_all(&burst).await.expect("write burst");
    wait_for_read_pause(&source, &server, me).await;

    // Paused well past `op_timeout`. A closed connection shows as EOF (or
    // a reset) on a peek; a live one has nothing to read and times out.
    tokio::time::sleep(OP_TIMEOUT * 5 / 2).await;
    let mut probe = [0u8; 1];
    let peeked = tokio::time::timeout(Duration::from_millis(100), sock.peek(&mut probe)).await;
    assert!(
        !matches!(peeked, Ok(Ok(0)) | Ok(Err(_))),
        "the server closed the connection during the resolver pause ({peeked:?}); the stall \
         watchdog counted the pause as peer silence"
    );

    // Release: the loop resumes reading and every reply must still arrive.
    source.gate.send_replace(true);
    let replies = read_burst_replies(&mut sock).await;
    let expected: Vec<u32> = (0..BURST as u32).collect();
    assert_eq!(replies, expected, "every cid must be answered exactly once");

    // The peer now stays silent while the loop is reading: the watchdog
    // must close the connection within `op_timeout` plus one tick.
    let closed = tokio::time::timeout(OP_TIMEOUT * 3, async {
        let mut sink = [0u8; 256];
        loop {
            match sock.read(&mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "the server kept a silent peer past op_timeout once reads had resumed"
    );

    drop(server);
}
