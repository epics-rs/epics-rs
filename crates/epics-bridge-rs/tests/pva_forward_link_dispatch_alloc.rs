//! A connected `pva` forward link — an FLNK or a fanout `LNKn` — is
//! dispatched through the handle the resolver opened it with, so the
//! record-processing thread allocates nothing for it: pvxs `pvaScanForward`
//! starts the put and returns (`pvxs/ioc/pvalink_lset.cpp:672-688`). Counted
//! with this binary's own allocator on the processing thread, against the same
//! record type with no forward, while a real PVA server receives the puts.
#![cfg(all(feature = "pvalink", tokio_backend))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_bridge_rs::pvalink::install_pvalink_resolver;
use epics_pva_rs::server::PvDatabaseSource;
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards every call to `System` unchanged; the counters are
// const-initialised thread-local `Cell`s, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|n| n.set(n.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|n| n.set(n.get() + 1));
        }
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const TARGETS: &str = r#"
record(calc, "T:A") { field(CALC, "VAL+1") }
record(calc, "T:B") { field(CALC, "VAL+1") }
record(calc, "T:C") { field(CALC, "VAL+1") }
record(calc, "T:D") { field(CALC, "VAL+1") }
"#;

const SOURCES: &str = r#"
record(calc, "BARE") { field(CALC, "VAL+1") }
record(calc, "PVA") { field(CALC, "VAL+1") field(FLNK, "pva://T:A") }
record(calc, "JSON") { field(CALC, "VAL+1") field(FLNK, {pva:{pv:"T:C", Q:4}}) }
record(fanout, "FNONE") {}
record(fanout, "FPVA") { field(LNK1, "pva://T:B") }
record(fanout, "FJSON") { field(LNK1, {pva:{pv:"T:D", Q:4}}) }
"#;

async fn db(text: &str) -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_link_type("pva", |_db| async { Vec::new() })
        .db_string(text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn val(db: &PvDatabase, rec: &str) -> f64 {
    match db.get_pv(format!("{rec}.VAL")).unwrap() {
        EpicsValue::Double(v) => v,
        other => panic!("{rec}.VAL is {other:?}"),
    }
}

async fn process(db: &PvDatabase, rec: &str) {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .unwrap();
}

/// Allocations one process of `rec` makes on this thread, after a warm-up.
async fn allocs(db: &PvDatabase, rec: &str) -> usize {
    process(db, rec).await;
    let mut visited = ProcStack::new();
    ALLOCS.with(|n| n.set(0));
    COUNTING.with(|c| c.set(true));
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
    COUNTING.with(|c| c.set(false));
    ALLOCS.with(Cell::get)
}

/// Process `rec` until its forward reaches `target`, which the remote server
/// processes once per put.
async fn connect(local: &PvDatabase, remote: &PvDatabase, rec: &str, target: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while val(remote, target) == 0.0 {
        assert!(
            Instant::now() < deadline,
            "{rec} never forwarded to {target}"
        );
        process(local, rec).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn a_connected_pva_forward_allocates_nothing_on_the_processing_thread() {
    let port = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    // SAFETY: the one test in this binary, before any runtime exists, so no
    // other thread reads the environment.
    unsafe {
        std::env::set_var("EPICS_PVA_NAME_SERVERS", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
    }

    // The target IOC runs on its own runtime, off the counted thread.
    let server_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (remote, _server) = server_rt.block_on(async {
        let remote = db(TARGETS).await;
        let server = PvaServer::start(
            Arc::new(PvDatabaseSource::new(remote.clone())),
            PvaServerConfig {
                tcp_port: port,
                ..PvaServerConfig::isolated()
            },
        )
        .expect("target PVA server starts");
        (remote, server)
    });

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let local = db(SOURCES).await;
        install_pvalink_resolver(&local).await;
        local.ioc_init().await;
        for (rec, target) in [
            ("PVA", "T:A"),
            ("JSON", "T:C"),
            ("FPVA", "T:B"),
            ("FJSON", "T:D"),
        ] {
            connect(&local, &remote, rec, target).await;
        }

        for (bare, forwarding) in [("BARE", ["PVA", "JSON"]), ("FNONE", ["FPVA", "FJSON"])] {
            let bare_allocs = allocs(&local, bare).await;
            for rec in forwarding {
                assert_eq!(allocs(&local, rec).await, bare_allocs, "{rec}");
            }
        }

        // The counted forwards were staged, not dropped: the worker delivers
        // them once this thread yields.
        let before = val(&remote, "T:A");
        process(&local, "PVA").await;
        let deadline = Instant::now() + Duration::from_secs(10);
        while val(&remote, "T:A") == before {
            assert!(
                Instant::now() < deadline,
                "the staged forward never arrived"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
}
