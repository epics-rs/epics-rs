//! A refusal report under a record hold is an errlog enqueue, as C reports
//! under `dbScanLock`: `errlogVprintf` formats into the slot `errlogInitPvt`
//! allocated (`errlog.c:121-144`), and the worker hands listeners that slot.
//! So neither the poster, a thread not ok-to-block, nor the delivery to a
//! listener allocates per message. Counted per thread with the alloc binary's
//! allocator, on the poster and on the errlog worker.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use alloc_count::named::{self, Name};
use alloc_count::thread;
use epics_base_rs::runtime::log::{
    errlog_add_listener, errlog_flush, errlog_remove_listener, errlog_vprintf,
    install_console_subscriber,
};
use epics_base_rs::runtime::task::{
    MandatoryThread, StackSizeClass, ThreadPriority, thread_is_ok_to_block,
};
use epics_base_rs::server::ioc_builder::IocBuilder;

/// The errlog worker, `epicsThreadCreateOpt("errlog", …)` (`errlog.c:568`).
static WORKER: [Name; 1] = [Name::Exact("errlog")];

static DELIVERED: AtomicUsize = AtomicUsize::new(0);

#[epics_macros_rs::epics_test]
async fn a_thousand_reports_under_a_hold_allocate_nothing() {
    // An IOC's subscriber: errlog lines must not be rendered through it.
    install_console_subscriber();
    let (db, _) = IocBuilder::new()
        .db_string(r#"record(ao, "R:AO") {}"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let rec = db.get_record("R:AO").unwrap();

    let listener = errlog_add_listener(|_| {
        DELIVERED.fetch_add(1, Ordering::Relaxed);
    });

    // An IOC thread, as a scan thread is: its first contended park on the
    // queue lock must not be the one that builds its park state.
    let (tx, rx) = std::sync::mpsc::channel();
    MandatoryThread::new("scan-1", ThreadPriority::ScanLow, StackSizeClass::Big).spawn(move || {
        assert!(!thread_is_ok_to_block(), "a scan thread is not ok-to-block");
        // The callsite registers with `tracing` once, and the worker sizes its
        // listener snapshot on its first pass.
        errlog_vprintf(format_args!("warm\n"));
        errlog_flush();
        let worker = named::open(&WORKER);

        let guard = rec.write();
        let poster = thread::open();
        for i in 0..1000 {
            errlog_vprintf(format_args!("R:AO: refused put {i}\n"));
        }
        let poster = poster.close().allocs;
        drop(guard);
        errlog_flush();
        let [delivery] = worker.close().map(|c| c.allocs);
        tx.send((poster, delivery)).unwrap();
    });
    let (poster, delivery) = rx.recv().unwrap();
    errlog_remove_listener(listener);

    assert_eq!(
        poster, 0,
        "allocations on the posting thread under the hold"
    );
    assert!(
        DELIVERED.load(Ordering::Relaxed) > 1,
        "nothing was delivered"
    );
    assert_eq!(
        delivery, 0,
        "allocations on the errlog worker delivering the burst"
    );
}
