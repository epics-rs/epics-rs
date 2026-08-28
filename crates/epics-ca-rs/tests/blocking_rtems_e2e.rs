//! Host end-to-end proof of the RTEMS-model blocking CA server front-end.
//!
//! This boots a **real record IOC** on [`BlockingCaServer`] — the `std::net`
//! thread-per-client driver that exists so the CA server runs on RTEMS — and
//! drives it with **epics-ca-rs's own async CA client** over real `127.0.0.1`
//! sockets, asserting the full request/reply and monitor paths end-to-end:
//! `caget`, `camonitor` (initial value + a post-`caput` update), and a
//! `caput_callback` (`CA_PROTO_WRITE_NOTIFY`) that round-trips to completion.
//!
//! # What execution model this proves (and what it does not)
//!
//! The **server side** runs entirely on dedicated `std::thread`s:
//!
//! * the TCP accept loop ([`BlockingCaServer::serve`]),
//! * one blocking thread per accepted client (C `camsgtask`), and
//! * the per-client event-task thread (C `dbEvent` `event_task`),
//! * the UDP name-search responder ([`BlockingCaServer::serve_udp_search`]).
//!
//! None of these threads enters a tokio runtime, so every shared handler
//! (`dispatch_message`, `register_subscription`, `serve_write_head`,
//! `run_event_task`) is driven by `block_on_sync` → `park_on`: the thread
//! polls the handler future and parks between polls, waking on the
//! runtime-agnostic `tokio::sync` locks another thread releases. The driver
//! issues **zero `runtime::task::spawn`** (a static guard test in
//! `server::blocking` even forbids the `tokio::spawn`/`tokio::net`/`tokio::time`
//! literals in that file). This is exactly the RTEMS I/O + dispatch execution
//! model — proven here to serve real CA clients on the host.
//!
//! The **client side** is the ordinary async `CaClient` (tokio `UdpSocket` for
//! search, tokio `TcpStream` for the circuit). It is the *test driver*, not the
//! artifact under test — the RTEMS deployment ships the server, not this client.
//!
//! ## What the background executor does here
//!
//! The **background executor** (callback pool + delayed timer + scanOnce
//! worker — `epics_base_rs::runtime::background`) is a *separate* facility from
//! the blocking front-end. It drives **asynchronous** record-processing
//! completion — the deferred half of a `WRITE_NOTIFY` on an async record, SDLY
//! re-processing, put-callback completion.
//!
//! Which executor the `runtime::task::spawn` / `sleep` / `interval` seam
//! routes to is chosen by **`EPICS_RS_BUILD_EXEC_BACKEND=thread`**, not by the
//! target: `epics-libcom-rs/build.rs` emits `cfg(exec_backend)` when it is set
//! and `cfg(tokio_backend)` when it is not, and `task.rs` carries one impl per
//! cfg. A hosted `EPICS_RS_BUILD_EXEC_BACKEND=thread` build therefore runs the
//! same executor RTEMS does, which is why this file is in the exec-backend
//! suite (see the census marker below) rather than gated out of it.
//! `background_init()` is an ordinary ungated `pub fn` and
//! `IocApplication::run` calls it on every backend (`ioc_app.rs`, C
//! `callbackInit` parity), so the executor exists here on both.
//!
//! What this test does *not* reach is the asynchronous completion path, and
//! that is a property of the records it loads rather than of any cfg: `ao` and
//! `longout` on Soft Channel with a passive scan complete their `WRITE_NOTIFY`
//! *synchronously on the client thread via `park_on`*, so there is no deferred
//! tail for either executor to drive. The async half is covered by
//! `async_write_notify_rtems_exec.rs` — `#![cfg(exec_backend)]`,
//! a `calcout` with `ODLY` that really does defer, and its own
//! `background_init()` call.

#![cfg(feature = "client-core")]

// RTEMS-EXEC-MODEL-ALLOW(1): measured, not argued — all 1 case(s) here run and
// pass under `EPICS_RS_BUILD_EXEC_BACKEND=thread`. The file-level gate removed
// with this marker asserted a reactor panic the exec backend does not produce,
// and while it stood the exec-backend suite could not see this file at all.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::thread;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::client::{CaClient, CaClientConfig};
use epics_ca_rs::server::blocking::{BlockingCaServer, bind_udp_search};
use serial_test::serial;

/// Load a small **real** record database (dbLoadRecords-style, then `iocInit`),
/// exactly as the async `CaServer` builder does — the resulting `PvDatabase`
/// carries genuine `ao` / `longout` records, not synthetic `SimplePv`s.
async fn build_real_db() -> Arc<PvDatabase> {
    let (db, _autosave) = IocBuilder::new()
        .db_string(
            "record(ao, \"E2E:AO\") { field(VAL, \"1.5\") }\n\
             record(longout, \"E2E:LO\") { field(VAL, \"7\") }\n",
            &HashMap::new(),
        )
        .expect("load db string")
        .build()
        .await
        .expect("iocInit");
    db
}

/// End-to-end: a real IOC on the blocking (RTEMS-model) front-end serves the
/// real epics-ca-rs client over real sockets — `caget`, `camonitor`
/// (initial + post-`caput` update), and `caput_callback` (`WRITE_NOTIFY`).
///
/// `#[serial(epics_env)]` because the client reads its address list from the
/// process-wide `EPICS_CA_*` env, shared with every other env-touching test.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn blocking_rtems_server_serves_real_ca_client_end_to_end() {
    // (1) Real record database.
    let db = build_real_db().await;

    // (2) BlockingCaServer front-end. TCP on an ephemeral 127.0.0.1 port; the
    //     UDP name-search responder on a separate ephemeral port (never the
    //     real 5064 — workspace rule). Both served on dedicated std::threads,
    //     so `block_on_sync` inside them selects `park_on` (no tokio runtime is
    //     entered on these threads).
    let acf = epics_base_rs::server::access_security::new_acf_cell(None);
    let server = Arc::new(BlockingCaServer::bind("127.0.0.1:0", db.clone(), acf).unwrap());
    let tcp_port = server.tcp_port();

    let udp_sock = bind_udp_search(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let udp_port = udp_sock.local_addr().unwrap().port();

    let srv_tcp = server.clone();
    let tcp_thread = thread::spawn(move || srv_tcp.serve());
    let srv_udp = server.clone();
    let udp_thread = thread::spawn(move || srv_udp.serve_udp_search(udp_sock));

    // (3) Real epics-ca-rs client, pinned to the responder's UDP port (the
    //     search reply carries `tcp_port` for the circuit). Same env-pinning
    //     pattern the async client/server integration tests use.
    //
    // SAFETY: the env-touching tests are `#[serial(epics_env)]`, so no other
    // thread reads/writes these concurrently.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{udp_port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", udp_port.to_string());
    }
    let client = CaClient::new_with_config(CaClientConfig::default())
        .await
        .expect("CA client");

    // (a) caget a scalar from each record — search over UDP (blocking
    //     responder) then READ_NOTIFY over TCP (blocking per-client thread).
    let (_ft, ao_val) = client.caget("E2E:AO").await.expect("caget E2E:AO");
    assert_eq!(
        ao_val,
        EpicsValue::Double(1.5),
        "caget reads the seeded ao VAL over the blocking front-end (search port {udp_port}, tcp port {tcp_port})"
    );
    let (_lft, lo_val) = client.caget("E2E:LO").await.expect("caget E2E:LO");
    assert_eq!(
        lo_val,
        EpicsValue::Long(7),
        "caget reads the seeded longout VAL"
    );

    // (b) camonitor: subscribe, receive the INITIAL value, then a subsequent
    //     update after a caput changes the record. The update travels: caput's
    //     client thread processes the record and posts a monitor event → the
    //     subscription circuit's event-task thread delivers it → the client
    //     monitor receives it. All server-side steps run on park_on std::threads.
    let ch = client.create_channel("E2E:AO");
    ch.wait_connected(budget::FACT_BUDGET)
        .await
        .expect("channel connects to the blocking server");
    let mut monitor = ch.subscribe().await.expect("subscribe");

    let initial = monitor
        .recv()
        .await
        .expect("initial monitor delivery present")
        .expect("initial monitor delivery ok");
    assert_eq!(
        initial.value,
        EpicsValue::Double(1.5),
        "camonitor delivers the initial value on subscribe"
    );

    client.caput("E2E:AO", "2.5").await.expect("caput E2E:AO");

    let update = monitor
        .recv()
        .await
        .expect("post-caput monitor update present")
        .expect("post-caput monitor update ok");
    assert_eq!(
        update.value,
        EpicsValue::Double(2.5),
        "camonitor delivers the updated value after a caput"
    );

    // (c) caput_callback (CA_PROTO_WRITE_NOTIFY) round-trips to completion. For
    //     a synchronous ao the write processes on the client thread and the
    //     WRITE_NOTIFY reply is written back inline via park_on — no async
    //     re-entry, no task::spawn. A returned Ok means the server sent the
    //     completion reply; caget then confirms the value committed.
    client
        .caput_callback("E2E:AO", "3.5", 5.0)
        .await
        .expect("WRITE_NOTIFY completes");
    let (_aft, after) = client
        .caget("E2E:AO")
        .await
        .expect("caget after WRITE_NOTIFY");
    assert_eq!(
        after,
        EpicsValue::Double(3.5),
        "WRITE_NOTIFY committed the written value"
    );

    // Teardown: drop the client, stop the accept loop (which dials itself to
    // unblock) and the UDP responder (which polls the shutdown flag between
    // short-capped recv_from calls), and join both server threads.
    client.shutdown().await;
    server.shutdown();
    tcp_thread.join().expect("accept thread joins");
    udp_thread
        .join()
        .expect("udp thread joins")
        .expect("udp responder exits cleanly");
}

#[path = "common/budget.rs"]
mod budget;
