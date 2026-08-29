//! Server-side echo heartbeat, after it was folded from a per-connection
//! task into a deadline arm of the connection's read-loop `select!`
//! (RTEMS phase 6 item 6: fewer tasks per connection, fewer things the
//! blocking backend has to migrate).
//!
//! The fold's risk is that the beat now depends on the read loop reaching
//! its `select!`, and that the task's `AbortOnDrop` teardown guard is gone.
//! Two boundaries, one test each:
//!
//! * **idle** — a connection with no traffic at all must still receive the
//!   server's ECHO_REQUEST. This is the case the fold could silently break
//!   (no frames arrive, so nothing else wakes the loop) and the case no
//!   existing test covered: `stability.rs` only asserts an idle connection
//!   stays *alive*, which passes with no heartbeat at all.
//! * **shutdown** — with the heartbeat's abort guard removed, a client
//!   disconnect must still retire the connection. If the folded arm kept
//!   the loop alive, the peer would linger.
//!
//! Self-contained (no external EPICS/pvxs tools), so it runs in the default
//! nextest profile rather than the gated `interop` suites.

#![cfg(tokio_backend)]

use std::io::{Cursor, Read};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::proto::{ByteOrder, ControlCommand, PvaHeader};
use epics_pva_rs::pvdata::{FieldDesc, PvField};
use epics_pva_rs::server_native::MonitorStream;
use epics_pva_rs::server_native::PvaServer;
use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServerConfig};

/// The server's beat period (`tcp.rs`, `interval(Duration::from_secs(15))`),
/// matching pvxs. Not configurable, so the idle test has to wait it out.
const BEAT: Duration = Duration::from_secs(15);

/// Minimal source — the control-frame path is independent of channels, so
/// the server needs no real PVs to emit a heartbeat.
#[derive(Clone)]
struct EmptySource;

impl ChannelSource for EmptySource {
    async fn list_pvs(&self) -> Vec<String> {
        Vec::new()
    }
    async fn has_pv(&self, _: &str) -> bool {
        false
    }
    async fn get_introspection(&self, _: &str) -> Option<FieldDesc> {
        None
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

fn isolated_cfg() -> PvaServerConfig {
    PvaServerConfig {
        wire_byte_order: ByteOrder::Little,
        tcp_port: 0,
        udp_port: {
            let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        },
        ..PvaServerConfig::isolated()
    }
}

/// Read one PVA frame, consuming any application-frame body so the next
/// read lands on a frame boundary.
fn read_frame(stream: &mut TcpStream) -> PvaHeader {
    let mut hdr_buf = [0u8; PvaHeader::SIZE];
    stream.read_exact(&mut hdr_buf).expect("read frame header");
    let hdr = PvaHeader::decode(&mut Cursor::new(&hdr_buf[..])).expect("decode frame header");
    if !hdr.flags.is_control() {
        let mut body = vec![0u8; hdr.payload_length as usize];
        stream.read_exact(&mut body).expect("read frame body");
    }
    hdr
}

/// The connection sends nothing after connecting — no CONNECTION_VALIDATION
/// reply, no channels, no ops — so the read loop is parked in its `select!`
/// with only the heartbeat's deadline arm able to fire. One ECHO_REQUEST
/// must arrive one beat in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_heartbeat_fires_on_a_connection_that_sends_nothing() {
    let server = PvaServer::start(Arc::new(EmptySource), isolated_cfg()).expect("server start");
    let port = server.tcp_addr().port();

    let waited = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream.set_nodelay(true).ok();
        // One beat plus slack; a miss fails as a read timeout, not a hang.
        stream
            .set_read_timeout(Some(BEAT + Duration::from_secs(10)))
            .unwrap();

        let start = std::time::Instant::now();
        loop {
            let f = read_frame(&mut stream);
            if f.flags.is_control() && f.command == ControlCommand::EchoRequest.code() {
                return start.elapsed();
            }
        }
    })
    .await
    .expect("join blocking client");

    // The handshake frames (SET_BYTE_ORDER, CONNECTION_VALIDATION) arrive
    // immediately; only the heartbeat is on a timer, so anything that
    // arrived promptly cannot have been mistaken for one.
    assert!(
        waited >= BEAT - Duration::from_secs(2),
        "the ECHO_REQUEST must be the timed beat, not a handshake frame — \
         got one after {waited:?}"
    );
}

/// The heartbeat used to be aborted by an `AbortOnDrop` guard when the read
/// loop returned. Folded in, there is no guard to drop: the arm ends with
/// the loop. Prove the loop still ends — a client disconnect must retire
/// the peer rather than leave the folded arm beating into a dead socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_disconnect_retires_the_peer_with_the_heartbeat_folded_in() {
    let server = PvaServer::start(Arc::new(EmptySource), isolated_cfg()).expect("server start");
    let port = server.tcp_addr().port();

    {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream.set_nodelay(true).ok();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        // Read the handshake so the connection is fully established before
        // the drop — otherwise a retired peer proves nothing.
        let first = read_frame(&mut stream);
        assert_eq!(
            first.command,
            ControlCommand::SetByteOrder.code(),
            "first server frame must be SET_BYTE_ORDER"
        );
        for _ in 0..50 {
            if server.report().peer_count > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            server.report().peer_count,
            1,
            "the connection must be registered before we test its teardown"
        );
    } // socket dropped -> EOF at the server

    // Well under one beat: teardown must come from the read loop seeing EOF,
    // not from the heartbeat's idle watchdog.
    for _ in 0..100 {
        if server.report().peer_count == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "peer still registered 2s after disconnect: {}",
        server.report().peer_count
    );
}
