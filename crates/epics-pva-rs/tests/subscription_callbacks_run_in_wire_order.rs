//! Subscription callbacks on one connection run in wire order.
//!
//! pvxs runs every subscription's callbacks on the client loop thread in the
//! order their frames arrive. Each subscription here drains on its own task,
//! and the task woken last often ran first, so B's update reached its
//! callback before A's that the server wrote ahead of it.
//!
//! A scripted server writes A's and B's update in one TCP write per round;
//! no Rust or pvxs server fixes the order of two subscriptions' frames.

#![cfg(test)]
// Live client ↔ scripted-server over `tokio::net`; the exec backend has no
// tokio reactor for it (see `monitor_finish_body.rs`).
#![cfg(tokio_backend)]
#![cfg(feature = "client")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::proto::{
    BitSet, ByteOrder, Command, ControlCommand, PvaHeader, Status, WriteExt, encode_size_into,
    encode_string_into,
};
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

const ORDER: ByteOrder = ByteOrder::Little;
const ROUNDS: u32 = 200;
const NAMES: [&str; 2] = ["ORDER:A", "ORDER:B"];

type Rounds = Arc<Mutex<Option<mpsc::UnboundedReceiver<u32>>>>;
type Log<T> = Arc<Mutex<Vec<(char, T)>>>;

fn intro() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))].into(),
    }
}

fn app_frame(cmd: u8, payload: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    PvaHeader::application(true, ORDER, cmd, payload.len() as u32).write_into(&mut out);
    out.extend_from_slice(&payload);
    out
}

fn update_body(value: f64) -> Vec<u8> {
    let mut changed = BitSet::new();
    changed.set(1);
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
    body.extend_from_slice(&BitSet::new().encode(ORDER));
    body
}

fn value_of(round: u32) -> f64 {
    f64::from(round) + 0.5
}

/// A size-prefixed string at `pos`; returns it and the position after it.
fn read_string(buf: &[u8], pos: usize) -> (String, usize) {
    let len = buf[pos] as usize;
    let s = String::from_utf8_lossy(&buf[pos + 1..pos + 1 + len]).into_owned();
    (s, pos + 1 + len)
}

fn u32_at(buf: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]])
}

/// SEARCH_RESPONSE finding every channel of a TCP name-server SEARCH here.
fn search_response(search: &[u8], port: u16) -> Vec<u8> {
    // seq(4) + flags(1) + reserved(3) + reply address(16) + reply port(2).
    let mut pos = 26;
    let nproto = search[pos] as usize;
    pos += 1;
    for _ in 0..nproto {
        pos = read_string(search, pos).1;
    }
    let count = u16::from_le_bytes([search[pos], search[pos + 1]]);
    pos += 2;
    let mut cids = Vec::new();
    for _ in 0..count {
        cids.push(u32_at(search, pos));
        pos = read_string(search, pos + 4).1;
    }
    let mut p = Vec::new();
    p.extend_from_slice(&[0xA5u8; 12]);
    p.put_u32(u32_at(search, 0), ORDER);
    p.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 127, 0, 0, 1]);
    p.put_u16(port, ORDER);
    encode_string_into("tcp", ORDER, &mut p);
    p.put_u8(1);
    p.put_u16(cids.len() as u16, ORDER);
    for cid in cids {
        p.put_u32(cid, ORDER);
    }
    p
}

/// Serves the two channels. Once both monitors have STARTed, each round
/// received from the test is written as A's update then B's, in one write.
async fn spawn_server(rounds: mpsc::UnboundedReceiver<u32>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind scripted server");
    let addr = listener.local_addr().expect("scripted server addr");
    let rounds: Rounds = Arc::new(Mutex::new(Some(rounds)));
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            tokio::spawn(serve_conn(sock, addr, rounds.clone()));
        }
    });
    addr
}

async fn serve_conn(mut sock: tokio::net::TcpStream, addr: std::net::SocketAddr, rounds: Rounds) {
    let mut hello = Vec::new();
    PvaHeader::control(true, ORDER, ControlCommand::SetByteOrder.code(), 0).write_into(&mut hello);
    let mut p = Vec::new();
    p.put_u32(0x10000, ORDER);
    p.put_u16(32_767, ORDER);
    encode_size_into(1, ORDER, &mut p);
    encode_string_into("anonymous", ORDER, &mut p);
    hello.extend_from_slice(&app_frame(Command::ConnectionValidation.code(), p));
    if sock.write_all(&hello).await.is_err() {
        return;
    }

    // Index into NAMES is the channel's sid.
    let mut ioids: [Option<u32>; 2] = [None, None];
    let mut started = [false, false];
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while started != [true, true] {
        let Ok(n) = sock.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        while buf.len() >= 8 {
            let len = u32_at(&buf, 4) as usize;
            if buf.len() < 8 + len {
                break;
            }
            let cmd = buf[3];
            let payload = buf[8..8 + len].to_vec();
            buf.drain(..8 + len);
            let out = if cmd == Command::Search.code() {
                app_frame(
                    Command::SearchResponse.code(),
                    search_response(&payload, addr.port()),
                )
            } else if cmd == Command::ConnectionValidation.code() {
                app_frame(Command::ConnectionValidated.code(), vec![0xFF])
            } else if cmd == Command::CreateChannel.code() {
                // count(u16) + cid(u32) + name.
                let cid = u32_at(&payload, 2);
                let (name, _) = read_string(&payload, 6);
                let sid = NAMES.iter().position(|n| *n == name).expect("known PV") as u32;
                let mut p = Vec::new();
                p.put_u32(cid, ORDER);
                p.put_u32(sid, ORDER);
                Status::ok().write_into(ORDER, &mut p);
                app_frame(Command::CreateChannel.code(), p)
            } else if cmd == Command::Monitor.code() {
                // sid(u32) + ioid(u32) + subcmd(u8) + …
                let sid = u32_at(&payload, 0) as usize;
                let ioid = u32_at(&payload, 4);
                match payload[8] {
                    0x08 => {
                        ioids[sid] = Some(ioid);
                        let mut p = Vec::new();
                        p.put_u32(ioid, ORDER);
                        p.put_u8(0x08);
                        Status::ok().write_into(ORDER, &mut p);
                        epics_pva_rs::pvdata::encode::encode_type_desc(&intro(), ORDER, &mut p);
                        app_frame(Command::Monitor.code(), p)
                    }
                    0x44 => {
                        started[sid] = true;
                        continue;
                    }
                    _ => continue,
                }
            } else {
                continue;
            };
            if sock.write_all(&out).await.is_err() {
                return;
            }
        }
    }

    let Some(mut rounds) = rounds.lock().unwrap().take() else {
        return;
    };
    while let Some(round) = rounds.recv().await {
        let mut out = Vec::new();
        for ioid in ioids {
            let mut p = Vec::new();
            p.put_u32(ioid.expect("INIT before START"), ORDER);
            p.put_u8(0x00);
            p.extend_from_slice(&update_body(value_of(round)));
            out.extend_from_slice(&app_frame(Command::Monitor.code(), p));
        }
        if sock.write_all(&out).await.is_err() {
            return;
        }
    }
}

/// Drive the rounds; after each, the log must hold A's then B's update.
async fn assert_rounds_in_wire_order<T: PartialEq + std::fmt::Debug>(
    rounds: mpsc::UnboundedSender<u32>,
    log: &Mutex<Vec<(char, T)>>,
    expect: impl Fn(u32) -> T,
) {
    let mut expected = Vec::new();
    for round in 1..=ROUNDS {
        rounds.send(round).expect("server alive");
        expected.extend([('A', expect(round)), ('B', expect(round))]);
        tokio::time::timeout(fact_budget::FACT_BUDGET, async {
            while log.lock().unwrap().len() < expected.len() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("both updates delivered");
        assert_eq!(
            *log.lock().unwrap(),
            expected,
            "round {round}: callbacks ran out of wire order"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_subscription_callbacks_run_in_wire_order() {
    let (tx, rx) = mpsc::unbounded_channel();
    let addr = spawn_server(rx).await;
    let client = PvaClient::builder().name_servers(vec![addr]).build();
    let log: Log<f64> = Arc::default();
    let mut handles = Vec::new();
    for (name, tag) in NAMES.into_iter().zip(['A', 'B']) {
        let log = log.clone();
        let handle = client.pvmonitor_handle(
            name,
            move |_, value| {
                if let PvField::Structure(s) = value
                    && let Some(PvField::Scalar(ScalarValue::Double(v))) = s.get_field("value")
                {
                    log.lock().unwrap().push((tag, *v));
                }
            },
            |_| {},
        );
        handles.push(
            tokio::time::timeout(fact_budget::FACT_BUDGET, handle)
                .await
                .expect("subscribe in time")
                .expect("subscribe"),
        );
    }
    assert_rounds_in_wire_order(tx, &log, value_of).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_subscription_callbacks_run_in_wire_order() {
    let (tx, rx) = mpsc::unbounded_channel();
    let addr = spawn_server(rx).await;
    let client = PvaClient::builder().name_servers(vec![addr]).build();
    let log: Log<Vec<u8>> = Arc::default();
    let mut handles = Vec::new();
    for (name, tag) in NAMES.into_iter().zip(['A', 'B']) {
        let log = log.clone();
        let handle = client.pvmonitor_raw_frames_handle(
            name,
            move |_, body, _| log.lock().unwrap().push((tag, body.to_vec())),
            |_| {},
        );
        handles.push(
            tokio::time::timeout(fact_budget::FACT_BUDGET, handle)
                .await
                .expect("subscribe in time")
                .expect("subscribe"),
        );
    }
    assert_rounds_in_wire_order(tx, &log, |round| update_body(value_of(round))).await;
}
