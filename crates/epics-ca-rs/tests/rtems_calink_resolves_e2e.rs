//! A `ca://` record link RESOLVES end to end on the exec backend — value
//! included.
//!
//! `realtime_ca_ioc_boots.rs` proves the whole init path runs and the
//! name-server dial is *attempted* (it points the client at a closed port
//! and waits for the refusal). Nothing proved the other half: that under
//! `--features rtems-exec-model` a `ca://UPSTREAM CP` input link actually
//! reaches a live upstream, resolves through `EPICS_CA_NAME_SERVERS`, opens
//! a monitor, and lands the upstream value in the downstream record. That
//! is the difference between "the seam does not panic" and "the resolver
//! resolves".
//!
//! # Topology — two IOC halves in one process
//!
//! * **Upstream**: a real record database ([`IocBuilder`], as
//!   `blocking_real_record_e2e.rs` builds one) served by
//!   [`BlockingCaServer`] — the blocking `std::net` driver the exec backend
//!   uses — on an OS-assigned loopback port. Never 5064: `bind() ⟹
//!   listening`, and an ephemeral port cannot collide with anything.
//! * **Downstream**: a second database whose one record carries
//!   `INP = ca://UPSTREAM:VAL CP`, with the calink resolver installed and
//!   iocInit's three link phases run in `realtime-ca-ioc`'s exact order.
//!
//! `EPICS_CA_NAME_SERVERS` names the upstream server's TCP port and the UDP
//! search path is configured off (`EPICS_CA_AUTO_ADDR_LIST=NO`, empty
//! `EPICS_CA_ADDR_LIST`) — on this backend the search engine is
//! name-servers-only anyway, so a resolution can only have come over the
//! TCP name-server circuit: SEARCH over TCP, the server's
//! use-the-peer-address reply, the data-circuit dial, CREATE_CHAN,
//! EVENT_ADD, and the CP monitor processing the downstream record.
//!
//! # Why this must be feature-ON only
//!
//! With `rtems-exec-model` off, the `runtime::task` seam routes the
//! resolver's spawns to tokio and every one of them needs a reactor this
//! plain `#[test]` thread does not have. With it on, they land on the
//! background executor `background_init()` starts, and the client's
//! circuits run on `runtime::blocking_io`'s pump threads — the exact
//! configuration the RTEMS target has, which is the configuration under
//! test.
//!
//! # Synchronization
//!
//! Bounded polls against a deadline, no bare sleeps-then-assert: first for
//! the link registering in the resolver (staged by `setup_cp_links`,
//! completed on the link work owner after a real subscribe round-trip),
//! then for the upstream value appearing in the downstream record. The
//! budget is generous for a loaded CI box; the poll interval keeps the
//! failure message's timing tight.
//!
//! # Environment
//!
//! The three `EPICS_CA_*` variables are process-global, set before the
//! resolver exists because the client it lazily builds reads them once at
//! construction. This file deliberately holds a single `#[test]` so there
//! is no intra-binary concurrency under `cargo test`, and nextest runs one
//! process per test.

#![cfg(feature = "rtems-exec-model")]

use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::{background_init, block_on_sync};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_ca_rs::calink::install_calink_resolver;
use epics_ca_rs::server::blocking::BlockingCaServer;

/// The upstream IOC: one `ao` whose seeded VAL is the value that must cross.
/// 42.5 is exactly representable, so equality below is not a float gamble.
const UPSTREAM_DB: &str =
    "record(ao, \"UPSTREAM:VAL\") { field(VAL, \"42.5\") field(PREC, \"3\") }\n";

/// The downstream IOC: one Passive `ai` that can only get a value through
/// the `ca://` CP link — Passive, so nothing scans it; the CP monitor is the
/// only way it ever processes.
const DOWNSTREAM_DB: &str = concat!(
    "record(ai, \"DOWN:AI\") {\n",
    "  field(INP, \"ca://UPSTREAM:VAL CP\")\n",
    "  field(PREC, \"3\") field(SCAN, \"Passive\")\n",
    "}\n",
);

/// One budget for the whole resolution: search, dial, subscribe, first
/// monitor, record processing. Generous for a loaded CI box.
const RESOLVE_BUDGET: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

/// Load a database exactly as `realtime-ca-ioc::load_database` does — driven by
/// `block_on_sync`, which parks this thread between polls; the build future
/// awaits only in-process locks, so no reactor is involved.
fn build_db(db: &str) -> Arc<PvDatabase> {
    let (db, _autosave) = block_on_sync(
        IocBuilder::new()
            .db_string(db, &HashMap::new())
            .expect("load db string")
            .build(),
    )
    .expect("no async runtime entered on this test thread")
    .expect("iocInit");
    db
}

#[test]
fn a_ca_link_resolves_end_to_end_on_the_exec_backend() {
    // C `callbackInit` first, as every exec-model fixture does: the resolver
    // and the client spawn onto this executor.
    background_init();

    // Upstream first — its OS-assigned port is what EPICS_CA_NAME_SERVERS
    // has to name, and `bind` returning IS it listening.
    let upstream = build_db(UPSTREAM_DB);
    let server = Arc::new(
        BlockingCaServer::bind(
            "127.0.0.1:0",
            upstream,
            epics_base_rs::server::access_security::new_acf_cell(None),
        )
        .expect("bind the upstream CA server on an ephemeral loopback port"),
    );
    let addr = server.local_addr().expect("local_addr");
    let srv = server.clone();
    let accept = thread::spawn(move || srv.serve());

    // The client the resolver lazily builds reads these once at
    // construction, so they go in before the resolver exists.
    //
    // SAFETY (edition 2024): this file holds a single test, so no other
    // thread in this process reads or writes the environment concurrently
    // (the background executor's workers touch it only through the client
    // construction this test itself triggers, later).
    unsafe {
        std::env::set_var(
            "EPICS_CA_NAME_SERVERS",
            format!("127.0.0.1:{}", addr.port()),
        );
        std::env::set_var("EPICS_CA_ADDR_LIST", "");
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
    }

    // Downstream: mount the resolver, then iocInit's link phases in
    // `realtime-ca-ioc`'s order — locality first (a no-op for the `ca://`
    // spelling, kept for order fidelity), then the CP warm-up that stages
    // this link's open, then the stage for everything else.
    let downstream = build_db(DOWNSTREAM_DB);
    let resolver = block_on_sync(install_calink_resolver(&downstream))
        .expect("no async runtime entered on this test thread");
    block_on_sync(async {
        downstream.initialize_link_locality().await;
        downstream.setup_cp_links().await;
        downstream.setup_external_link_opens().await;
    })
    .expect("no async runtime entered on this test thread");

    let deadline = Instant::now() + RESOLVE_BUDGET;

    // Stage 1 — the link registers. `setup_cp_links` only STAGES the open;
    // the registration lands after a real subscribe round-trip to the
    // upstream server, so this wait is the search + dial + CREATE_CHAN +
    // EVENT_ADD leg of the proof.
    while resolver.link_count() < 1 {
        assert!(
            Instant::now() < deadline,
            "the ca:// link never registered within {RESOLVE_BUDGET:?} — \
             resolution through EPICS_CA_NAME_SERVERS={addr} did not complete \
             (report: {:?})",
            resolver.link_report(),
        );
        thread::sleep(POLL);
    }

    // Stage 2 — the upstream value lands in the downstream record: the CP
    // monitor's initial snapshot processes DOWN:AI, whose only input is the
    // link. This is the assertion the boot smoke test could not make.
    loop {
        let got = downstream.get_pv("DOWN:AI").ok().and_then(|v| v.to_f64());
        if got.is_some_and(|v| (v - 42.5).abs() < 1e-9) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "DOWN:AI never received the upstream value 42.5 within \
             {RESOLVE_BUDGET:?}; last read {got:?}, link report {:?}",
            resolver.link_report(),
        );
        thread::sleep(POLL);
    }

    server.shutdown();
    accept.join().expect("the accept thread exits on shutdown");
}
