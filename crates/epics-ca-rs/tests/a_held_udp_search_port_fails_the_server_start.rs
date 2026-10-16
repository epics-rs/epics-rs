//! A search port that cannot be bound makes the CA server's start return an
//! error. C's `rsrv_init` prints and suspends the init thread instead
//! (`caservertask.c:135-141`, `:752`); the difference is a signed-off
//! deviation (docs/differences.md).

#![cfg(tokio_backend)]

use epics_base_rs::server::database::PvDatabase;
use epics_ca_rs::server::CaServer;
use std::sync::Arc;

#[tokio::test]
async fn a_held_udp_search_port_fails_from_parts_instead_of_hanging() {
    // Held for the whole test, with no reuse flag, so the server's bind cannot
    // share it. On the server's own address: Windows lets a `SO_REUSEADDR`
    // bind on a specific address share a port held on the wildcard.
    let held = std::net::UdpSocket::bind("127.0.0.1:0").expect("take a UDP port");
    let port = held.local_addr().unwrap().port();
    // SAFETY: nextest runs this test alone in its process.
    unsafe {
        std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1");
    }
    let started = CaServer::from_parts(
        Arc::new(PvDatabase::new()),
        port,
        Some(0),
        epics_base_rs::server::access_security::new_acf_cell(None),
        None,
        None,
    )
    .await;
    assert!(started.is_err(), "the server started on a held UDP port");
}
