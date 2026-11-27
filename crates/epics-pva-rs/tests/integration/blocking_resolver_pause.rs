//! A blocking-driver connection outlives a CREATE_CHANNEL resolver pause
//! longer than `op_timeout`.
//!
//! The pause is the read loop's own silence, not the peer's, so only the
//! loop's pause-aware read-stall watchdog may judge it — as on the tokio
//! driver, and in pvxs, whose `tcpTimeout` runs only while the bufferevent
//! reads (`serverconn.cpp:84-85`). A peer request sent during the pause must
//! still be answered once the resolver drains.

use std::io::{Cursor, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::Reactor;
use epics_base_rs::types::RecordName;
use fact_budget::{FACT_BUDGET, barrier};
use tokio::sync::watch;

use epics_pva_rs::proto::{
    ByteOrder, Command, ControlCommand, PvaHeader, WriteExt, encode_string_into,
};
use epics_pva_rs::pvdata::{FieldDesc, PvField, ScalarType};
use epics_pva_rs::server_native::blocking::BlockingPvaServer;
use epics_pva_rs::server_native::{ChannelSource, MonitorStream, OpError, PvaServerConfig};

const ORDER: ByteOrder = ByteOrder::Little;
/// `CREATE_CHANNEL_RESOLVE_CONCURRENCY` (16) names in `has_pv`,
/// `CREATE_CHANNEL_QUEUE_DEPTH` (64) queued, and one decoded and waiting for
/// queue space: the read loop reads nothing after these until the gate opens.
const PAUSING_FRAMES: u32 = 16 + 64 + 1;
/// Well inside the server's 15 s echo heartbeat, whose arrival marks the
/// pause as longer than this.
const OP_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE: &[u8] = b"sent during the pause";

/// Every `has_pv` waits until the gate opens.
struct GatedSource(watch::Sender<bool>);

impl ChannelSource for GatedSource {
    fn list_pvs(&self) -> Vec<RecordName> {
        Vec::new()
    }
    async fn has_pv(&self, _: &[u8]) -> bool {
        let _ = self.0.subscribe().wait_for(|open| *open).await;
        true
    }
    async fn get_introspection(&self, _: &[u8]) -> Option<FieldDesc> {
        Some(FieldDesc::Scalar(ScalarType::Double))
    }
    async fn get_value(&self, _: &[u8]) -> Option<PvField> {
        None
    }
    async fn put_value(&self, _: &[u8], _: PvField) -> Result<(), OpError> {
        Err("read-only".into())
    }
    async fn is_writable(&self, _: &[u8]) -> bool {
        false
    }
    async fn subscribe(&self, _: &[u8]) -> Option<MonitorStream<PvField>> {
        None
    }
}

fn app_frame(command: Command, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    PvaHeader::application(false, ORDER, command.code(), payload.len() as u32).write_into(&mut out);
    out.extend_from_slice(payload);
    out
}

fn create_channel_frame(cid: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.put_u16(1, ORDER);
    payload.put_u32(cid, ORDER);
    encode_string_into(&format!("GATE:{cid:03}"), ORDER, &mut payload);
    app_frame(Command::CreateChannel, &payload)
}

/// What the peer reads from the server.
#[derive(Debug)]
enum Seen {
    Control(u8),
    App(u8, Vec<u8>),
    Closed,
}

fn next_seen(sock: &mut TcpStream, remaining: Duration) -> Option<Seen> {
    sock.set_read_timeout(Some(remaining))
        .expect("read timeout");
    let mut hdr = [0u8; PvaHeader::SIZE];
    if let Err(e) = sock.read_exact(&mut hdr) {
        return closed(e);
    }
    let h = PvaHeader::decode(&mut Cursor::new(&hdr[..])).expect("decode header");
    if h.flags.is_control() {
        return Some(Seen::Control(h.command));
    }
    let mut body = vec![0u8; h.payload_length as usize];
    if let Err(e) = sock.read_exact(&mut body) {
        return closed(e);
    }
    Some(Seen::App(h.command, body))
}

/// A read that timed out saw nothing; any other failure is the server
/// closing the connection.
fn closed(e: std::io::Error) -> Option<Seen> {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => None,
        _ => Some(Seen::Closed),
    }
}

fn bytes_in(server: &BlockingPvaServer, peer: SocketAddr) -> u64 {
    server
        .peers()
        .snapshot()
        .into_iter()
        .find(|(addr, _)| *addr == peer)
        .map_or(0, |(_, snap)| snap.bytes_in)
}

// RTEMS-EXEC-MODEL-ALLOW(1): checked - runs and passes in the exec-backend
// suite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resolver_pause_longer_than_op_timeout_leaves_the_connection_serving() {
    let gate = watch::Sender::new(false);
    let config = PvaServerConfig {
        wire_byte_order: ORDER,
        op_timeout: OP_TIMEOUT,
        // not-a-wait: keeps the writer's idle reaper out of this test.
        idle_timeout: Duration::from_secs(3600),
        ..PvaServerConfig::isolated()
    };
    let server = Arc::new(
        BlockingPvaServer::bind(
            (Ipv4Addr::LOCALHOST, 0),
            Arc::new(GatedSource(gate.clone())),
            config,
        )
        .expect("bind the blocking server"),
    );
    let reactor = Reactor::current().expect("inside the test runtime");
    let (served_tx, served_rx) = std::sync::mpsc::channel();
    let serving = server.clone();
    std::thread::spawn(move || {
        let _ = served_tx.send(serving.serve(&reactor, || true));
    });

    let mut peer = TcpStream::connect(server.local_addr().expect("addr")).expect("connect");
    let me = peer.local_addr().expect("peer addr");
    let burst: Vec<u8> = (0..PAUSING_FRAMES).flat_map(create_channel_frame).collect();
    peer.write_all(&burst).expect("write the pausing burst");
    let deadline = epics_libcom_rs::runtime::time::deadline_from_now(FACT_BUDGET);
    while bytes_in(&server, me) != burst.len() as u64 {
        assert!(Instant::now() < deadline, "the server never read the burst");
        std::thread::sleep(Duration::from_millis(5));
    }

    barrier::until(
        "the paused connection closed before the server's heartbeat",
        |s| matches!(s, Seen::Closed),
        |s| matches!(s, Seen::Control(c) if *c == ControlCommand::EchoRequest.code()),
        |remaining| next_seen(&mut peer, remaining),
    );

    peer.write_all(&app_frame(Command::Echo, PROBE))
        .expect("write the probe");
    gate.send_replace(true);
    barrier::until(
        "the connection closed after its resolver pause instead of answering",
        |s| matches!(s, Seen::Closed),
        |s| matches!(s, Seen::App(c, p) if *c == Command::Echo.code() && p == PROBE),
        |remaining| next_seen(&mut peer, remaining),
    );

    drop(peer);
    server.shutdown();
    served_rx
        .recv_timeout(FACT_BUDGET)
        .expect("serve returned after shutdown")
        .expect("serve");
}
