//! An oversized array put on a CA OUT link truncates; it does not fail.
//!
//! C clamps the request to the target's element count on BOTH put paths,
//! so the behaviour does not depend on whether the link is CA or DB:
//! `dbCaPutLinkCallback` does it before the CA request exists —
//! `if(nRequest>pca->nelements) nRequest = pca->nelements;` then
//! `aConvert(&dbAddr, pbuffer, nRequest, pca->nelements, 0)`
//! (`dbCa.c:604-606`), against `pca->nelements = ca_element_count(chid)`
//! (`:906`) — and `dbPut` does it for a local target with
//! `if (no_elements < nRequest) nRequest = no_elements;`
//! (`dbAccess.c:1365`). Either way the surplus elements are dropped and
//! the put SUCCEEDS.
//!
//! The refusal that must survive is libca's own: a direct
//! `ca_array_put` with `count > ca_element_count` is ECA_BADCOUNT
//! (`nciu.cpp:332-334` → `oldChannelNotify.cpp:512`). C reaches it only
//! from a direct client write, never from a link put, because dbCa has
//! already clamped — which is why the clamp belongs to the link path and
//! not to the client's write API. The last case pins that split.
//!
//! Not this rule: `dbAccess.c:995-1006` clamps `nRequest` on the GET
//! side against `no_elements` and against the caller's capacity. That is
//! the read direction and a separate bound.

// Host/tokio-only, for the reason given in `calink.rs`.
#![cfg(not(feature = "rtems-exec-model"))]

use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::{LinkPutOp, LinkSet};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use epics_ca_rs::calink::CaLinkResolver;
use epics_ca_rs::client::{CaClient, CaClientConfig};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// Point the ambient `EPICS_CA_*` env at `127.0.0.1:port`. Tests here are
/// `#[serial(epics_env)]` so the process-wide env is not raced.
fn pin_env(port: u16) {
    // SAFETY: serialized by `#[serial(epics_env)]`; no other thread
    // reads/writes these vars concurrently.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

/// A three-element DBF_DOUBLE waveform on its own CA server, plus a
/// client and a resolver with the link already connected.
async fn wf3(pv: &str) -> (Arc<CaClient>, CaLinkResolver) {
    let server = CaServer::builder()
        .port(0)
        .record(pv, WaveformRecord::new(3, DbFieldType::Double))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });

    pin_env(port);
    let client = Arc::new(
        CaClient::new_with_config(CaClientConfig::default())
            .await
            .expect("CA client"),
    );
    let resolver = CaLinkResolver::with_client(client.clone());
    assert!(
        resolver
            .wait_for_link_connected(pv, Duration::from_secs(5))
            .await,
        "CA link must connect to the upstream CA server"
    );
    (client, resolver)
}

/// Read `pv` back over a fresh channel until it matches `want`, so a
/// fire-and-forget put is not raced.
async fn readback_becomes(client: &CaClient, pv: &str, want: EpicsValue) {
    let ch = client.create_channel(pv);
    ch.wait_connected(Duration::from_secs(5))
        .await
        .expect("readback channel");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let got = ch
            .get_with_timeout_count(Duration::from_secs(5), 0)
            .await
            .expect("readback get")
            .1;
        if got == want {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{pv} never read back as {want:?}; last saw {got:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The trigger: five elements into a three-element target. C truncates
/// and the put succeeds; refusing it hands the record an error it has no
/// way to act on.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn an_oversized_array_put_truncates_instead_of_failing() {
    let (client, resolver) = wf3("CALINK:CLAMP:WF").await;

    LinkSet::put_value(
        &resolver,
        "CALINK:CLAMP:WF",
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0]),
        LinkPutOp::Async,
    )
    .await
    .expect("an oversized link put truncates, it does not fail (dbCa.c:604-605)");

    readback_becomes(
        &client,
        "CALINK:CLAMP:WF",
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]),
    )
    .await;
}

/// The plain fire-and-forget arm takes the same clamp — C applies it in
/// `dbCaPutLinkCallback`, which is upstream of the `CA_PUT` /
/// `CA_PUT_CALLBACK` split (`dbCa.c:614-624`), so both delivery
/// semantics see the already-clamped request.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_plain_put_takes_the_same_clamp() {
    let (client, resolver) = wf3("CALINK:CLAMP:WFP").await;

    LinkSet::put_value(
        &resolver,
        "CALINK:CLAMP:WFP",
        EpicsValue::DoubleArray(vec![10.0, 20.0, 30.0, 40.0]),
        LinkPutOp::Plain,
    )
    .await
    .expect("a plain oversized link put truncates too");

    readback_becomes(
        &client,
        "CALINK:CLAMP:WFP",
        EpicsValue::DoubleArray(vec![10.0, 20.0, 30.0]),
    )
    .await;
}

/// A scalar target is the same rule at its boundary: `nelements == 1`
/// clamps the request to one element and `aConvert` copies element 0.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn an_oversized_put_to_a_scalar_target_keeps_the_first_element() {
    let server = CaServer::builder()
        .port(0)
        .record("CALINK:CLAMP:AI", AiRecord::new(0.0))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });

    pin_env(port);
    let client = Arc::new(
        CaClient::new_with_config(CaClientConfig::default())
            .await
            .expect("CA client"),
    );
    let resolver = CaLinkResolver::with_client(client.clone());
    assert!(
        resolver
            .wait_for_link_connected("CALINK:CLAMP:AI", Duration::from_secs(5))
            .await
    );

    LinkSet::put_value(
        &resolver,
        "CALINK:CLAMP:AI",
        EpicsValue::DoubleArray(vec![7.5, 8.5, 9.5]),
        LinkPutOp::Async,
    )
    .await
    .expect("an oversized link put to a scalar target keeps element 0");

    readback_becomes(&client, "CALINK:CLAMP:AI", EpicsValue::Double(7.5)).await;
}

/// The split that must NOT be flattened: a direct client write is still
/// refused. libca's `nciu::write` throws `outOfBounds` on
/// `countIn > this->count` and `ca_array_put` returns ECA_BADCOUNT, and
/// nothing about the link-side clamp relaxes that — C reaches the
/// refusal only from a direct `ca_array_put`, never from a link put.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_direct_client_put_is_still_refused() {
    let (client, _resolver) = wf3("CALINK:CLAMP:DIRECT").await;

    let ch = client.create_channel("CALINK:CLAMP:DIRECT");
    ch.wait_connected(Duration::from_secs(5))
        .await
        .expect("direct channel");
    let err = ch
        .put(&EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0]))
        .await
        .expect_err("libca returns ECA_BADCOUNT for an oversized ca_array_put");
    assert!(
        err.to_string().contains("exceeds channel element count"),
        "the direct-write refusal must stay ECA_BADCOUNT-shaped, got: {err}"
    );
}
