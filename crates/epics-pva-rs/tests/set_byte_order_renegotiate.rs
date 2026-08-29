//! Server-side regression: a peer may re-negotiate the connection byte
//! order mid-stream with another SET_BYTE_ORDER control frame. pvxs latches
//! `sendBE = header[2] & pva_flags::MSB` on every received SetEndian
//! (conn.cpp:169-188) and uses it for all subsequent sends; old pvAccess
//! accepts it from either peer at any time. The Rust server's read loop is
//! the single owner of that latch: on SET_BYTE_ORDER it updates both its
//! local outbound `order` (used by its own synchronous replies and its
//! heartbeat arm) and the shared cell the monitor subscriber tasks read, so
//! the next outbound frame (here, the echo response) adopts the new order.
//!
//! Drives a real server over raw TCP: reads its initial SET_BYTE_ORDER
//! (Little), sends a mid-stream SET_BYTE_ORDER(Big) followed by an
//! EchoRequest, and asserts the server's EchoResponse header order has
//! flipped to Big. The EchoRequest's own header order is intentionally the
//! opposite (Little) so the response order can only come from the latch, not
//! from echoing the request frame's order.
//!
//! Self-contained (no external EPICS/pvxs tools), so it runs in the default
//! nextest profile rather than the gated `interop` suites.

#![cfg(tokio_backend)]

use epics_pva_rs::server_native::MonitorStream;
use std::io::{Cursor, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use epics_pva_rs::proto::{ByteOrder, ControlCommand, PvaHeader};
use epics_pva_rs::pvdata::{FieldDesc, PvField};
use epics_pva_rs::server_native::PvaServer;
use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServerConfig};

/// Minimal source — the control-frame path is independent of channels, so
/// the server needs no real PVs to handle SET_BYTE_ORDER / ECHO.
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

/// Read exactly one PVA frame and return its header. Control frames are
/// header-only (8 bytes); application frames carry a `payload_length` body
/// that must be consumed so the next read lands on a frame boundary.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_relatches_outbound_order_on_mid_stream_set_byte_order() {
    // Server starts little-endian (its handshake SET_BYTE_ORDER order).
    let cfg = PvaServerConfig {
        wire_byte_order: ByteOrder::Little,
        tcp_port: 0,
        udp_port: {
            let l = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        },
        ..PvaServerConfig::isolated()
    };
    let server = PvaServer::start(Arc::new(EmptySource), cfg).expect("server start");
    let port = server.tcp_addr().port();

    let response_order = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream.set_nodelay(true).ok();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        // The server's first frame is its handshake SET_BYTE_ORDER; assert
        // it announces Little (the configured starting order).
        let first = read_frame(&mut stream);
        assert!(
            first.flags.is_control(),
            "first server frame must be control"
        );
        assert_eq!(
            first.command,
            ControlCommand::SetByteOrder.code(),
            "first server frame must be SET_BYTE_ORDER"
        );
        assert_eq!(
            first.flags.byte_order(),
            ByteOrder::Little,
            "initial server outbound order must be Little"
        );

        // Send a mid-stream SET_BYTE_ORDER(Big) then an EchoRequest. The
        // server bit is clear (client → server). EchoRequest's own order is
        // Little so the response order can only come from the latch.
        let mut out = Vec::new();
        PvaHeader::control(
            false,
            ByteOrder::Big,
            ControlCommand::SetByteOrder.code(),
            0,
        )
        .write_into(&mut out);
        PvaHeader::control(
            false,
            ByteOrder::Little,
            ControlCommand::EchoRequest.code(),
            0,
        )
        .write_into(&mut out);
        stream.write_all(&out).expect("write control frames");

        // The server's echo response must adopt the re-latched Big order.
        // Skip any interleaved frames (e.g. CONNECTION_VALIDATION).
        loop {
            let f = read_frame(&mut stream);
            if f.flags.is_control() && f.command == ControlCommand::EchoResponse.code() {
                return f.flags.byte_order();
            }
        }
    })
    .await
    .expect("join blocking client");

    server.stop();

    assert_eq!(
        response_order,
        ByteOrder::Big,
        "server outbound order must re-latch to Big after a mid-stream SET_BYTE_ORDER"
    );
}
