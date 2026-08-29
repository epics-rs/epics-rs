//! A MONITOR FINISH frame may carry one last update (R6-35).
//!
//! pvxs `clientmon.cpp:504-511` decodes the final (`subcmd & 0x10`) frame's
//! trailing body whenever bytes remain after the Status —
//! `else if(!final || !M.empty()) { … from_wire_valid(…); from_wire(M, overrun); }`
//! — queues that update, and only then appends the `Finished()` marker
//! (`:701-707`). A subscriber therefore sees the last value followed by the end
//! of stream. `servermon.cpp:176-178` is the server side of the same shape.
//!
//! The Rust client classified every FINISH as status-only and dropped whatever
//! followed the Status, so the update reached neither a typed subscriber nor —
//! through `pva_gateway`'s raw-forwarding path — a downstream one.
//!
//! Neither the Rust server nor pvxs's emits this shape today, so the test
//! drives a scripted PVA server that does.

#![cfg(test)]
// This file drives a live client ↔ scripted-server monitor over `tokio::net`.
// Under `exec_backend` the client's connection tasks route through the
// callback pool, which has no tokio reactor, so the hosted TCP transport
// cannot run — every test here is reactor-dependent in full. The blocking
// transport that makes the client run on the pool for the target
// (`pva_blocking_client`, stage 2) is a separate config the scripted-peer
// fixtures do not drive. Gated out on the exec backend as a whole file.
#![cfg(tokio_backend)]
#![cfg(feature = "client")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::proto::{
    BitSet, ByteOrder, Command, ControlCommand, PvaHeader, Status, WriteExt, encode_size_into,
    encode_string_into,
};
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

const ORDER: ByteOrder = ByteOrder::Little;
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

/// `changed | value | overrun` — the body a MONITOR update carries, and the
/// body this test appends to the FINISH frame.
fn monitor_update_body(value: f64) -> Vec<u8> {
    let mut changed = BitSet::new();
    changed.set(1); // field 1 = `value`
    let mut body = changed.encode(ORDER);

    let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
    s.set("value", PvField::Scalar(ScalarValue::Double(value)));
    epics_pva_rs::pvdata::encode::encode_pv_field_with_bitset(
        &PvField::Structure(s),
        &intro(),
        &changed,
        0,
        ORDER,
        &mut body,
    );

    // A non-empty overrun bitset rides along, exactly as on a DATA frame.
    let mut overrun = BitSet::new();
    overrun.set(1);
    body.extend_from_slice(&overrun.encode(ORDER));
    body
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

/// A scripted PVA server: handshake → CREATE_CHANNEL → MONITOR INIT → and then,
/// on START, a single FINISH frame whose Status is followed by one last update.
/// It also answers a TCP name-server SEARCH on the same port, so a client that
/// resolves the PV by name (rather than by forced address) lands here too.
/// Returns its address.
async fn spawn_finish_with_update_server(value: f64) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind scripted server");
    let addr = listener.local_addr().expect("scripted server addr");

    tokio::spawn(async move {
        // The name-server search and the data circuit are separate connects.
        while let Ok((sock, _)) = listener.accept().await {
            tokio::spawn(serve_conn(sock, addr, value));
        }
    });

    addr
}

async fn serve_conn(mut sock: tokio::net::TcpStream, addr: std::net::SocketAddr, value: f64) {
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

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let Ok(n) = sock.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
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

            // A TCP name-server SEARCH (the client reaches this server as a
            // configured name server) → SEARCH_RESPONSE pointing at this
            // same listener, so the channel is then created on this circuit.
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
                    // START → the whole stream is one FINISH frame that
                    // carries its final update after the Status.
                    let mut p = Vec::new();
                    p.put_u32(ioid, ORDER);
                    p.put_u8(0x10); // FINISH
                    Status::ok().write_into(ORDER, &mut p);
                    p.extend_from_slice(&monitor_update_body(value));
                    if sock
                        .write_all(&app_frame(Command::Monitor.code(), p))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    }
}

/// The typed monitor loop must deliver the FINISH-carried update to the
/// subscriber before ending the stream — pvxs queues the update, then
/// `Finished()`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_monitor_delivers_the_update_carried_on_the_finish_frame() {
    let addr = spawn_finish_with_update_server(2.5).await;
    let client = PvaClient::builder().build();

    let seen: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let handle = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvmonitor_handle_from(
            "FINISH:PV",
            addr,
            move |_desc, value| {
                if let PvField::Structure(s) = value
                    && let Some(PvField::Scalar(ScalarValue::Double(v))) = s.get_field("value")
                {
                    sink.lock().expect("sink").push(*v);
                }
            },
            |_| {},
        ),
    )
    .await
    .expect("monitor setup timed out")
    .expect("monitor must start");

    // The update rides on the FINISH frame, so it lands once and the stream
    // then ends; poll briefly rather than assuming callback timing.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !seen.lock().expect("sink").is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the FINISH frame's trailing update was never delivered — pvxs decodes it \
             (clientmon.cpp:504-511) before pushing Finished()"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        *seen.lock().expect("sink"),
        vec![2.5],
        "exactly the one update carried by the FINISH frame"
    );

    drop(handle);
}

/// The raw-forwarding path (what `pva_gateway` relays through) must hand the
/// same body downstream instead of dropping it with the frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_monitor_forwards_the_body_carried_on_the_finish_frame() {
    let addr = spawn_finish_with_update_server(7.25).await;
    // The raw-frame API resolves the PV by name, so reach the scripted server
    // through the TCP name-server path it also answers.
    let client = PvaClient::builder().name_servers(vec![addr]).build();

    let bodies: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = bodies.clone();
    let handle = tokio::time::timeout(
        Duration::from_secs(5),
        client.pvmonitor_raw_frames_handle(
            "FINISH:PV",
            move |_desc, body, _order| {
                sink.lock().expect("sink").push(body.to_vec());
            },
            |_| {},
        ),
    )
    .await
    .expect("raw monitor setup timed out")
    .expect("raw monitor must start");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !bodies.lock().expect("sink").is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the FINISH frame's trailing body was never forwarded — a gateway would \
             drop the upstream's last update"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let got = bodies.lock().expect("sink").clone();
    assert_eq!(
        got,
        vec![monitor_update_body(7.25)],
        "the relayed body is the FINISH frame's `changed | value | overrun`, verbatim"
    );

    drop(handle);
}
