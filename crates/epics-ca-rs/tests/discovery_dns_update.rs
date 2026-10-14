//! The port a server registers over RFC 2136 is the one a client searches.
//!
//! A client turns the SRV port it discovers into a UDP SEARCH destination
//! (`discovery_mdns` is the same contract over mDNS), so the registered port
//! must be the UDP search port. With `port(0)` the UDP and TCP ports differ,
//! which is where registering the TCP port goes wrong.

#![cfg(all(feature = "discovery-dns-update", tokio_backend))]

use std::sync::Arc;
use std::time::Duration;

use epics_ca_rs::discovery::DnsRegistration;
use epics_ca_rs::server::CaServer;
use hickory_proto::op::Message;
use hickory_proto::rr::RData;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_registered_srv_port_is_the_udp_search_port() {
    let dns = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fake DNS bind");
    let reg = DnsRegistration {
        server: dns.local_addr().unwrap(),
        zone: "test.local.".into(),
        instance: "ca-rs-dns-update-test".into(),
        host: "localhost".into(),
        // Overwritten by the server with the port it actually serves.
        port: 5064,
        ..Default::default()
    };
    let server = CaServer::builder()
        .pv("DNSU:VAL", epics_base_rs::types::EpicsValue::Long(1))
        .port(0)
        .register_dns_update(reg)
        .build()
        .await
        .expect("build");
    let udp_port = server.udp_port();
    let server = Arc::new(server);
    let running = server.clone();
    let task = tokio::spawn(async move {
        let _ = running.run().await;
    });

    let (mut conn, _) = tokio::time::timeout(Duration::from_secs(10), dns.accept())
        .await
        .expect("the server sent no DNS UPDATE")
        .expect("accept");
    let mut srv_ports = Vec::new();
    // Each UPDATE is a 2-byte length, then the message; the SRV append is the
    // first, and a later one carries nothing this test needs.
    let mut len = [0u8; 2];
    tokio::time::timeout(Duration::from_secs(10), conn.read_exact(&mut len))
        .await
        .expect("UPDATE length")
        .expect("read length");
    let mut body = vec![0u8; u16::from_be_bytes(len) as usize];
    conn.read_exact(&mut body).await.expect("read UPDATE");
    let msg = Message::from_vec(&body).expect("decode UPDATE");
    for record in msg.name_servers() {
        if let Some(RData::SRV(srv)) = record.data() {
            srv_ports.push(srv.port());
        }
    }
    task.abort();
    assert_eq!(srv_ports, [udp_port], "SRV port of the first UPDATE");
}
