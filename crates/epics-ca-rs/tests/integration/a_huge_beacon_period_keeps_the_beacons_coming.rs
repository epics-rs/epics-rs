//! C obeys any finite positive `EPICS_CAS_BEACON_PERIOD`
//! (`online_notify.c:58-68`): the ramp doubles from 20 ms toward it and keeps
//! beaconing. The cases are the two ways a period can outgrow a `Duration`:
//! 1e300 s, which no `Duration` holds, and 1e19 s, which one holds but cannot
//! double.

use std::net::UdpSocket;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_ca_rs::protocol::{CA_PROTO_RSRV_IS_UP, CaHeader};
use epics_ca_rs::server::blocking_serving::BlockingCaServing;
use epics_ca_rs::server::stats::ServerStats;
use fact_budget::FACT_BUDGET;

/// The process's only beacon destination, with `period` as the ramp's limit.
fn beacon_sink(period: &str) -> UdpSocket {
    let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
    sink.set_read_timeout(Some(FACT_BUDGET)).unwrap();
    // SAFETY: nextest runs each test alone in its process, before the
    // memoized `addr_list::from_env` first reads these.
    unsafe {
        std::env::set_var(
            "EPICS_CAS_BEACON_ADDR_LIST",
            sink.local_addr().unwrap().to_string(),
        );
        std::env::set_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1");
        std::env::set_var("EPICS_CAS_BEACON_PERIOD", period);
    }
    sink
}

/// Two beacons for `tcp_port`: the second is sent only after the emitter has
/// computed its next period from the configured one.
fn two_beacons_arrive(sink: &UdpSocket, tcp_port: u16) {
    let mut buf = [0u8; 64];
    for nth in 1..=2 {
        let n = sink
            .recv(&mut buf)
            .unwrap_or_else(|e| panic!("beacon {nth} did not arrive: {e}"));
        let hdr = CaHeader::from_bytes(&buf[..n]).unwrap();
        assert_eq!((hdr.cmmd, hdr.count), (CA_PROTO_RSRV_IS_UP, tcp_port));
    }
}

fn blocking_server_beacons_with_period(period: &str) {
    let sink = beacon_sink(period);
    let serving = BlockingCaServing::start(
        0,
        0,
        Arc::new(PvDatabase::new()),
        epics_base_rs::server::access_security::new_acf_cell(None),
        Arc::new(ServerStats::default()),
    )
    .expect("bind an ephemeral server");
    two_beacons_arrive(&sink, serving.tcp_port());
    serving.shutdown();
    serving
        .join()
        .expect("every serving thread ends on shutdown");
}

#[test]
fn a_period_no_duration_holds_keeps_the_beacons_coming() {
    blocking_server_beacons_with_period("1e300");
}

#[test]
fn a_period_a_duration_holds_but_cannot_double_keeps_the_beacons_coming() {
    blocking_server_beacons_with_period("1e19");
}

#[cfg(tokio_backend)]
#[tokio::test]
async fn the_async_server_keeps_beaconing_with_a_period_no_duration_holds() {
    let sink = beacon_sink("1e300");
    let server = epics_ca_rs::server::CaServer::builder()
        .port(0)
        .pv("BEACON:PV", epics_base_rs::types::EpicsValue::Long(0))
        .build()
        .await
        .expect("build CA server");
    let tcp_port = server.tcp_port();
    let _run = tokio::spawn(async move { server.run().await });
    tokio::task::spawn_blocking(move || two_beacons_arrive(&sink, tcp_port))
        .await
        .unwrap();
}
