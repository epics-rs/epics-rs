//! Reactor-dependent in full: its mock source drives the monitor stream from a
//! bare `tokio::spawn` that sleeps between posts, and under `exec_backend` the
//! `runtime::task` seam drives that future on a `cbMedium` executor worker
//! with no tokio reactor, so the fixture panics with "there is no reactor
//! running". Gated at file scope because every test here shares that source.
#![cfg(tokio_backend)]

//! Server-side regression: a mid-stream SET_BYTE_ORDER must re-latch the
//! outbound order for an ALREADY-RUNNING monitor task, not only for the
//! connection's synchronous replies and heartbeat.
//!
//! pvxs latches `sendBE` on every received SetEndian (conn.cpp:169-188) and
//! `servermon.cpp:159,174` reads `conn->sendBE` at monitor send time, so an
//! open monitor follows a renegotiated order. Before the fix the Rust monitor
//! task captured `order` by value at spawn, so its DATA / FINISH / error
//! frames kept the INIT-time order forever even after the peer renegotiated.
//!
//! Drives a real server over raw TCP: opens a monitor on a self-streaming
//! source, reads a DATA frame (Little), sends a mid-stream SET_BYTE_ORDER(Big)
//! confirmed by an EchoResponse(Big), then asserts a subsequent monitor DATA
//! frame header has flipped to Big. Pre-fix, every monitor frame stays Little.
//!
//! Self-contained (no external EPICS/pvxs tools), so it runs in the default
//! nextest profile rather than the gated `interop` suites.

use epics_pva_rs::server_native::MonitorStream;
use std::io::{Cursor, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use epics_pva_rs::codec::PvaCodec;
use epics_pva_rs::proto::{ByteOrder, Command, ControlCommand, PvaHeader};
use epics_pva_rs::pv_request::build_pv_request_value_only;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
use epics_pva_rs::server_native::{ChannelSource, OpError, PvaServer, PvaServerConfig};

fn nt_scalar(v: f64) -> PvField {
    let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
    s.fields
        .push(("value".into(), PvField::Scalar(ScalarValue::Double(v))));
    PvField::Structure(s)
}

/// A source whose `subscribe` returns a receiver fed by a background task that
/// pushes an incrementing NTScalar value every 40 ms — a self-driving monitor
/// stream, so the test never has to coordinate a push from the blocking
/// client thread.
#[derive(Clone)]
struct StreamingSource;

impl ChannelSource for StreamingSource {
    async fn list_pvs(&self) -> Vec<String> {
        vec!["dut".into()]
    }
    async fn has_pv(&self, n: &str) -> bool {
        n == "dut"
    }
    async fn get_introspection(&self, _: &str) -> Option<FieldDesc> {
        Some(FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        })
    }
    async fn get_value(&self, _: &str) -> Option<PvField> {
        Some(nt_scalar(0.0))
    }
    async fn put_value(&self, _: &str, _: PvField) -> Result<(), OpError> {
        Err("read-only".into())
    }
    async fn is_writable(&self, _: &str) -> bool {
        false
    }
    async fn subscribe(&self, _: &str) -> Option<MonitorStream<PvField>> {
        let (tx, rx) = mpsc::channel::<PvField>(8);
        tokio::spawn(async move {
            let mut i = 1.0f64;
            while tx.send(nt_scalar(i)).await.is_ok() {
                i += 1.0;
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });
        Some(rx.into())
    }
}

/// Read exactly one PVA frame and return `(header, body)`. Control frames are
/// header-only (empty body); application frames carry a `payload_length` body
/// that is consumed so the next read lands on a frame boundary.
fn read_frame(stream: &mut TcpStream) -> (PvaHeader, Vec<u8>) {
    let mut hdr_buf = [0u8; PvaHeader::SIZE];
    stream.read_exact(&mut hdr_buf).expect("read frame header");
    let hdr = PvaHeader::decode(&mut Cursor::new(&hdr_buf[..])).expect("decode frame header");
    let body = if hdr.flags.is_control() {
        Vec::new()
    } else {
        let mut b = vec![0u8; hdr.payload_length as usize];
        stream.read_exact(&mut b).expect("read frame body");
        b
    };
    (hdr, body)
}

/// Subcmd byte of a server→client MONITOR reply: body is `ioid(u32) + subcmd`,
/// so the subcmd is the 5th body byte. `0x00` = DATA, `0x08` = INIT reply,
/// `0x10` = FINISH/error.
fn monitor_subcmd(body: &[u8]) -> Option<u8> {
    body.get(4).copied()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_relatches_outbound_order_on_mid_stream_set_byte_order() {
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
    let server = PvaServer::start(Arc::new(StreamingSource), cfg).expect("server start");
    let port = server.tcp_addr().port();

    let saw_big_data = tokio::task::spawn_blocking(move || {
        let codec = PvaCodec::new(); // little-endian client
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream.set_nodelay(true).ok();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        // CREATE_CHANNEL("dut"). The server accepts it right after its
        // SET_BYTE_ORDER without a CONNECTION_VALIDATION reply.
        stream
            .write_all(&codec.build_create_channel(1, "dut"))
            .expect("send CREATE_CHANNEL");

        // Wait for the CREATE_CHANNEL response and pull the SID from it
        // (body = cid(u32) + sid(u32) + status), decoded in the reply order.
        let sid = loop {
            let (hdr, body) = read_frame(&mut stream);
            if hdr.command == Command::CreateChannel.code() && !hdr.flags.is_control() {
                let order = hdr.flags.byte_order();
                let sid_bytes: [u8; 4] = body[4..8].try_into().unwrap();
                break match order {
                    ByteOrder::Big => u32::from_be_bytes(sid_bytes),
                    ByteOrder::Little => u32::from_le_bytes(sid_bytes),
                };
            }
        };

        // MONITOR INIT (value-only pvRequest), then MONITOR START.
        stream
            .write_all(&codec.build_monitor_init(sid, 1, &build_pv_request_value_only(false), None))
            .expect("send MONITOR INIT");
        loop {
            let (hdr, body) = read_frame(&mut stream);
            if hdr.command == Command::Monitor.code() && monitor_subcmd(&body) == Some(0x08) {
                break; // INIT reply received
            }
        }
        stream
            .write_all(&codec.build_monitor_start(sid, 1))
            .expect("send MONITOR START");

        // Read one DATA frame — it must arrive in the INIT-time order (Little).
        loop {
            let (hdr, body) = read_frame(&mut stream);
            if hdr.command == Command::Monitor.code() && monitor_subcmd(&body) == Some(0x00) {
                assert_eq!(
                    hdr.flags.byte_order(),
                    ByteOrder::Little,
                    "pre-renegotiation monitor DATA must use the INIT-time order"
                );
                break;
            }
        }

        // Renegotiate: SET_BYTE_ORDER(Big) (server bit clear = client→server)
        // followed by an EchoRequest. The EchoResponse(Big) confirms the read
        // loop has latched the new outbound order before we judge DATA frames.
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
        stream.write_all(&out).expect("send SET_BYTE_ORDER + ECHO");

        // Drain until the EchoResponse confirms the latch flipped to Big.
        loop {
            let (hdr, _body) = read_frame(&mut stream);
            if hdr.flags.is_control() && hdr.command == ControlCommand::EchoResponse.code() {
                assert_eq!(
                    hdr.flags.byte_order(),
                    ByteOrder::Big,
                    "EchoResponse must adopt the re-latched Big order"
                );
                break;
            }
        }

        // The latch is now Big. Every monitor DATA frame enqueued after the
        // EchoResponse (FIFO on the connection's single writer) was built after
        // the latch, so a Big DATA frame must appear. Pre-fix the monitor task
        // is frozen at the INIT-time Little order and no Big DATA ever arrives.
        for _ in 0..10 {
            let (hdr, body) = read_frame(&mut stream);
            if hdr.command == Command::Monitor.code()
                && monitor_subcmd(&body) == Some(0x00)
                && hdr.flags.byte_order() == ByteOrder::Big
            {
                return true;
            }
        }
        false
    })
    .await
    .expect("join blocking client");

    server.stop();

    assert!(
        saw_big_data,
        "an already-running monitor must re-latch to Big after a mid-stream \
         SET_BYTE_ORDER; pre-fix it stays frozen at the INIT-time order"
    );
}
