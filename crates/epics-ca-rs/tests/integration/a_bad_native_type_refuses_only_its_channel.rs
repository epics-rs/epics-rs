//! A CREATE_CHAN reply whose native type is no DBF type refuses that channel
//! and no other. C `nciu::connect` refuses it too (`nciu.cpp:115-123`), but
//! libca then drops the whole circuit (`tcpiiu.cpp:588`); here the circuit and
//! its other channels stay up.
//!
//! A proxy in front of a real server rewrites the reply's type to 7, the first
//! code past `DBR_DOUBLE`, for one PV. Every other PV on the same circuit keeps
//! its true type, `DBR_DOUBLE` (6), the last valid one.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::server::records::ai::AiRecord;
use epics_ca_rs::client::{CaClient, ConnectionEvent};
use epics_ca_rs::protocol::{CA_PROTO_CREATE_CHAN, CA_PROTO_SEARCH, CaHeader};
use epics_ca_rs::server::CaServer;
use epics_ca_rs::{CaError, DbFieldType};
use serial_test::serial;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::process::Command;

const GOOD: &str = "REFUSE:GOOD";
const BAD: &str = "REFUSE:BAD";
const LATER: &str = "REFUSE:LATER";
const BAD_TYPE: u16 = 7;
const REFUSAL: &str = "Ignored conn resp with bad native data type 7";

/// The cids the client created `BAD` under, and how many of their replies
/// the proxy has retyped.
#[derive(Default)]
struct Retype {
    cids: Mutex<HashSet<u32>>,
    sent: AtomicUsize,
}

/// Relay one direction of a circuit frame by frame, handing each frame to
/// `edit` first. The PVs are scalars, so no extended header appears.
async fn relay(
    mut from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    retype: Arc<Retype>,
    edit: fn(&mut [u8], &Retype),
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = match from.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        while buf.len() >= CaHeader::SIZE {
            let postsize = u16::from_be_bytes([buf[2], buf[3]]) as usize;
            let total = CaHeader::SIZE + postsize;
            if buf.len() < total {
                break;
            }
            let mut frame: Vec<u8> = buf.drain(..total).collect();
            edit(&mut frame, &retype);
            if to.write_all(&frame).await.is_err() {
                return;
            }
        }
    }
}

fn cmmd_and_cid(frame: &[u8]) -> (u16, u32) {
    (
        u16::from_be_bytes([frame[0], frame[1]]),
        u32::from_be_bytes([frame[8], frame[9], frame[10], frame[11]]),
    )
}

/// Client to server: note the cid of every create of `BAD`.
fn note_bad_creates(frame: &mut [u8], retype: &Retype) {
    let (cmmd, cid) = cmmd_and_cid(frame);
    let name = &frame[CaHeader::SIZE..];
    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
    if cmmd == CA_PROTO_CREATE_CHAN && name == BAD.as_bytes() {
        retype.cids.lock().unwrap().insert(cid);
    }
}

/// Server to client: give the reply to a create of `BAD` the bad type.
fn retype_bad_replies(frame: &mut [u8], retype: &Retype) {
    let (cmmd, cid) = cmmd_and_cid(frame);
    if cmmd == CA_PROTO_CREATE_CHAN && retype.cids.lock().unwrap().contains(&cid) {
        frame[4..6].copy_from_slice(&BAD_TYPE.to_be_bytes());
        retype.sent.fetch_add(1, Ordering::SeqCst);
    }
}

/// Serve the three PVs behind the retyping proxy: the proxy's search port,
/// and its count of retyped replies.
async fn served_behind_retyping_proxy() -> (u16, Arc<Retype>) {
    let server = CaServer::builder()
        .port(0)
        .record(GOOD, AiRecord::new(1.0))
        .record(BAD, AiRecord::new(2.0))
        .record(LATER, AiRecord::new(3.0))
        .build()
        .await
        .expect("build CA server");
    let search: SocketAddr = format!("127.0.0.1:{}", server.udp_port()).parse().unwrap();
    let circuit: SocketAddr = format!("127.0.0.1:{}", server.tcp_port()).parse().unwrap();
    tokio::spawn(async move { server.run().await });

    let udp = UdpSocket::bind(("127.0.0.1", 0))
        .await
        .expect("bind proxy UDP");
    let tcp = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind proxy TCP");
    let search_port = udp.local_addr().unwrap().port();
    let circuit_port = tcp.local_addr().unwrap().port();

    // Relay searches, pointing each SEARCH reply's TCP port (its `data_type`)
    // at the proxy so the circuit comes through it.
    tokio::spawn(async move {
        let upstream = UdpSocket::bind(("127.0.0.1", 0))
            .await
            .expect("bind proxy upstream UDP");
        let mut buf = [0u8; 4096];
        loop {
            let Ok((n, client)) = udp.recv_from(&mut buf).await else {
                return;
            };
            if upstream.send_to(&buf[..n], search).await.is_err() {
                continue;
            }
            let mut reply = [0u8; 4096];
            let Ok(Ok((rn, _))) =
                tokio::time::timeout(Duration::from_secs(2), upstream.recv_from(&mut reply)).await
            else {
                continue;
            };
            let mut off = 0usize;
            while off + CaHeader::SIZE <= rn {
                let cmmd = u16::from_be_bytes([reply[off], reply[off + 1]]);
                let postsize = u16::from_be_bytes([reply[off + 2], reply[off + 3]]) as usize;
                if cmmd == CA_PROTO_SEARCH {
                    reply[off + 4..off + 6].copy_from_slice(&circuit_port.to_be_bytes());
                }
                off += CaHeader::SIZE + postsize;
            }
            let _ = udp.send_to(&reply[..rn], client).await;
        }
    });

    let retype = Arc::new(Retype::default());
    let shared = retype.clone();
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = tcp.accept().await else {
                return;
            };
            let Ok(upstream) = TcpStream::connect(circuit).await else {
                continue;
            };
            let (cr, cw) = client.into_split();
            let (sr, sw) = upstream.into_split();
            tokio::spawn(relay(cr, sw, shared.clone(), note_bad_creates));
            tokio::spawn(relay(sr, cw, shared.clone(), retype_bad_replies));
        }
    });
    (search_port, retype)
}

/// The channel with the bad type never connects, waits out its caller's
/// timeout, and holds a derived subscribe for a later connect; the channels
/// beside it on the circuit connect and read.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_bad_native_type_refuses_its_channel_and_spares_the_circuit() {
    let (search_port, retype) = served_behind_retyping_proxy().await;
    // SAFETY: `#[serial(epics_env)]` keeps every env-touching test apart, and
    // `CaClient::new` snapshots its resolver configuration.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{search_port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", search_port.to_string());
    }
    let client = CaClient::new().await.expect("CA client");

    let good = client.create_channel(GOOD);
    good.wait_connected(fact_budget::FACT_BUDGET)
        .await
        .expect("a DBR_DOUBLE channel connects");
    assert_eq!(
        good.info().await.expect("connected").native_type,
        DbFieldType::Double
    );

    let bad = client.create_channel(BAD);
    let mut events = bad.connection_events();
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while retype.sent.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the server never answered {BAD}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Created after the refused reply went out, on the same circuit: it
    // connects only once the client has handled that reply.
    let later = client.create_channel(LATER);
    later
        .wait_connected(fact_budget::FACT_BUDGET)
        .await
        .expect("the circuit outlives the refusal");

    let subscribed = bad.subscribe().await;
    let wait = Duration::from_millis(500);
    let started = Instant::now();
    let waited = bad.wait_connected(wait).await;
    let took = started.elapsed();
    assert!(
        subscribed.is_ok(),
        "a subscribe on the refused channel is held for its connect: {:?}",
        subscribed.err()
    );
    assert!(
        matches!(waited, Err(CaError::ChannelNotFound(_))),
        "the refused channel ends its caller's wait at the timeout: {waited:?}"
    );
    assert!(took >= wait, "the wait ended after {took:?}");
    assert!(matches!(bad.info().await, Err(CaError::Disconnected)));

    let again = client.create_channel(GOOD);
    again
        .wait_connected(fact_budget::FACT_BUDGET)
        .await
        .expect("the client still connects channels");
    let (_, value) = good
        .get_with_timeout(fact_budget::FACT_BUDGET)
        .await
        .expect("a channel on the circuit still reads");
    assert_eq!(value.to_f64(), Some(1.0));
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, ConnectionEvent::Connected),
            "the refused channel announced Connected"
        );
    }

    // SAFETY: `#[serial(epics_env)]`.
    unsafe {
        std::env::remove_var("EPICS_CA_ADDR_LIST");
        std::env::remove_var("EPICS_CA_AUTO_ADDR_LIST");
        std::env::remove_var("EPICS_CA_SERVER_PORT");
    }
}

/// `caget` on the refused PV prints libca's text with the type once per
/// refused reply, re-searches on the reconnect schedule rather than at once,
/// and times the PV's connect out.
#[tokio::test(flavor = "multi_thread")]
async fn caget_logs_each_refusal_once_and_reports_the_pv_unconnected() {
    let (search_port, retype) = served_behind_retyping_proxy().await;
    let out = Command::new(env!("CARGO_BIN_EXE_caget-rs"))
        .args(["-w", "3", BAD])
        .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{search_port}"))
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", search_port.to_string())
        .output()
        .await
        .expect("run caget-rs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let refused = retype.sent.load(Ordering::SeqCst);
    let logged = stderr.matches(REFUSAL).count();

    assert!(refused >= 1, "the server never answered {BAD}");
    assert_eq!(
        logged, refused,
        "one line per refused reply; stderr: {stderr:?}"
    );
    // The reconnect schedule's tick is at least 2 s, so 3 s hold at most one
    // re-search after the first refusal.
    assert!(refused <= 2, "{refused} refusals in 3 s");
    assert!(
        stderr.contains(&format!("Channel connect timed out: '{BAD}' not found.")),
        "stderr: {stderr:?}"
    );
    assert_eq!(stdout, "");
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr:?}");
}
