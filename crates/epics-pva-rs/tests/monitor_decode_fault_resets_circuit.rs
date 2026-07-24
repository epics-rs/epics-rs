//! An invalid MONITOR frame must reset the whole virtual circuit (R8-34).
//!
//! pvxs `clientmon.cpp:601-605`: when a MONITOR message fails to decode, or
//! violates the subscription state machine, `!M.good()` sends the client into
//!
//! ```text
//! log_crit_printf(io, "%s:%d Server %s sends invalid MONITOR.  Disconnecting...\n", …);
//! bev.reset();
//! ```
//!
//! — the *connection* dies, not just the subscription. It has to: the frame was
//! decoded against the connection's shared `rxRegistry` type cache, so a
//! half-decoded frame can leave that cache mutated, and every other channel on
//! the circuit would keep being served from it.
//!
//! The Rust client already did this for GET/PUT/RPC/PUT_GET/GET_FIELD/PROCESS
//! (`decode_op_or_reset` → `ServerConn::close()`), but both MONITOR loops ended
//! only the subscription (`MonitorEnd::Fatal` + `unregister_ioid`) and left the
//! circuit up. These tests pin the reset for the typed loop (truncated DATA
//! body) and for the raw-forwarding loop the gateway uses (a subcmd no server
//! may send).
//!
//! Observable: the client hangs up. The scripted server watches its socket for
//! EOF after emitting the bad frame — that is `bev.reset()` seen from the peer.

#![cfg(test)]
// This file drives a live client ↔ scripted-server monitor over `tokio::net`.
// Under `rtems-exec-model` the client's connection tasks route through the
// callback pool, which has no tokio reactor, so the hosted TCP transport cannot
// run — every test here is reactor-dependent in full. The blocking transport
// that makes the client run on the pool for the target (`pva_blocking_client`,
// stage 2) is a separate config the scripted-peer fixtures do not drive. Gated
// out feature-ON as a whole file (doc/pvalink-rtems-design.md §4.2, stage 3).
#![cfg(not(feature = "rtems-exec-model"))]

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::proto::{
    Command, ControlCommand, PvaHeader, Status, WriteExt, encode_size_into, encode_string_into,
};
use epics_pva_rs::pvdata::{FieldDesc, ScalarType};

const ORDER: epics_pva_rs::proto::ByteOrder = epics_pva_rs::proto::ByteOrder::Little;
const SID: u32 = 42;

/// The monitored PV's type: `NTScalar<double>` with a single `value` leaf.
fn intro() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
    }
}

fn app_frame(cmd: u8, payload: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    PvaHeader::application(true, ORDER, cmd, payload.len() as u32).write_into(&mut out);
    out.extend_from_slice(&payload);
    out
}

/// Which malformed frame the scripted server emits once the monitor starts.
#[derive(Clone, Copy)]
enum BadFrame {
    /// subcmd 0x00 (DATA) whose body claims `value` is set in the changed
    /// bitset and then stops — the Double's 8 bytes never arrive. The typed
    /// loop's `decode_op_response` fails on it.
    TruncatedData,
    /// subcmd 0x04 — a client->server STOP byte. No server emits it on a
    /// monitor stream (pvxs `servermon.cpp:133-149`), so the raw-forwarding
    /// loop (which never decodes bodies) rejects it on the subcmd alone.
    ServerSentStopSubcmd,
}

impl BadFrame {
    fn payload(self, ioid: u32) -> Vec<u8> {
        let mut p = Vec::new();
        p.put_u32(ioid, ORDER);
        match self {
            BadFrame::TruncatedData => {
                p.put_u8(0x00); // DATA
                p.put_u8(0x01); // changed bitset: 1 byte follows
                p.put_u8(0x02); // bit 1 = `value` is present…
                // …and the 8 bytes of Double never arrive.
            }
            BadFrame::ServerSentStopSubcmd => {
                p.put_u8(0x04);
            }
        }
        p
    }
}

/// SEARCH_RESPONSE for the client's TCP name-server SEARCH: `guid | seq |
/// address | port | protocol | found | count | cid`, pointing the client at
/// this same listener.
fn search_response_payload(search: &[u8], port: u16) -> Vec<u8> {
    let seq = u32::from_le_bytes([search[0], search[1], search[2], search[3]]);
    // seq(4) + flags(1) + reserved(3) + reply address(16) + reply port(2).
    let mut pos = 26;
    let nproto = search[pos] as usize;
    pos += 1;
    for _ in 0..nproto {
        let len = search[pos] as usize;
        pos += 1 + len;
    }
    pos += 2; // channel count
    let cid = u32::from_le_bytes([
        search[pos],
        search[pos + 1],
        search[pos + 2],
        search[pos + 3],
    ]);

    let mut p = Vec::new();
    p.extend_from_slice(&[0xA5u8; 12]); // guid
    p.put_u32(seq, ORDER);
    // IPv4-mapped 127.0.0.1.
    p.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 127, 0, 0, 1]);
    p.put_u16(port, ORDER);
    encode_string_into("tcp", ORDER, &mut p);
    p.put_u8(1); // found
    p.put_u16(1, ORDER); // one cid
    p.put_u32(cid, ORDER);
    p
}

/// A scripted PVA server: handshake → CREATE_CHANNEL → MONITOR INIT → and on
/// START, one malformed MONITOR frame. It then keeps reading, and pulses
/// `hung_up` when the client closes the circuit (read → 0 bytes = the peer's
/// FIN, i.e. pvxs's `bev.reset()`). It also answers a TCP name-server SEARCH on
/// the same port. Returns its address and that notification.
async fn spawn_bad_frame_server(bad: BadFrame) -> (std::net::SocketAddr, Arc<Notify>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind scripted server");
    let addr = listener.local_addr().expect("scripted server addr");
    let hung_up = Arc::new(Notify::new());
    let signal = hung_up.clone();

    tokio::spawn(async move {
        // The name-server search and the data circuit are separate connects.
        while let Ok((sock, _)) = listener.accept().await {
            tokio::spawn(serve_conn(sock, addr, bad, signal.clone()));
        }
    });

    (addr, hung_up)
}

async fn serve_conn(
    mut sock: tokio::net::TcpStream,
    addr: std::net::SocketAddr,
    bad: BadFrame,
    hung_up: Arc<Notify>,
) {
    // SET_BYTE_ORDER (control) + CONNECTION_VALIDATION request.
    let mut hello = Vec::new();
    PvaHeader::control(true, ORDER, ControlCommand::SetByteOrder.code(), 0).write_into(&mut hello);
    let mut p = Vec::new();
    p.put_u32(0x10000, ORDER); // buffer size
    p.put_u16(32_767, ORDER); // registry size
    encode_size_into(1, ORDER, &mut p); // one auth method
    encode_string_into("anonymous", ORDER, &mut p);
    hello.extend_from_slice(&app_frame(Command::ConnectionValidation.code(), p));
    if sock.write_all(&hello).await.is_err() {
        return;
    }

    // Only the circuit that actually carried the bad frame counts as a hang-up;
    // a name-server connection closing proves nothing.
    let mut sent_bad_frame = false;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let Ok(n) = sock.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            // The peer closed. This is `bev.reset()` observed from the server.
            // `notify_one` (not `notify_waiters`) stores a permit, so the test
            // still sees the hang-up if it arrives before the test awaits.
            if sent_bad_frame {
                hung_up.notify_one();
            }
            return;
        }
        buf.extend_from_slice(&chunk[..n]);

        // Split the stream into frames: 8-byte header, then payload_length.
        while buf.len() >= 8 {
            let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
            if buf.len() < 8 + len {
                break;
            }
            let cmd = buf[3];
            let payload = buf[8..8 + len].to_vec();
            buf.drain(..8 + len);

            if cmd == Command::Search.code() {
                let out = app_frame(
                    Command::SearchResponse.code(),
                    search_response_payload(&payload, addr.port()),
                );
                if sock.write_all(&out).await.is_err() {
                    return;
                }
            } else if cmd == Command::ConnectionValidation.code() {
                let out = app_frame(Command::ConnectionValidated.code(), vec![0xFF]);
                if sock.write_all(&out).await.is_err() {
                    return;
                }
            } else if cmd == Command::CreateChannel.code() {
                // count(u16) + cid(u32) + name — reply cid + sid + status.
                let cid = u32::from_le_bytes([payload[2], payload[3], payload[4], payload[5]]);
                let mut p = Vec::new();
                p.put_u32(cid, ORDER);
                p.put_u32(SID, ORDER);
                Status::ok().write_into(ORDER, &mut p);
                if sock
                    .write_all(&app_frame(Command::CreateChannel.code(), p))
                    .await
                    .is_err()
                {
                    return;
                }
            } else if cmd == Command::Monitor.code() {
                // sid(u32) + ioid(u32) + subcmd(u8) + …
                let ioid = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
                let subcmd = payload[8];
                if subcmd & 0x08 != 0 {
                    // INIT → status + type descriptor.
                    let mut p = Vec::new();
                    p.put_u32(ioid, ORDER);
                    p.put_u8(0x08);
                    Status::ok().write_into(ORDER, &mut p);
                    epics_pva_rs::pvdata::encode::encode_type_desc(&intro(), ORDER, &mut p);
                    if sock
                        .write_all(&app_frame(Command::Monitor.code(), p))
                        .await
                        .is_err()
                    {
                        return;
                    }
                } else if subcmd == 0x44 {
                    // START → the one malformed frame this test is about.
                    let out = app_frame(Command::Monitor.code(), bad.payload(ioid));
                    if sock.write_all(&out).await.is_err() {
                        return;
                    }
                    sent_bad_frame = true;
                }
            }
        }
    }
}

/// Typed monitor loop: a DATA frame whose body is truncated fails
/// `decode_op_response`. pvxs treats that as an invalid MONITOR and resets the
/// circuit; before the fix the Rust loop returned `MonitorEnd::Fatal`, ended
/// the subscription, and left the TCP circuit — and its shared reader type
/// cache — serving every other channel on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_monitor_decode_fault_closes_the_circuit() {
    let (addr, hung_up) = spawn_bad_frame_server(BadFrame::TruncatedData).await;
    let client = PvaClient::builder().build();

    let handle = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvmonitor_handle_from("BAD:PV", addr, move |_desc, _value| {}, |_| {}),
    )
    .await
    .expect("monitor setup timed out")
    .expect("monitor must start");

    tokio::time::timeout(Duration::from_secs(5), hung_up.notified())
        .await
        .expect(
            "the client kept the circuit open after an undecodable MONITOR DATA frame — pvxs \
             does bev.reset() (clientmon.cpp:601-605), dropping the connection and every \
             channel on it",
        );

    drop(handle);
}

/// Raw-forwarding loop (the path `pva_gateway` relays through): it never
/// decodes bodies, so its invalid frame is a subcmd no server may send. Same
/// rule — the circuit goes down.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_monitor_invalid_frame_closes_the_circuit() {
    let (addr, hung_up) = spawn_bad_frame_server(BadFrame::ServerSentStopSubcmd).await;
    // The raw-frame API resolves the PV by name, so reach the scripted server
    // through the TCP name-server path it also answers.
    let client = PvaClient::builder().name_servers(vec![addr]).build();

    let handle = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvmonitor_raw_frames_handle("BAD:PV", move |_desc, _body, _order| {}, |_| {}),
    )
    .await
    .expect("raw monitor setup timed out")
    .expect("raw monitor must start");

    tokio::time::timeout(Duration::from_secs(5), hung_up.notified())
        .await
        .expect(
            "the client kept the circuit open after a MONITOR frame carrying a subcmd no \
             server may emit — pvxs faults the buffer and does bev.reset() \
             (clientmon.cpp:601-605)",
        );

    drop(handle);
}
