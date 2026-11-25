//! A record whose pva link is down reads or refuses from state on every
//! cycle, as pvxs `pvaGetValue`/`pvaPutValue` return on `!self->valid()`
//! (`pvalink_lset.cpp:263-268`) without building anything. Counted with the
//! bridge's own pvalink lset on a primed IOC thread.
#![cfg(feature = "pvalink")]

// RTEMS-EXEC-MODEL-ALLOW(1): checked - runs and passes in the exec-backend
// suite.

use std::collections::HashMap;
use std::sync::Arc;

use alloc_count::thread;
use epics_base_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = r#"
record(ai, "PL:AI") { field(INP, {pva:"PL:NOPE"}) }
record(calc, "PL:CALC") { field(INPA, {pva:"PL:NOPE"}) field(CALC, "A") }
record(ao, "PL:AO") { field(OUT, {pva:"PL:NOPE"}) }
"#;

/// Allocations 100 cycles of `rec` make on a primed scan thread.
fn allocs(db: Arc<PvDatabase>, rec: &'static str) -> u64 {
    let (tx, rx) = std::sync::mpsc::channel();
    MandatoryThread::new("scan-1", ThreadPriority::ScanLow, StackSizeClass::Big).spawn(move || {
        for _ in 0..3 {
            db.process_record_with_links_already_locked(rec, &mut ProcStack::new())
                .or_else(epics_base_rs::error::CaError::unless_process_status)
                .unwrap();
        }
        let mut visited = ProcStack::new();
        let ((), counts) = thread::counted(|| {
            for _ in 0..100 {
                db.process_record_with_links_already_locked(rec, &mut visited)
                    .or_else(epics_base_rs::error::CaError::unless_process_status)
                    .unwrap();
            }
        });
        tx.send(counts.allocs).unwrap();
    });
    rx.recv().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unconnected_pva_link_cycle_allocates_nothing() {
    let (db, _) = IocBuilder::new()
        .register_link_type("pva", |db| async move {
            epics_bridge_rs::pvalink::install_pvalink_resolver(&db).await;
            Vec::new()
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    for rec in ["PL:AI", "PL:CALC", "PL:AO"] {
        assert_eq!(allocs(db.clone(), rec), 0, "{rec}");
    }
}
