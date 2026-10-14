//! rsrv binds a second UDP socket on 127.0.0.1 because
//! `osiSockDiscoverBroadcastAddresses` answers the loopback address with
//! itself (`osdNetIfAddrs.c:41-52`, `caservertask.c:680-703`). The two share
//! the address through datagram fanout, and a search sent to it is still
//! answered once: compiled softIoc, three searches, one reply each.
#![cfg(all(tokio_backend, target_os = "linux"))]

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServer;

/// UDP sockets of this process bound to 127.0.0.1:`port`.
fn sockets_on(port: u16) -> usize {
    let want = format!("0100007F:{port:04X}");
    std::fs::read_to_string("/proc/net/udp")
        .expect("read /proc/net/udp")
        .lines()
        .filter(|l| l.split_whitespace().nth(1) == Some(want.as_str()))
        .count()
}

/// CA_PROTO_VERSION followed by a CA_PROTO_SEARCH for `pv`.
fn search_datagram(pv: &str) -> Vec<u8> {
    let mut name = pv.as_bytes().to_vec();
    name.push(0);
    while !name.len().is_multiple_of(8) {
        name.push(0);
    }
    let mut d = Vec::new();
    for w in [0u16, 0, 0, 13] {
        d.extend_from_slice(&w.to_be_bytes());
    }
    d.extend_from_slice(&[0; 8]);
    d.extend_from_slice(&6u16.to_be_bytes());
    d.extend_from_slice(&(name.len() as u16).to_be_bytes());
    d.extend_from_slice(&5u16.to_be_bytes());
    d.extend_from_slice(&13u16.to_be_bytes());
    d.extend_from_slice(&1u32.to_be_bytes());
    d.extend_from_slice(&1u32.to_be_bytes());
    d.extend_from_slice(&name);
    d
}

#[tokio::test]
async fn a_loopback_interface_has_two_responder_sockets_and_one_reply() {
    // SAFETY: nextest runs this test alone in its process.
    unsafe {
        std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1");
    }
    let server = CaServer::builder()
        .port(0)
        .tcp_port(0)
        .pv("LOOP:A", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("build");
    let port = server.udp_port();
    assert_eq!(sockets_on(port), 2, "rsrv holds two sockets on 127.0.0.1");
    let run = tokio::spawn(async move {
        let _ = server.run().await;
    });

    let replies = tokio::task::spawn_blocking(move || {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        s.send_to(&search_datagram("LOOP:A"), ("127.0.0.1", port))
            .unwrap();
        let (mut n, until) = (0, Instant::now() + Duration::from_millis(1500));
        let mut buf = [0u8; 2048];
        while Instant::now() < until {
            if s.recv(&mut buf).is_ok() {
                n += 1;
            }
        }
        n
    })
    .await
    .unwrap();
    run.abort();
    assert_eq!(replies, 1, "a search to 127.0.0.1 is answered exactly once");
}
