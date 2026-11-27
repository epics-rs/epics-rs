//! A connected `pva` forward link — an FLNK or a fanout `LNKn` — is
//! dispatched through the handle the resolver opened it with, so the
//! record-processing thread allocates nothing for it: pvxs `pvaScanForward`
//! starts the put and returns (`pvxs/ioc/pvalink_lset.cpp:672-688`). Counted
//! per thread with the alloc binary's allocator on the processing thread,
//! against the same record type with no forward, while a real PVA server
//! receives the puts.
#![cfg(all(feature = "pvalink", tokio_backend))]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloc_count::thread;
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_bridge_rs::pvalink::install_pvalink_resolver;
use epics_pva_rs::server::PvDatabaseSource;
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};
use fact_budget::FACT_BUDGET;
use serial_test::serial;

const TARGETS: &str = r#"
record(calc, "T:A") { field(CALC, "VAL+1") }
record(calc, "T:B") { field(CALC, "VAL+1") }
record(calc, "T:C") { field(CALC, "VAL+1") }
record(calc, "T:D") { field(CALC, "VAL+1") }
"#;

const SOURCES: &str = r#"
record(calc, "BARE") { field(CALC, "VAL+1") }
record(calc, "PVA") { field(CALC, "VAL+1") field(FLNK, {pva:"T:A"}) }
record(calc, "JSON") { field(CALC, "VAL+1") field(FLNK, {pva:{pv:"T:C", Q:4}}) }
record(fanout, "FNONE") {}
record(fanout, "FPVA") { field(LNK1, {pva:"T:B"}) }
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
async fn allocs(db: &PvDatabase, rec: &str) -> u64 {
    process(db, rec).await;
    let mut visited = ProcStack::new();
    let window = thread::open();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
    window.close().allocs
}

/// Process `rec` until its forward reaches `target`, which the remote server
/// processes once per put.
async fn connect(local: &PvDatabase, remote: &PvDatabase, rec: &str, target: &str) {
    let deadline = epics_base_rs::runtime::time::deadline_from_now(FACT_BUDGET);
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
#[serial(epics_env)]
fn a_connected_pva_forward_allocates_nothing_on_the_processing_thread() {
    // The target IOC runs on its own runtime, off the counted thread.
    let server_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (remote, server) = server_rt.block_on(async {
        let remote = db(TARGETS).await;
        let server = PvaServer::start(
            Arc::new(PvDatabaseSource::new(remote.clone())),
            PvaServerConfig::isolated(),
        )
        .expect("target PVA server starts");
        (remote, server)
    });
    // SAFETY: every test of this binary that sets the PVA env is
    // `#[serial(epics_env)]`; before the PVA client exists, and the server's
    // threads read the environment only through `std::env`.
    unsafe {
        std::env::set_var("EPICS_PVA_NAME_SERVERS", server.tcp_addr().to_string());
        std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
    }

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
        let deadline = epics_base_rs::runtime::time::deadline_from_now(FACT_BUDGET);
        while val(&remote, "T:A") == before {
            assert!(
                Instant::now() < deadline,
                "the staged forward never arrived"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
}
