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
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::server::records::ai::AiRecord;
use epics_ca_rs::client::{CaClient, ConnectionEvent};
use epics_ca_rs::protocol::{
    CA_PROTO_CREATE_CHAN, CA_PROTO_REPEATER_CONFIRM, CA_PROTO_REPEATER_REGISTER,
    CA_PROTO_RSRV_IS_UP, CA_PROTO_SEARCH, CA_PROTO_SERVER_DISCONN, CaHeader,
};
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

/// The cids the client created `BAD` under, how many of their replies the
/// proxy has retyped, and when it retyped each. With `disconnect`, the proxy
/// follows each retyped reply with a `CA_PROTO_SERVER_DISCONN` for its cid.
#[derive(Default)]
struct Retype {
    cids: Mutex<HashSet<u32>>,
    sent: AtomicUsize,
    at: Mutex<Vec<Instant>>,
    disconnect: bool,
}

/// Relay one direction of a circuit frame by frame, handing each frame to
/// `edit` first, which may append frames to it. The PVs are scalars, so no
/// extended header appears.
async fn relay(
    mut from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    retype: Arc<Retype>,
    edit: fn(&mut Vec<u8>, &Retype),
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
fn note_bad_creates(frame: &[u8], retype: &Retype) {
    let (cmmd, cid) = cmmd_and_cid(frame);
    let name = &frame[CaHeader::SIZE..];
    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
    if cmmd == CA_PROTO_CREATE_CHAN && name == BAD.as_bytes() {
        retype.cids.lock().unwrap().insert(cid);
    }
}

/// Server to client: give the reply to a create of `BAD` the bad type.
fn retype_bad_replies(frame: &mut Vec<u8>, retype: &Retype) {
    let (cmmd, cid) = cmmd_and_cid(frame);
    if cmmd == CA_PROTO_CREATE_CHAN && retype.cids.lock().unwrap().contains(&cid) {
        frame[4..6].copy_from_slice(&BAD_TYPE.to_be_bytes());
        retype.at.lock().unwrap().push(Instant::now());
        retype.sent.fetch_add(1, Ordering::SeqCst);
        if retype.disconnect {
            let mut disconn = CaHeader::new(CA_PROTO_SERVER_DISCONN);
            disconn.cid = cid;
            frame.extend_from_slice(&disconn.to_bytes());
        }
    }
}

/// Serve the three PVs behind a proxy that retypes as `retype` says: the
/// proxy's search port, and its record of retyped replies.
async fn served_behind_retyping_proxy(retype: Retype) -> (u16, Arc<Retype>) {
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

    let retype = Arc::new(retype);
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
            tokio::spawn(relay(cr, sw, shared.clone(), |f, r| note_bad_creates(f, r)));
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
    let (search_port, retype) = served_behind_retyping_proxy(Retype::default()).await;
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
    let deadline = epics_libcom_rs::runtime::time::deadline_from_now(fact_budget::FACT_BUDGET);
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
/// refused reply, holds the re-search for the disconnect governor rather than
/// searching at once, and times the PV's connect out.
#[tokio::test(flavor = "multi_thread")]
async fn caget_logs_each_refusal_once_and_reports_the_pv_unconnected() {
    let (search_port, retype) = served_behind_retyping_proxy(Retype::default()).await;
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
    // The governor's first tick comes one period after the client starts, so
    // 3 s hold no re-search after the first refusal.
    assert_eq!(refused, 1, "{refused} refusals in 3 s");
    assert!(
        stderr.contains(&format!("Channel connect timed out: '{BAD}' not found.")),
        "stderr: {stderr:?}"
    );
    assert_eq!(stdout, "");
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr:?}");
}

/// C `disconnectGovernorPeriod` (`disconnectGovernorTimer.cpp:27`): a refused
/// channel searches again on the next tick of a timer with this period.
const GOVERNOR: Duration = Duration::from_secs(10);

/// What a search, its reply and the create after it may add to a refusal's
/// time on a loaded host.
const SLACK: Duration = Duration::from_secs(1);

/// The refusals came on the governor's ticks: none later than one period
/// after the one before it, and none sooner once the first refusal had put
/// the channel on the governor's phase.
fn assert_on_governor_ticks(at: &[Instant]) {
    for (n, pair) in (2..).zip(at.windows(2)) {
        let gap = pair[1].duration_since(pair[0]);
        assert!(
            gap <= GOVERNOR + SLACK,
            "refusal {n} came {gap:?} after the one before it, over one governor period"
        );
        if n > 2 {
            assert!(
                gap + SLACK >= GOVERNOR,
                "refusal {n} came {gap:?} after the one before it, under one governor period"
            );
        }
    }
}

/// When the proxy retyped its `n`th reply, once it has.
async fn nth_refusal(retype: &Retype, n: usize) -> Instant {
    let deadline = epics_libcom_rs::runtime::time::deadline_from_now(fact_budget::FACT_BUDGET);
    loop {
        let at = retype.at.lock().unwrap().get(n - 1).copied();
        if let Some(at) = at {
            return at;
        }
        assert!(Instant::now() < deadline, "refusal {n} never came");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The client's beacon socket: the first source port that sends
/// `REPEATER_REGISTER` twice, as the client re-sends it until confirmed; a
/// one-shot "is a repeater running?" probe sends once.
async fn registered_client(repeater: &UdpSocket) -> SocketAddr {
    let mut ports = Vec::new();
    let mut buf = [0u8; 64];
    tokio::time::timeout(fact_budget::FACT_BUDGET, async {
        loop {
            let (n, src) = repeater.recv_from(&mut buf).await.expect("repeater recv");
            match CaHeader::from_bytes(&buf[..n]) {
                Ok(h) if h.cmmd == CA_PROTO_REPEATER_REGISTER => {}
                _ => continue,
            }
            if ports.contains(&src.port()) {
                return src;
            }
            ports.push(src.port());
        }
    })
    .await
    .expect("no port re-sent REPEATER_REGISTER")
}

/// When the proxy refused each of the first `n` creates of `BAD`, at the
/// shortest normal search tick. With `beacons`, beacons from ever-new servers
/// hold the client on its fast search tick from the first refusal on: every
/// beacon wakes its searching channels.
#[cfg(feature = "client")]
async fn refusals(retype: Retype, n: usize, beacons: bool) -> Vec<Instant> {
    let (search_port, retype) = served_behind_retyping_proxy(retype).await;
    let repeater = UdpSocket::bind("0.0.0.0:0").await.expect("bind repeater");
    // SAFETY: `#[serial(epics_env)]` keeps every env-touching test apart, and
    // `CaClient::new` snapshots its resolver configuration.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{search_port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", search_port.to_string());
        std::env::set_var(
            "EPICS_CA_REPEATER_PORT",
            repeater.local_addr().unwrap().port().to_string(),
        );
        std::env::set_var("EPICS_CA_MAX_SEARCH_PERIOD", "60");
    }
    let client = CaClient::new().await.expect("CA client");
    let beacon_socket = registered_client(&repeater).await;
    repeater
        .send_to(
            &CaHeader::new(CA_PROTO_REPEATER_CONFIRM).to_bytes(),
            beacon_socket,
        )
        .await
        .expect("send CONFIRM");

    let _bad = client.create_channel(BAD);
    let mut at = vec![nth_refusal(&retype, 1).await];
    let burst = beacons.then(|| {
        tokio::spawn(async move {
            for port in 1..=u16::MAX {
                let mut beacon = CaHeader::new(CA_PROTO_RSRV_IS_UP);
                beacon.count = port;
                beacon.cid = 1;
                beacon.available = u32::from(Ipv4Addr::LOCALHOST);
                let _ = repeater.send_to(&beacon.to_bytes(), beacon_socket).await;
                // not-a-wait: pacing between beacons, each a server never seen
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
    });
    for i in 2..=n {
        at.push(nth_refusal(&retype, i).await);
    }
    if let Some(burst) = burst {
        burst.abort();
    }

    // SAFETY: `#[serial(epics_env)]`.
    unsafe {
        std::env::remove_var("EPICS_CA_ADDR_LIST");
        std::env::remove_var("EPICS_CA_AUTO_ADDR_LIST");
        std::env::remove_var("EPICS_CA_SERVER_PORT");
        std::env::remove_var("EPICS_CA_REPEATER_PORT");
        std::env::remove_var("EPICS_CA_MAX_SEARCH_PERIOD");
    }
    at
}

/// A refused channel searches again on the disconnect governor's next tick,
/// not on the normal search tick.
#[cfg(feature = "client")]
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_refused_channel_searches_again_on_the_governors_tick() {
    assert_on_governor_ticks(&refusals(Retype::default(), 3, false).await);
}

/// Beacons waking a refused channel do not bring its search before the
/// governor's tick.
#[cfg(feature = "client")]
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_beacon_burst_does_not_hasten_a_refused_channels_search() {
    assert_on_governor_ticks(&refusals(Retype::default(), 3, true).await);
}

/// A server that retires the refused cid at once sends
/// `CA_PROTO_SERVER_DISCONN`, which re-queues the channel as a reconnect;
/// that does not bring its next search forward either.
#[cfg(feature = "client")]
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_server_disconnect_does_not_hasten_a_refused_channels_search() {
    let retype = Retype {
        disconnect: true,
        ..Retype::default()
    };
    assert_on_governor_ticks(&refusals(retype, 3, true).await);
}
