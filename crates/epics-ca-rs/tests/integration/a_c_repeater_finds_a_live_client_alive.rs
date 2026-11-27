//! C `caRepeater` reaps a registered client when it can bind `INADDR_ANY` at
//! the client's port (`repeaterClient::verify`, `repeater.cpp:285-311`), and
//! runs that probe over every client whenever another one registers
//! (`verifyClients`, `repeater.cpp:317-335`). A live client it reaps gets no
//! more beacons, so a server that starts later is found only on the slow
//! search schedule.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket as StdUdpSocket};

use epics_ca_rs::client::CaClient;
use epics_ca_rs::protocol::{CA_PROTO_REPEATER_CONFIRM, CA_PROTO_REPEATER_REGISTER, CaHeader};
use fact_budget::FACT_BUDGET;
use tokio::net::UdpSocket;

/// The first source port that sends `REPEATER_REGISTER` twice: the client
/// re-sends every second until confirmed (C `repeaterSubscribeTimer`), a
/// one-shot "is a repeater running?" probe sends once.
async fn registered_client(repeater: &UdpSocket) -> SocketAddr {
    let mut ports = Vec::new();
    let mut buf = [0u8; 64];
    tokio::time::timeout(FACT_BUDGET, async {
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

#[tokio::test]
async fn c_repeaters_liveness_probe_collides_with_a_live_client() {
    let repeater = UdpSocket::bind("0.0.0.0:0").await.expect("bind repeater");
    // SAFETY: nextest runs each test alone in its process, and the client
    // resolves the port inside `CaClient::new` below.
    unsafe {
        std::env::set_var(
            "EPICS_CA_REPEATER_PORT",
            repeater.local_addr().unwrap().port().to_string(),
        );
    }
    let _client = CaClient::new().await.expect("client");

    let client = registered_client(&repeater).await;
    repeater
        .send_to(&CaHeader::new(CA_PROTO_REPEATER_CONFIRM).to_bytes(), client)
        .await
        .expect("send CONFIRM");

    // A host with many interface addresses: Linux checks a wildcard bind
    // against per-address sockets only while at most ten share the port.
    let _crowd: Vec<StdUdpSocket> = (2..=12)
        .filter_map(|last| {
            StdUdpSocket::bind(SocketAddrV4::new(
                Ipv4Addr::new(127, 0, 0, last),
                client.port(),
            ))
            .ok()
        })
        .collect();

    let probe = StdUdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, client.port()));
    assert!(
        matches!(&probe, Err(e) if e.kind() == io::ErrorKind::AddrInUse),
        "C caRepeater's probe bound INADDR_ANY:{} beside a live client ({probe:?}), \
         so it would reap the client and stop forwarding it beacons",
        client.port()
    );
}
