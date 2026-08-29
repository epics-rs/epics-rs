//! The `ca://` lset must answer link reads on any runtime flavor.

#![cfg(feature = "client-core")]

// RTEMS-EXEC-MODEL-ALLOW(2): both flavored tests run a live CaServer
// over tokio::net. These run and pass in the exec-backend suite on the
// tokio driver.
use epics_base_rs::server::database::LinkSet;
use epics_ca_rs::calink::CaLinkResolver;

#[tokio::test(flavor = "current_thread")]
async fn get_value_of_an_unconnected_link_on_current_thread_runtime() {
    let resolver = CaLinkResolver::new();
    assert!(
        resolver.get_value("NO:SUCH:PV:CALINK").await.is_none(),
        "an unconnected link reads as no value"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_value_of_an_unconnected_link_on_multi_thread_runtime() {
    let resolver = CaLinkResolver::new();
    assert!(
        resolver.get_value("NO:SUCH:PV:CALINK").await.is_none(),
        "an unconnected link reads as no value"
    );
}
