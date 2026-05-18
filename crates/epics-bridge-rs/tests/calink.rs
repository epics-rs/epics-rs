//! Integration tests for the `calink` CA-link resolver.
//!
//! These exercise the full Gap-2 path: a real [`epics_ca_rs::server::CaServer`]
//! hosts a PV, a [`CaLinkResolver`] is registered on a [`PvDatabase`] as the
//! `ca` link set, and a soft-channel record whose INP is a CA link fetches the
//! remote PV's value through the monitor-backed cache — the C `dbCa.c` model.
#![cfg(feature = "calink")]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::LinkType;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;
use epics_bridge_rs::calink::{CaLinkResolver, install_calink_resolver};
use epics_ca_rs::client::{CaClient, CaClientConfig};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// Reserve a free TCP port by binding ephemeral then dropping.
fn free_port() -> u16 {
    let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve free CA port");
    probe.local_addr().unwrap().port()
}

/// Build a `CaClient` pinned to `127.0.0.1:port` so a test does not
/// depend on the ambient `EPICS_CA_ADDR_LIST`.
async fn pinned_client(port: u16) -> CaClient {
    pin_env(port);
    CaClient::new_with_config(CaClientConfig::default())
        .await
        .expect("CA client")
}

/// Point the ambient `EPICS_CA_*` env at `127.0.0.1:port`. Tests that
/// touch this are `#[serial(epics_env)]` so the process-wide env is
/// not raced.
fn pin_env(port: u16) {
    // SAFETY: tests sharing process-wide env are serialized via
    // `#[serial(epics_env)]`; no other thread reads/writes these vars
    // concurrently.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

/// Gap 2 — a CA link reads the remote PV's current value through the
/// monitor-backed cache. The resolver's `LinkSet::get_value` must
/// return the value the upstream CA server hosts.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn ca_link_resolves_remote_value() {
    let port = free_port();
    let server = CaServer::builder()
        .port(port)
        .pv("CALINK:SRC", EpicsValue::Double(73.5))
        .build()
        .await
        .expect("CA server");
    let _server = tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let client = Arc::new(pinned_client(port).await);
    let resolver = CaLinkResolver::with_client(client, tokio::runtime::Handle::current());

    // Open the link + wait for the first monitor event to populate
    // the cache.
    let connected = resolver
        .wait_for_link_connected("CALINK:SRC", Duration::from_secs(5))
        .await;
    assert!(connected, "CA link must connect to the upstream CA server");

    // The monitor-backed cache now serves the remote value.
    use epics_base_rs::server::database::LinkSet;
    let value = LinkSet::get_value(&resolver, "CALINK:SRC");
    assert_eq!(
        value.and_then(|v| v.to_f64()),
        Some(73.5),
        "CA link must return the upstream PV's value"
    );
    assert!(
        LinkSet::is_connected(&resolver, "CALINK:SRC"),
        "CA link must report connected once a value is cached"
    );
    assert_eq!(resolver.link_count(), 1);
}

/// Gap 2 — the `ca://` scheme prefix is accepted: `epics-base-rs`
/// stores a `ca://X` link verbatim in `ParsedLink::Ca`, so the
/// resolver must strip it.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn ca_link_resolves_with_scheme_prefix() {
    let port = free_port();
    let server = CaServer::builder()
        .port(port)
        .pv("CALINK:SCHEME", EpicsValue::Long(404))
        .build()
        .await
        .expect("CA server");
    let _server = tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let client = Arc::new(pinned_client(port).await);
    let resolver = CaLinkResolver::with_client(client, tokio::runtime::Handle::current());

    let connected = resolver
        .wait_for_link_connected("ca://CALINK:SCHEME", Duration::from_secs(5))
        .await;
    assert!(connected, "scheme-prefixed CA link must connect");

    use epics_base_rs::server::database::LinkSet;
    assert_eq!(
        LinkSet::get_value(&resolver, "ca://CALINK:SCHEME").and_then(|v| v.to_f64()),
        Some(404.0),
        "scheme-prefixed CA link must resolve to the upstream value"
    );
}

/// Gap 2 end-to-end — a soft-channel `ai` record whose INP is a CA
/// link, processed through `process_record_with_links`, fetches the
/// remote PV value into its own VAL. This is the exact C path:
/// `aiRecord::process` → `dbGetLink` → `dbCaGetLink`.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn record_with_ca_inp_link_reads_remote_value() {
    let port = free_port();
    let server = CaServer::builder()
        .port(port)
        .pv("CALINK:INP:SRC", EpicsValue::Double(19.25))
        .build()
        .await
        .expect("CA server");
    let _server = tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let client = Arc::new(pinned_client(port).await);
    let db = PvDatabase::new();
    // Register the CA link set on the database.
    let resolver = CaLinkResolver::with_client(client, tokio::runtime::Handle::current());
    db.register_link_set("ca", Arc::new(resolver.clone())).await;

    // Pre-open the link and wait so the synchronous lset read serves
    // from cache (the C `dbCaAddLink` + iocInit-wait analogue).
    let connected = resolver
        .wait_for_link_connected("CALINK:INP:SRC", Duration::from_secs(5))
        .await;
    assert!(connected, "CA link must connect before record processing");

    // A soft-channel ai record whose INP is a bare ` CA`-modified
    // link — Gap 1 classifies it as a CA link, Gap 2 resolves it.
    db.add_record("CADST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("CADST").await.expect("record exists");
        let mut inst = rec.write().await;
        inst.put_common_field("INP", EpicsValue::String("CALINK:INP:SRC CA".into()))
            .unwrap();
        inst.common.udf = false;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("CADST", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("CADST").await.expect("record exists");
    let inst = rec.read().await;
    assert_eq!(
        inst.record.val().and_then(|v| v.to_f64()),
        Some(19.25),
        "ai record must read the CA link's remote value into VAL"
    );
}

/// Gap 2 OUT write — a CA-type OUT link writes the remote PV.
/// `CaLinkResolver::put_value` (the `LinkSet` OUT-write path) must
/// push a value through the CA channel into the upstream CA server's
/// PV. This mirrors the C `dbCaPutLink` path for a `DBF_OUTLINK`
/// carrying a ` CA` modifier. Verified two ways: the resolver's own
/// monitor-backed cache reflects the new value, and a fresh
/// independent CA client GET against the upstream server reads it
/// back — proving the write reached the server, not just the cache.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn ca_link_out_write_updates_remote_pv() {
    let port = free_port();
    let server = CaServer::builder()
        .port(port)
        .pv("CALINK:OUT:DST", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("CA server");
    let _server = tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let client = Arc::new(pinned_client(port).await);
    let resolver = CaLinkResolver::with_client(client, tokio::runtime::Handle::current());

    // Open the link + wait for the first monitor event so the channel
    // is connected and the OUT write has a live circuit to push on.
    let connected = resolver
        .wait_for_link_connected("CALINK:OUT:DST", Duration::from_secs(5))
        .await;
    assert!(connected, "CA link must connect before the OUT write");

    use epics_base_rs::server::database::LinkSet;
    // Bare ` CA`-modified OUT link form: `epics-base-rs` strips the
    // modifier and hands the resolver the bare PV name.
    LinkSet::put_value(&resolver, "CALINK:OUT:DST", EpicsValue::Double(88.0))
        .expect("CA-link OUT write must succeed");

    // The resolver's monitor sees the server-side change — poll the
    // monitor-backed cache until the write propagates back.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if LinkSet::get_value(&resolver, "CALINK:OUT:DST").and_then(|v| v.to_f64()) == Some(88.0) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "CA-link OUT write did not propagate to the resolver cache"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Independent confirmation: a fresh CA client GET against the
    // upstream server reads the written value — the write reached the
    // server, not merely the resolver's local cache.
    let verify_client = pinned_client(port).await;
    let ch = verify_client.create_channel("CALINK:OUT:DST");
    ch.wait_connected(Duration::from_secs(5))
        .await
        .expect("verify channel connects");
    let (_dbf, read_back) = ch.get().await.expect("verify GET");
    assert_eq!(
        read_back.to_f64(),
        Some(88.0),
        "upstream CA server PV must reflect the OUT-link write"
    );
}

/// Gap 2 OUT write — the `ca://` scheme-prefixed form of an OUT link
/// is accepted by `put_value` (the resolver strips the prefix), same
/// as the INP-side `ca_link_resolves_with_scheme_prefix` test.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn ca_link_out_write_accepts_scheme_prefix() {
    let port = free_port();
    let server = CaServer::builder()
        .port(port)
        .pv("CALINK:OUT:SCHEME", EpicsValue::Long(7))
        .build()
        .await
        .expect("CA server");
    let _server = tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let client = Arc::new(pinned_client(port).await);
    let resolver = CaLinkResolver::with_client(client, tokio::runtime::Handle::current());

    let connected = resolver
        .wait_for_link_connected("ca://CALINK:OUT:SCHEME", Duration::from_secs(5))
        .await;
    assert!(connected, "scheme-prefixed CA OUT link must connect");

    use epics_base_rs::server::database::LinkSet;
    LinkSet::put_value(&resolver, "ca://CALINK:OUT:SCHEME", EpicsValue::Long(123))
        .expect("scheme-prefixed CA-link OUT write must succeed");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if LinkSet::get_value(&resolver, "ca://CALINK:OUT:SCHEME").and_then(|v| v.to_f64())
            == Some(123.0)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "scheme-prefixed CA OUT write did not propagate"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// `install_calink_resolver` registers under the `ca` scheme so the
/// database's `registered_link_schemes` reports it.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn install_registers_ca_scheme() {
    // No server needed — the resolver registers regardless of
    // upstream connectivity.
    pin_env(free_port());

    let db = PvDatabase::new();
    let _resolver = install_calink_resolver(&db, tokio::runtime::Handle::current())
        .await
        .expect("install resolver");
    let schemes = db.registered_link_schemes().await;
    assert!(
        schemes.contains(&"ca".to_string()),
        "install_calink_resolver must register the `ca` scheme, got {schemes:?}"
    );
}

/// Sanity: the Gap-1 parser change is visible from the bridge crate —
/// a ` CA`-modified link classifies as `LinkType::Ca`.
#[test]
fn ca_modifier_link_classifies_as_ca() {
    assert_eq!(
        epics_base_rs::server::record::link_field_type("REMOTE:PV.VAL CA"),
        LinkType::Ca,
    );
}
