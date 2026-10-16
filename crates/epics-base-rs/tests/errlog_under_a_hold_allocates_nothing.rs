//! A refusal report under a record hold is an errlog enqueue, as C reports
//! under `dbScanLock`: `errlogVprintf` formats into the slot `errlogInitPvt`
//! allocated (`errlog.c:121-144`), and the worker hands listeners that slot.
//! So neither the poster, a thread not ok-to-block, nor the delivery to a
//! listener allocates per message. Each thread counts its own allocations.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::runtime::log::{
    errlog_add_listener, errlog_flush, errlog_remove_listener, errlog_vprintf,
    install_console_subscriber,
};
use epics_base_rs::runtime::task::{
    MandatoryThread, StackSizeClass, ThreadPriority, thread_is_ok_to_block,
};
use epics_base_rs::server::ioc_builder::IocBuilder;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|n| n.set(n.get() + 1));
        }
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The errlog worker's own count, published by the listener it runs.
static DELIVERY_ALLOCS: AtomicUsize = AtomicUsize::new(0);
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
        COUNTING.with(|c| c.set(true));
        DELIVERY_ALLOCS.store(ALLOCS.with(Cell::get), Ordering::Relaxed);
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
        let before = DELIVERY_ALLOCS.load(Ordering::Relaxed);

        let guard = rec.write();
        ALLOCS.with(|n| n.set(0));
        COUNTING.with(|c| c.set(true));
        for i in 0..1000 {
            errlog_vprintf(format_args!("R:AO: refused put {i}\n"));
        }
        COUNTING.with(|c| c.set(false));
        drop(guard);
        errlog_flush();
        tx.send((ALLOCS.with(Cell::get), before)).unwrap();
    });
    let (poster, before) = rx.recv().unwrap();
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
        DELIVERY_ALLOCS.load(Ordering::Relaxed) - before,
        0,
        "allocations on the errlog worker delivering the burst"
    );
}
