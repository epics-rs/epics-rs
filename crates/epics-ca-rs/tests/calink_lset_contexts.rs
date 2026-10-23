//! The `ca://` lset must answer link reads on any runtime flavor.

#![cfg(feature = "client-core")]

// RTEMS-EXEC-MODEL-ALLOW(2): both flavored tests run a live CaServer
// over tokio::net. These run and pass in the exec-backend suite on the
// tokio driver.
use epics_base_rs::server::database::{LinkRef, LinkSet};
use epics_base_rs::server::record::OpenLatch;
use epics_ca_rs::calink::CaLinkResolver;

#[tokio::test(flavor = "current_thread")]
async fn get_value_of_an_unconnected_link_on_current_thread_runtime() {
    let resolver = CaLinkResolver::new(
        epics_base_rs::runtime::task::Reactor::current().expect("test runtime"),
    );
    assert!(
        resolver
            .get_value(LinkRef::new(b"NO:SUCH:PV:CALINK", &OpenLatch::default()))
            .await
            .is_none(),
        "an unconnected link reads as no value"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_value_of_an_unconnected_link_on_multi_thread_runtime() {
    let resolver = CaLinkResolver::new(
        epics_base_rs::runtime::task::Reactor::current().expect("test runtime"),
    );
    assert!(
        resolver
            .get_value(LinkRef::new(b"NO:SUCH:PV:CALINK", &OpenLatch::default()))
            .await
            .is_none(),
        "an unconnected link reads as no value"
    );
}
