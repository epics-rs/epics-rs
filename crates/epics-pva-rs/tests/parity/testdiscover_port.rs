//! Port of pvxs `test/testdiscover.cpp::testBeacon` (subset).
//!
//! pvxs starts a server, subscribes a discover callback, and verifies
//! `Discovered::Online` events arrive when the server emits beacons.
//! We test the public `SearchEngine::discover()` API by injecting a
//! synthetic `BeaconObserved` command and observing the receiver.

#![cfg(test)]

// RTEMS-EXEC-MODEL-ALLOW(3): not run by the default nextest profile - this file is a module of the `parity_interop` binary, which `.config/nextest.toml`'s default-filter excludes.

use std::time::Duration;

use epics_pva_rs::client_native::search_engine::{Discovered, SearchEngine};

#[tokio::test]
async fn pvxs_discover_emits_online_for_first_observed_beacon() {
    let engine = SearchEngine::spawn(Vec::new(), Vec::new())
        .await
        .expect("spawn");
    let mut rx = engine.discover().await.expect("subscribe");

    let server: std::net::SocketAddr = "127.0.0.1:5075".parse().unwrap();
    let guid = [9u8; 12];
    engine.observe_beacon(server, guid).await;

    let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");
    match evt {
        Discovered::Online {
            server: s, guid: g, ..
        } => {
            assert_eq!(s, server);
            assert_eq!(g, guid);
        }
        Discovered::Timeout { .. } => panic!("unexpected Timeout"),
    }
}

#[tokio::test]
async fn pvxs_discover_no_event_for_repeated_same_guid() {
    let engine = SearchEngine::spawn(Vec::new(), Vec::new())
        .await
        .expect("spawn");
    let mut rx = engine.discover().await.expect("subscribe");

    let server: std::net::SocketAddr = "127.0.0.1:5076".parse().unwrap();
    let guid = [3u8; 12];

    // First observation: should produce an event.
    engine.observe_beacon(server, guid).await;
    let _first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("first event timeout")
        .expect("first event channel closed");

    // Second observation with same GUID: no new event (BeaconTracker
    // returns false, so we don't notify subscribers).
    engine.observe_beacon(server, guid).await;
    let second = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
    assert!(
        second.is_err(),
        "expected no second event for same guid, got {second:?}"
    );
}

#[tokio::test]
async fn pvxs_discover_emits_for_guid_change_same_server() {
    let engine = SearchEngine::spawn(Vec::new(), Vec::new())
        .await
        .expect("spawn");
    let mut rx = engine.discover().await.expect("subscribe");

    let server: std::net::SocketAddr = "127.0.0.1:5077".parse().unwrap();
    engine.observe_beacon(server, [1u8; 12]).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("first")
        .expect("first");

    // Same server, different GUID = the server was replaced (restart / new
    // instance). pvxs reports this as the prior incarnation going offline and
    // then the new one coming online: a Timeout for the OLD guid FOLLOWED BY
    // an Online for the NEW guid (client.cpp:814-844, mirrored by the
    // BeaconAction::Changed arm in search_engine.rs `emit_beacon_action`).
    // Assert that exact ordered pair — the previous single-Online expectation
    // ignored the Timeout pvxs emits first and tripped on it.
    engine.observe_beacon(server, [2u8; 12]).await;
    let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("guid-change Timeout timed out")
        .expect("channel closed before Timeout");
    match first {
        Discovered::Timeout { guid, .. } => assert_eq!(guid, [1u8; 12]),
        other => panic!("expected Timeout for the old GUID first, got {other:?}"),
    }
    let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("guid-change Online timed out")
        .expect("channel closed before Online");
    match second {
        Discovered::Online { guid, .. } => assert_eq!(guid, [2u8; 12]),
        other => panic!("expected Online for the new GUID second, got {other:?}"),
    }
}
