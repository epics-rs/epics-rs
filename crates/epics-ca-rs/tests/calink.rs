//! Integration tests for the `calink` CA-link resolver.
//!
//! These exercise the full Gap-2 path: a real [`epics_ca_rs::server::CaServer`]
//! hosts a PV, a [`CaLinkResolver`] is registered on a [`PvDatabase`] as the
//! `ca` link set, and a soft-channel record whose INP is a CA link fetches the
//! remote PV's value through the monitor-backed cache — the C `dbCa.c` model.
// The tests that drive a live server are `tokio_backend`-only, so on
// `exec_backend` the fixtures and imports they share go unreferenced while the
// rest of this file still runs. The default build lints it in full.
#![cfg_attr(exec_backend, allow(dead_code, unused_imports))]
#![cfg(feature = "client-core")]

// RTEMS-EXEC-MODEL-ALLOW(1): measured - the one case left ungated here runs
// and passes under `EPICS_RS_BUILD_EXEC_BACKEND=thread`; the other 11 drive a
// live `CaServer` and leave the exec-backend suite with it.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::LinkType;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::calink::{CaLinkResolver, install_calink_resolver};
use epics_ca_rs::client::{CaClient, CaClientConfig};
#[cfg(tokio_backend)]
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// A held, silent UDP socket that OWNS a dead CA port for the test's whole
/// lifetime — for tests that want an unreachable upstream.
///
/// Ownership rule: a port is TAKEN by binding it, never probed and handed
/// on. The old `TcpListener::bind(:0)` + drop probe reserved nothing after
/// it returned — and it never touched the UDP namespace at all, so a
/// parallel test's `CaServer` (which binds UDP `:0`) could land on the very
/// same number and answer the `EPICS_CA_SERVER_PORT` searches, flaking the
/// "dead upstream" false-alive. Binding UDP `127.0.0.1:0` and keeping the
/// socket (never reading it) instead guarantees (a) no other socket in the
/// process can take the number, and (b) every search sent there lands in
/// this socket's buffer and is never answered — the upstream is
/// deterministically dead. UDP-only suffices: the CA client only opens a
/// TCP circuit after a UDP search *reply* names a server, which never comes.
///
/// The caller must bind the returned guard for the test duration — the port
/// is dead only while the socket lives. A test that *hosts* a server must
/// not use this: it asks the server for port 0 and reads back the port it
/// bound.
fn dead_upstream() -> (std::net::UdpSocket, u16) {
    let sock = std::net::UdpSocket::bind(("127.0.0.1", 0)).expect("own a dead CA port");
    let port = sock.local_addr().unwrap().port();
    (sock, port)
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

#[cfg(tokio_backend)]
/// Gap 2 — a CA link reads the remote PV's current value through the
/// monitor-backed cache. The resolver's `LinkSet::get_value` must
/// return the value the upstream CA server hosts.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn ca_link_resolves_remote_value() {
    let server = CaServer::builder()
        .port(0)
        .pv("CALINK:SRC", EpicsValue::Double(73.5))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });

    let client = Arc::new(pinned_client(port).await);
    let resolver = CaLinkResolver::with_client(client);

    // Open the link + wait for the first monitor event to populate
    // the cache.
    let connected = resolver
        .wait_for_link_connected("CALINK:SRC", budget::FACT_BUDGET)
        .await;
    assert!(connected, "CA link must connect to the upstream CA server");

    // The monitor-backed cache now serves the remote value.
    use epics_base_rs::server::database::LinkSet;
    let value = LinkSet::get_value(&resolver, "CALINK:SRC").await;
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

#[cfg(tokio_backend)]
/// `dbcar`'s per-link report against a LIVE channel: C reads
/// `ca_host_name`, `ca_read_access`, `ca_write_access` and
/// `pca->nDisconnect` off the `chid` (`dbCaTest.c:95-123`), and the columns
/// are only as true as what the resolver caches. The counter's edge
/// behaviour is unit-tested on its owner; what needs a real channel is that
/// the other four read the channel and not a default.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn link_diagnostics_reads_a_live_channel() {
    use epics_base_rs::server::database::LinkSet;

    let server = CaServer::builder()
        .port(0)
        .pv("CALINK:DIAG", EpicsValue::Double(4.5))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    // `ca_host_name` names the peer of the CIRCUIT, so it carries the TCP
    // port, which this server picks independently of the search port.
    let tcp_port = server.tcp_port();
    let _server = tokio::spawn(async move { server.run().await });

    let client = Arc::new(pinned_client(port).await);
    let resolver = CaLinkResolver::with_client(client);

    // A name the resolver has never opened is C's `pca == NULL`, which
    // `dbcar` renders as a not-connected link with zero counters — and the
    // report must NOT open it, because `dbcar` is a report.
    assert!(
        LinkSet::link_diagnostics(&resolver, "CALINK:NEVER")
            .await
            .is_none()
    );
    assert_eq!(resolver.link_count(), 0, "the report opened nothing");

    assert!(
        resolver
            .wait_for_link_connected("CALINK:DIAG", budget::FACT_BUDGET)
            .await
    );

    // Before any read, C's `pvlOptInpNative` is unset: the bit is set BY
    // `dbCaGetLink` (`dbCa.c:455-457`), not by connecting.
    let before = LinkSet::link_diagnostics(&resolver, "CALINK:DIAG")
        .await
        .expect("an opened link reports");
    assert!(before.connected);
    assert!(!before.input_native, "connecting is not an input transfer");

    let _ = LinkSet::get_value(&resolver, "CALINK:DIAG").await;
    let after = LinkSet::link_diagnostics(&resolver, "CALINK:DIAG")
        .await
        .expect("an opened link reports");
    assert!(after.input_native, "a value read is C's pvlOptInpNative");
    assert_eq!(after.n_disconnect, 0, "the link never dropped");
    // This server grants both rights, so `dbcar` prints "Read/Write" and
    // neither `can't` counter moves.
    assert!(after.read_access && after.write_access);
    // C prints `ca_host_name(chid)`, the peer with its port — never the
    // empty string, and never the PV name.
    assert!(
        after.host.contains(&tcp_port.to_string()),
        "host must name the server's own address, got {:?}",
        after.host
    );
    // The three C never sets at the pin: both OUT bits are inside
    // `/* Disabled by ANJ ... */` (`dbCa.c:539-542`, `:555-558`), and the
    // string monitor this port does not keep is `pvlOptInpString`.
    assert!(!after.input_string && !after.output_native && !after.output_string);
}

#[cfg(tokio_backend)]
/// epics-base #856 (`dbCa: iocInit wait for all conditions`): the
/// iocInit gate `init_ready` is satisfied only once the detached CTRL
/// attribute fetch has completed too, and it does flip true on a live
/// link — a connected, value-serving link is necessary but not
/// sufficient.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn ca_link_init_ready_flips_after_metadata_fetch() {
    let server = CaServer::builder()
        .port(0)
        .pv("CALINK:INITREADY", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });

    let client = Arc::new(pinned_client(port).await);
    let resolver = CaLinkResolver::with_client(client);

    use epics_base_rs::server::database::LinkSet;
    // Unopened link: not init-ready.
    assert!(!LinkSet::init_ready(&resolver, "CALINK:INITREADY"));

    let connected = resolver
        .wait_for_link_connected("CALINK:INITREADY", budget::FACT_BUDGET)
        .await;
    assert!(connected, "CA link must connect to the upstream CA server");

    // The attribute fetch is detached from the monitor path, so poll
    // for the flip; a connected link whose fetch never completes would
    // hold iocInit, and this assert names that failure.
    let deadline = std::time::Instant::now() + budget::FACT_BUDGET;
    while !LinkSet::init_ready(&resolver, "CALINK:INITREADY") {
        assert!(
            std::time::Instant::now() < deadline,
            "metadata fetch never completed: init_ready stayed false on a connected link"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(LinkSet::is_connected(&resolver, "CALINK:INITREADY"));
}

#[cfg(tokio_backend)]
/// Gap 2 — the `ca://` scheme prefix is accepted: `epics-base-rs`
/// stores a `ca://X` link verbatim in `ParsedLink::Ca`, so the
/// resolver must strip it.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn ca_link_resolves_with_scheme_prefix() {
    let server = CaServer::builder()
        .port(0)
        .pv("CALINK:SCHEME", EpicsValue::Long(404))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });

    let client = Arc::new(pinned_client(port).await);
    let resolver = CaLinkResolver::with_client(client);

    let connected = resolver
        .wait_for_link_connected("ca://CALINK:SCHEME", budget::FACT_BUDGET)
        .await;
    assert!(connected, "scheme-prefixed CA link must connect");

    use epics_base_rs::server::database::LinkSet;
    assert_eq!(
        LinkSet::get_value(&resolver, "ca://CALINK:SCHEME")
            .await
            .and_then(|v| v.to_f64()),
        Some(404.0),
        "scheme-prefixed CA link must resolve to the upstream value"
    );
}

#[cfg(tokio_backend)]
/// Gap 2 end-to-end — a soft-channel `ai` record whose INP is a CA
/// link, processed through `process_record_with_links`, fetches the
/// remote PV value into its own VAL. This is the exact C path:
/// `aiRecord::process` → `dbGetLink` → `dbCaGetLink`.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn record_with_ca_inp_link_reads_remote_value() {
    let server = CaServer::builder()
        .port(0)
        .pv("CALINK:INP:SRC", EpicsValue::Double(19.25))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });

    let client = Arc::new(pinned_client(port).await);
    let db = PvDatabase::new();
    // Register the CA link set on the database.
    let resolver = CaLinkResolver::with_client(client);
    db.register_link_set("ca", Arc::new(resolver.clone())).await;

    // Pre-open the link and wait so the synchronous lset read serves
    // from cache (the C `dbCaAddLink` + iocInit-wait analogue).
    let connected = resolver
        .wait_for_link_connected("CALINK:INP:SRC", budget::FACT_BUDGET)
        .await;
    assert!(connected, "CA link must connect before record processing");

    // A soft-channel ai record whose INP is a bare ` CA`-modified
    // link — Gap 1 classifies it as a CA link, Gap 2 resolves it.
    db.add_record("CADST", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("CADST").expect("record exists");
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("CALINK:INP:SRC CA".into()))
            .unwrap();
        inst.common.udf = 0;
    }

    let mut visited = HashSet::new();
    db.process_record_with_links("CADST", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("CADST").expect("record exists");
    let inst = rec.read();
    assert_eq!(
        inst.record.val().and_then(|v| v.to_f64()),
        Some(19.25),
        "ai record must read the CA link's remote value into VAL"
    );
}

#[cfg(tokio_backend)]
/// A Passive `ai` holder
/// whose INP is a `CP` CA link MUST process (and read the new value into
/// VAL) on every remote change, driven solely by the calink monitor
/// callback — never by an explicit `process_record` call.
///
/// This is the exact C `dbCa.c` path: `eventCallback` refreshes the
/// cached value and adds `CA_DBPROCESS` for a CP link (`dbCa.c:891`, `:958-962`),
/// and the worker thread runs `db_process(prec)` (`dbCa.c:1255`).
///
/// The holder is Passive on purpose: a Passive record never self-scans,
/// so its CA link never opens lazily and the monitor that drives the
/// dispatch is never created — the chicken-and-egg the iocInit warm in
/// `setup_cp_links` closes. Pre-fix, `setup_cp_links` matched only
/// `ParsedLink::Db`, so the holder was never registered, never warmed,
/// and `run_monitor` had no dispatch hook: VAL stayed at its initial 0.0
/// forever. Two observations prove the fix:
///   1. the warm's first monitor event drives VAL to the source's 5.0;
///   2. an independent remote write to 42.0 drives VAL to 42.0.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn ca_cp_holder_processes_on_remote_change() {
    let server = CaServer::builder()
        .port(0)
        .pv("CALINK:CP:SRC", EpicsValue::Double(5.0))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });

    pin_env(port);
    let db = PvDatabase::new();

    // Passive ai holder with a bare ` CP` INP link to the remote PV. `CP` is
    // the whole modifier: C's process-class chain (`dbStaticLib.c:2369-2373`)
    // assigns exactly one bit and matches `CA` *before* `CP`, so spelling this
    // `"... CP CA"` would yield `pvlOptCA` alone — a plain CA link whose
    // holder never processes. A bare `CP` is what makes the link a dbCa link
    // with `CA_DBPROCESS` (`dbCa.c:958-962`).
    // ai's default SCAN is Passive — assert it so the "never self-scans,
    // so the link is never opened lazily" precondition is explicit.
    db.add_record("CALINK:CP:HOLDER", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("CALINK:CP:HOLDER").unwrap();
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("CALINK:CP:SRC CP".into()))
            .unwrap();
        inst.common.udf = 0;
        assert_eq!(
            inst.common.scan,
            epics_base_rs::server::record::ScanType::Passive,
            "holder must be Passive so its link only opens via the iocInit warm"
        );
    }

    // Production wiring: installs the `ca` lset AND attaches the db so
    // the monitor callback can dispatch CP holders (this
    // fix). Uses its own client, which picks
    // up the pinned env set above.
    let _resolver = install_calink_resolver(&db).await;

    // iocInit step: registers the external CP edge AND warms (opens) the
    // monitor for the Passive holder's source PV.
    db.setup_cp_links().await;

    // (1) The warm's first monitor event must drive the holder to process
    // and read the remote value (5.0) into VAL — with NO explicit
    // process_record call anywhere in this test.
    let deadline = std::time::Instant::now() + budget::FACT_BUDGET;
    loop {
        let v = {
            let rec = db.get_record("CALINK:CP:HOLDER").unwrap();
            let inst = rec.read();
            inst.record.val().and_then(|v| v.to_f64())
        };
        if v == Some(5.0) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "CP holder VAL must reach the warmed source value 5.0 \
             (got {v:?}) — the iocInit warm + monitor dispatch did not fire"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // (2) An independent CA client changes the source to 42.0. The
    // steady-state monitor event must drive the holder to reprocess to
    // 42.0 — again with no explicit process_record call.
    let writer = pinned_client(port).await;
    let wch = writer.create_channel("CALINK:CP:SRC");
    wch.wait_connected(budget::FACT_BUDGET)
        .await
        .expect("writer channel connects");
    wch.put(&EpicsValue::Double(42.0))
        .await
        .expect("remote write to source");

    let deadline = std::time::Instant::now() + budget::FACT_BUDGET;
    loop {
        let v = {
            let rec = db.get_record("CALINK:CP:HOLDER").unwrap();
            let inst = rec.read();
            inst.record.val().and_then(|v| v.to_f64())
        };
        if v == Some(42.0) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "CP holder VAL must follow the remote change to 42.0 \
             (got {v:?}) — the steady-state monitor dispatch did not fire"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(tokio_backend)]
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
    let server = CaServer::builder()
        .port(0)
        .pv("CALINK:OUT:DST", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });

    let client = Arc::new(pinned_client(port).await);
    let resolver = CaLinkResolver::with_client(client);

    // Open the link + wait for the first monitor event so the channel
    // is connected and the OUT write has a live circuit to push on.
    let connected = resolver
        .wait_for_link_connected("CALINK:OUT:DST", budget::FACT_BUDGET)
        .await;
    assert!(connected, "CA link must connect before the OUT write");

    use epics_base_rs::server::database::{LinkPutOp, LinkSet};
    // Bare ` CA`-modified OUT link form: `epics-base-rs` strips the
    // modifier and hands the resolver the bare PV name.
    LinkSet::put_value(
        &resolver,
        "CALINK:OUT:DST",
        EpicsValue::Double(88.0),
        LinkPutOp::Plain,
    )
    .await
    .expect("CA-link OUT write must succeed");

    // The resolver's monitor sees the server-side change — poll the
    // monitor-backed cache until the write propagates back.
    let deadline = std::time::Instant::now() + budget::FACT_BUDGET;
    loop {
        if LinkSet::get_value(&resolver, "CALINK:OUT:DST")
            .await
            .and_then(|v| v.to_f64())
            == Some(88.0)
        {
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
    ch.wait_connected(budget::FACT_BUDGET)
        .await
        .expect("verify channel connects");
    let (_dbf, read_back) = ch.get().await.expect("verify GET");
    assert_eq!(
        read_back.to_f64(),
        Some(88.0),
        "upstream CA server PV must reflect the OUT-link write"
    );
}

#[cfg(tokio_backend)]
/// Gap 2 OUT write, completion-aware arm — a `LinkPutOp::Async` put
/// (the put-notify / blocking-put chain case, C `dbCaPutLinkCallback`
/// → `ca_array_put_callback`) issues a CA WRITE_NOTIFY and waits for
/// the server's put-completion reply, then the value lands on the
/// upstream PV. Contrast with `ca_link_out_write_updates_remote_pv`,
/// which exercises the `LinkPutOp::Plain` fire-and-forget
/// `CA_PROTO_WRITE` arm (C `dbCaPutLink` → `ca_array_put`); the two
/// arms must route to the two distinct CA write opcodes.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn ca_link_out_write_async_waits_for_completion() {
    let server = CaServer::builder()
        .port(0)
        .pv("CALINK:OUT:ASYNC", EpicsValue::Double(1.0))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });

    let client = Arc::new(pinned_client(port).await);
    let resolver = CaLinkResolver::with_client(client);

    let connected = resolver
        .wait_for_link_connected("CALINK:OUT:ASYNC", budget::FACT_BUDGET)
        .await;
    assert!(connected, "CA link must connect before the async OUT write");

    use epics_base_rs::server::database::{LinkPutOp, LinkSet};
    // Completion-aware put: this call returns only after the server's
    // WRITE_NOTIFY completion reply, so the written value is already
    // committed on the upstream PV by the time `put_value` returns.
    LinkSet::put_value(
        &resolver,
        "CALINK:OUT:ASYNC",
        EpicsValue::Double(42.0),
        LinkPutOp::Async,
    )
    .await
    .expect("completion-aware CA-link OUT write must succeed");

    // An independent CA client GET reads the committed value back —
    // the WRITE_NOTIFY reached the server and completed.
    let verify_client = pinned_client(port).await;
    let ch = verify_client.create_channel("CALINK:OUT:ASYNC");
    ch.wait_connected(budget::FACT_BUDGET)
        .await
        .expect("verify channel connects");
    let (_dbf, read_back) = ch.get().await.expect("verify GET");
    assert_eq!(
        read_back.to_f64(),
        Some(42.0),
        "upstream CA server PV must reflect the completion-aware OUT write"
    );
}

#[cfg(tokio_backend)]
/// Gap 2 OUT write — the `ca://` scheme-prefixed form of an OUT link
/// is accepted by `put_value` (the resolver strips the prefix), same
/// as the INP-side `ca_link_resolves_with_scheme_prefix` test.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn ca_link_out_write_accepts_scheme_prefix() {
    let server = CaServer::builder()
        .port(0)
        .pv("CALINK:OUT:SCHEME", EpicsValue::Long(7))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });

    let client = Arc::new(pinned_client(port).await);
    let resolver = CaLinkResolver::with_client(client);

    let connected = resolver
        .wait_for_link_connected("ca://CALINK:OUT:SCHEME", budget::FACT_BUDGET)
        .await;
    assert!(connected, "scheme-prefixed CA OUT link must connect");

    use epics_base_rs::server::database::{LinkPutOp, LinkSet};
    LinkSet::put_value(
        &resolver,
        "ca://CALINK:OUT:SCHEME",
        EpicsValue::Long(123),
        LinkPutOp::Plain,
    )
    .await
    .expect("scheme-prefixed CA-link OUT write must succeed");

    let deadline = std::time::Instant::now() + budget::FACT_BUDGET;
    loop {
        if LinkSet::get_value(&resolver, "ca://CALINK:OUT:SCHEME")
            .await
            .and_then(|v| v.to_f64())
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

#[cfg(tokio_backend)]
/// End-to-end — a CA link inherits the remote PV's
/// display/control limits, precision, units, DBF type and element count
/// through `LinkSet::link_metadata`. The upstream `ai` record carries
/// EGU/PREC/HOPR/LOPR; a `DBR_CTRL` get on connect caches them, mirroring
/// C `dbCa.c` `getAttribEventCallback` populating `pca->controlLimits` &c.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn ca_link_exposes_remote_metadata() {
    // ai record with real display/control metadata so the upstream CTRL
    // get returns non-default limits/precision/units.
    let mut src = AiRecord::new(50.0);
    src.egu = "degC".into();
    src.hopr = 100.0;
    src.lopr = -50.0;
    src.prec = 3;
    let server = CaServer::builder()
        .port(0)
        .record("CALINK:META:SRC", src)
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });

    let client = Arc::new(pinned_client(port).await);
    let resolver = CaLinkResolver::with_client(client);

    let connected = resolver
        .wait_for_link_connected("CALINK:META:SRC", budget::FACT_BUDGET)
        .await;
    assert!(connected, "CA link must connect to the upstream CA server");

    use epics_base_rs::server::database::{LinkDbfType, LinkSet};
    // The CTRL attribute fetch is detached after the `Connected` event,
    // so poll until the cached metadata lands.
    let deadline = std::time::Instant::now() + budget::FACT_BUDGET;
    let md = loop {
        if let Some(md) = LinkSet::link_metadata(&resolver, "CALINK:META:SRC") {
            // Wait for the CTRL get to fill the limits, not just the
            // channel-info type/count from a partial first store.
            if md.graphic_limits.is_some() {
                break md;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "CA link metadata never populated"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert_eq!(
        md.dbf_type,
        Some(LinkDbfType::Double),
        "ai VAL is DBF_DOUBLE"
    );
    assert_eq!(md.element_count, Some(1), "scalar ai is one element");
    assert_eq!(
        md.graphic_limits,
        Some((-50.0, 100.0)),
        "graphic limits come from LOPR/HOPR"
    );
    assert_eq!(
        md.control_limits,
        Some((-50.0, 100.0)),
        "input record control limits fall back to LOPR/HOPR"
    );
    assert_eq!(md.precision, Some(3), "precision comes from PREC");
    assert_eq!(md.units.as_deref(), Some("degC"), "units come from EGU");

    // While connected the metadata is served; the read path gates on the
    // connection flag exactly like the value/alarm getters.
    assert!(LinkSet::is_connected(&resolver, "CALINK:META:SRC"));
}

/// `install_calink_resolver` registers under the `ca` scheme so the
/// database's `registered_link_schemes` reports it.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn install_registers_ca_scheme() {
    // No server needed — the resolver registers regardless of
    // upstream connectivity. Own a dead port for the whole test so a
    // parallel `CaServer` cannot land on the number mid-run.
    let (_dead, port) = dead_upstream();
    pin_env(port);

    let db = PvDatabase::new();
    let _resolver = install_calink_resolver(&db).await;
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

#[cfg(tokio_backend)]
/// The calink `ca` link set
/// must install at the base `AfterCaLinkInit` seam (BEFORE `setup_cp_links`)
/// when an IOC is built with
/// [`IocApplication::register_link_set_installer`], so a Passive CP holder
/// warms through the PRODUCTION `IocApplication::run` path — not only when a
/// test hand-replicates the ordering with a manual `install_calink_resolver`
/// + `setup_cp_links` (the [`ca_cp_holder_processes_on_remote_change`] path).
///
/// Pre-fix, calink was installed inside the Phase-3 protocol runner — AFTER
/// `setup_cp_links` had already warmed Passive CP holders — so the holder's
/// `resolve_external_pv` open no-op'd (no `ca` link set was registered yet)
/// and VAL stayed at its initial 0.0; the pure-CA `run_ca_ioc` runner never
/// installed calink at all. This test fails on either of those.
///
/// The holder record and its bare ` CP` INP link are loaded via
/// `dbLoadRecords` from an st.cmd — the real iocInit path, where INP is set
/// before `setup_cp_links` runs. A custom protocol runner (in place of
/// `run_ca_ioc`) observes the warmed VAL and returns `Ok(())`, so `run`
/// completes instead of serving forever.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn calink_warms_cp_holder_via_iocapplication_run_seam() {
    use epics_base_rs::server::ioc_app::IocApplication;

    let server = CaServer::builder()
        .port(0)
        .pv("CALINK:SEAM:SRC", EpicsValue::Double(5.0))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });

    pin_env(port);

    // The Passive `ai` holder's INP is a bare ` CP` link to the remote PV.
    // `CP` is the whole modifier: C's process-class chain matches `CA` before
    // `CP` and assigns exactly one bit, so `"... CP CA"` would be `pvlOptCA`
    // alone — a plain CA link whose holder never processes
    // (`dbStaticLib.c:2369-2373`). PINI=NO so the holder never self-processes;
    // only the iocInit CP warm can open its monitor and drive the first
    // process.
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("seam.db");
    std::fs::write(
        &db_path,
        "record(ai, \"CALINK:SEAM:HOLDER\") {\n\
         \tfield(INP, \"CALINK:SEAM:SRC CP\")\n\
         \tfield(PINI, \"NO\")\n\
         \tfield(SCAN, \"Passive\")\n\
         }\n",
    )
    .expect("write seam.db");
    let stcmd_path = dir.path().join("st.cmd");
    std::fs::write(
        &stcmd_path,
        format!("dbLoadRecords(\"{}\")\n", db_path.display()),
    )
    .expect("write st.cmd");

    // The seam under test: register the calink installer at construction.
    // It fires at `AfterCaLinkInit`, before `setup_cp_links` — zero further
    // wiring, no feature opt-in.
    let result = IocApplication::new()
        .startup_script(stcmd_path.to_str().unwrap())
        .register_link_set_installer(epics_ca_rs::calink::calink_link_set_install)
        .run(|config| async move {
            // By here iocInit has already run `setup_cp_links` and the
            // external-link wait. Poll the holder until the warm's monitor
            // event drives VAL to the source's 5.0 — with NO explicit
            // process call anywhere in this test.
            let deadline = std::time::Instant::now() + budget::FACT_BUDGET;
            loop {
                let v = {
                    let rec = config
                        .db
                        .get_record("CALINK:SEAM:HOLDER")
                        .expect("holder loaded via dbLoadRecords");
                    let inst = rec.read();
                    inst.record.val().and_then(|v| v.to_f64())
                };
                if v == Some(5.0) {
                    return Ok(());
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "CP holder VAL must reach the warmed source value 5.0 via \
                     the IocApplication::run seam (got {v:?}) — the installer \
                     fired after setup_cp_links, or not at all"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;
    result.expect("IOC run returns Ok once the CP holder warmed");
}

#[path = "common/budget.rs"]
mod budget;
