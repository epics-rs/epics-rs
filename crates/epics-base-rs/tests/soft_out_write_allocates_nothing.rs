//! A soft output's OUT write allocates nothing per cycle once warm: C
//! `dbPutLink(&prec->out, dbrType, &prec->oval, 1)` hands `dbPut` the
//! record's own field (`devAoSoft.c:39`, `devSoSoft.c:38`) and copies
//! nothing. Counted on the scan thread by this binary's own allocator.

#![cfg(all(exec_backend, target_os = "linux"))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;

static ARMED: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicUsize = AtomicUsize::new(0);

/// The calling thread is the 10 Hz scan thread. `pthread_getname_np` on the
/// calling thread is a `prctl`, which allocates nothing.
fn on_scan_thread() -> bool {
    let mut name = [0u8; 16];
    // SAFETY: a 16-byte buffer is the size the call is documented to fill.
    let rc = unsafe {
        libc::pthread_getname_np(libc::pthread_self(), name.as_mut_ptr().cast(), name.len())
    };
    rc == 0 && name.starts_with(b"scan-0.1\0")
}

fn count() {
    if ARMED.load(Ordering::Relaxed) && on_scan_thread() {
        CALLS.fetch_add(1, Ordering::Relaxed);
    }
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        count();
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

async fn out_writes_allocate_nothing(db_text: &str, counter: &str) {
    let db = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let ticks = || db.get_pv(counter).unwrap().to_f64().unwrap();
    let owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1000)).await;

    let before = ticks();
    ARMED.store(true, Ordering::SeqCst);
    epics_base_rs::runtime::task::sleep(Duration::from_millis(2000)).await;
    ARMED.store(false, Ordering::SeqCst);
    let ran = ticks() - before;
    drop(owner);

    assert!(ran >= 10.0, "only {ran} cycles ran in the counted window");
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        0,
        "allocations and frees on scan-0.1 over {ran} cycles"
    );
}

#[epics_macros_rs::epics_test]
async fn ao_out_write_allocates_nothing() {
    out_writes_allocate_nothing(
        r#"
record(calc, "TICK") { field(SCAN, ".1 second") field(CALC, "VAL+1") }
record(ao, "SRC") { field(SCAN, ".1 second") field(VAL, "2.5") field(OUT, "DST NPP") }
record(ao, "DST") {}
"#,
        "TICK",
    )
    .await;
}

#[epics_macros_rs::epics_test]
async fn stringout_out_write_allocates_nothing() {
    out_writes_allocate_nothing(
        r#"
record(calc, "TICK") { field(SCAN, ".1 second") field(CALC, "VAL+1") }
record(stringout, "SRC") { field(SCAN, ".1 second") field(VAL, "forty") field(OUT, "DST NPP") }
record(stringout, "DST") {}
"#,
        "TICK",
    )
    .await;
}
