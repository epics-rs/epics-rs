//! `pvget -w 1e19`: a timeout past the clock's range means "never time out".
//!
//! pvxget hands `-w` to `epicsEvent::wait(double)` (tools/get.cpp:133), and
//! libCom clamps an event wait to ten years (`convertDoubleToWakeTime`,
//! posix/osdTime.cpp:97-100), so the GET simply completes. The client must do
//! the same with an operation budget that overflows `Instant`.

#![cfg(tokio_backend)]
#![cfg(feature = "client")]

use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::task::Reactor;
use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::nt::typed::TypedNT;
use epics_pva_rs::pvalink::PvaLinkResolver;
use epics_pva_rs::server_native::PvaServer;
use epics_pva_rs::server_native::{SharedPV, SharedSource};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_get_with_an_unbounded_timeout_returns_the_value() {
    let pv = SharedPV::new();
    pv.open(f64::descriptor(), f64::to_pv_field(&7.25)).unwrap();
    let source = SharedSource::new();
    source.add("test:unbounded", pv);
    let server = PvaServer::isolated(Arc::new(source)).expect("isolated test server must start");

    let client = PvaClient::builder()
        .server_addr(server.tcp_addr())
        // not-a-wait: the past-the-clock operation budget under test.
        .timeout(Duration::from_secs_f64(1e19))
        .build();

    let got = tokio::time::timeout(fact_budget::FACT_BUDGET, client.pvget("test:unbounded"))
        .await
        .expect("GET with an unbounded timeout did not complete")
        .expect("GET with an unbounded timeout must succeed");
    assert_eq!(f64::from_pv_field(&got).expect("f64 value"), 7.25);

    drop(server);
}

/// pvxs `testqsrvWaitForLinkConnected` (ioc/pvalink.cpp:131-146) waits on the
/// same clamped `epicsEvent::wait`, so a link wait whose timeout is past the
/// clock's range returns once the link connects.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_wait_with_an_unbounded_timeout_sees_the_link_connect() {
    let pv = SharedPV::new();
    pv.open(f64::descriptor(), f64::to_pv_field(&7.25)).unwrap();
    let source = SharedSource::new();
    source.add("test:link", pv);
    let server = PvaServer::isolated(Arc::new(source)).expect("isolated test server must start");
    // SAFETY: nextest runs this test alone in its process. The link
    // registry's client, built by the resolver, finds the server by name
    // server only.
    unsafe {
        std::env::set_var("EPICS_PVA_NAME_SERVERS", server.tcp_addr().to_string());
        std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
    }
    let resolver = PvaLinkResolver::new(Reactor::current().expect("inside the runtime"));

    let connected = tokio::time::timeout(
        fact_budget::FACT_BUDGET,
        resolver.wait_for_link_connected("test:link", Duration::MAX),
    )
    .await
    .expect("the link wait did not see the link connect");
    assert!(connected, "the link wait reported a timeout");

    drop(server);
}
