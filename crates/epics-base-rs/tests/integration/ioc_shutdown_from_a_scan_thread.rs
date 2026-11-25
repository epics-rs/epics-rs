//! `iocShutdown` reached from a record a `scan-%g` thread is processing — an
//! `epicsExit` there runs the exit list on that thread. The shutdown joins
//! the scan threads (`dbScan.c:173-176`) and must not wait on its own.

use epics_base_rs::server::ioc_app::ioc_shutdown;
use epics_base_rs::server::ioc_builder::IocBuilder;
use std::collections::HashMap;
use std::sync::{Mutex, mpsc};

#[epics_macros_rs::epics_test]
async fn a_shutdown_from_a_periodic_record_returns() {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(Some(tx));
    let ioc = IocBuilder::new()
        .db_string(
            r#"record(sub, "QUIT") { field(SCAN, ".1 second") field(SNAM, "quit") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .register_subroutine("quit", move |_| {
            if let Some(tx) = tx.lock().unwrap().take() {
                tx.send(ioc_shutdown()).unwrap();
            }
            Ok(0)
        })
        .build_ioc()
        .await
        .unwrap();
    assert!(ioc.run().await, "a built IOC runs");
    // Blocking here is sound: a sweep needs no runtime.
    let status = rx.recv_timeout(fact_budget::FACT_BUDGET);
    assert_eq!(status, Ok(0), "iocShutdown on the scan thread hung");
}
