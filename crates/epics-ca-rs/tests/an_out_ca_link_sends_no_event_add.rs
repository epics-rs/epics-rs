//! C monitors a CA link only as a native input: `dbInitLink` sets
//! `pvlOptInpNative` for a `DBF_INLINK` (`dbLink.c:125-126`), whose connect
//! adds the monitor (`dbCa.c:865-867`); any other link is monitored from its
//! first `dbCaGetLink` (`dbCa.c:455-457`). So an OUT link to a remote PV
//! puts no `CA_PROTO_EVENT_ADD` on the wire, and its put is admitted once
//! the channel connects.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use std::sync::Arc;
use std::time::Instant;

use epics_base_rs::server::database::{LinkRef, LinkSet, PutAdmission};
use epics_base_rs::server::record::{LinkFieldType, LinkProcessPolicy, OpenLatch};
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::calink::CaLinkResolver;
use epics_ca_rs::client::{CaClient, CaClientConfig};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

const IN_PV: &str = "RMT:IN";
const OUT_PV: &str = "RMT:OUT";

#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn an_out_link_is_monitored_only_from_its_first_read() {
    let server = CaServer::builder()
        .port(0)
        .pv(IN_PV, EpicsValue::Double(1.0))
        .pv(OUT_PV, EpicsValue::Double(1.0))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let served = server.database().clone();
    tokio::spawn(async move { server.run().await });
    // SAFETY: every test touching the process-wide CA env is
    // `#[serial(epics_env)]`.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let client = CaClient::new_with_config(CaClientConfig::default())
        .await
        .expect("CA client");
    let resolver = CaLinkResolver::with_client(Arc::new(client));
    let in_open = OpenLatch::ca(LinkProcessPolicy::NoProcess, LinkFieldType::In);
    let out_open = OpenLatch::ca(LinkProcessPolicy::NoProcess, LinkFieldType::Out);
    let in_link = LinkRef::new(IN_PV.as_bytes(), &in_open);
    let out_link = LinkRef::new(OUT_PV.as_bytes(), &out_open);
    LinkSet::connect_link(&resolver, in_link, None).await;
    LinkSet::connect_link(&resolver, out_link, None).await;
    for pv in [IN_PV, OUT_PV] {
        assert!(
            resolver
                .wait_for_link_connected(pv, fact_budget::FACT_BUDGET)
                .await,
            "{pv} never connected"
        );
    }
    let monitors = |pv: &'static str| {
        let served = served.clone();
        async move { served.find_pv(pv).unwrap().subscriber_count() }
    };
    assert_eq!(
        monitors(IN_PV).await,
        1,
        "an INLINK monitors from its connect"
    );
    assert_eq!(monitors(OUT_PV).await, 0, "no EVENT_ADD for an OUT link");
    assert_eq!(
        LinkSet::put_admission(&resolver, out_link),
        PutAdmission::Connected
    );

    assert_eq!(LinkSet::get_cached_value(&resolver, out_link), None);
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while LinkSet::get_cached_value(&resolver, out_link).is_none() {
        assert!(
            Instant::now() < deadline,
            "the first read monitors the link"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(monitors(OUT_PV).await, 1);
}
