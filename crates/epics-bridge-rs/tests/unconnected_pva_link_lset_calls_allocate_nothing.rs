//! The per-link calls a record cycle makes on the pva lset allocate nothing
//! on the record thread for a link that never connects, on every config
//! route. A link's config is decided once, when its key first resolves, and
//! shared with every later call, so no call builds, parses or clones one.
//! Counted with this binary's own allocator on an IOC thread.
#![cfg(feature = "pvalink")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use epics_base_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};
use epics_base_rs::server::database::{LinkForward, LinkSet, PvDatabase};
use epics_base_rs::server::record::{JlinkValue, pvajson_identity_key};
use epics_bridge_rs::pvalink::install_pvalink_resolver;

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

const CALLS: usize = 100;

/// Allocations `CALLS` runs of `calls` make on this thread, after one run
/// that decides every config it touches.
fn allocs(mut calls: impl FnMut()) -> usize {
    calls();
    ALLOCS.with(|n| n.set(0));
    COUNTING.with(|c| c.set(true));
    for _ in 0..CALLS {
        calls();
    }
    COUNTING.with(|c| c.set(false));
    ALLOCS.with(Cell::get)
}

/// Every per-link call a cycle makes on a link that never connects.
fn unconnected_calls(lset: &dyn LinkSet, name: &[u8], forward: &dyn LinkForward) {
    assert!(!lset.is_connected(name));
    assert!(lset.get_cached_value(name).is_none());
    let _ = lset.put_admission(name);
    assert!(lset.alarm_message(name).is_none());
    assert!(lset.alarm_severity(name).is_none());
    assert!(lset.alarm_status(name).is_none());
    assert!(lset.remote_alarm(name).is_none());
    assert!(lset.time_stamp(name).is_none());
    assert!(lset.link_metadata(name).is_none());
    let _ = forward.scan_forward();
}

// RTEMS-EXEC-MODEL-ALLOW(1): checked - runs and passes in the exec-backend
// build (EPICS_RS_BUILD_EXEC_BACKEND=thread); the runtime only drives setup.
#[test]
fn unconnected_pva_link_lset_calls_allocate_nothing() {
    let port = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    // SAFETY: the one test in this binary, before any runtime exists, so no
    // other thread reads the environment. Nothing listens on `port`.
    unsafe {
        std::env::set_var("EPICS_PVA_NAME_SERVERS", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
    }

    let worker = MandatoryThread::new("scan-1", ThreadPriority::ScanLow, StackSizeClass::Big)
        .spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let db = Arc::new(PvDatabase::new());
                let resolver = install_pvalink_resolver(&db).await;
                // One name per config route: the defaults, a convenience URI
                // (one naming a `field`, so a clone of it would allocate) and a
                // structured key.
                let json = format!(
                    "pva://{}",
                    pvajson_identity_key("T:NONE", &[("Q".to_string(), JlinkValue::Int(7))])
                );
                for name in [
                    "pva://T:NONE",
                    "pva://T:NONE?sevr=MS&field=value",
                    json.as_str(),
                ] {
                    resolver.connect_link(name.as_bytes(), None).await;
                    let forward = resolver.forward_handle(name.as_bytes());
                    let n = allocs(|| unconnected_calls(&resolver, name.as_bytes(), &*forward));
                    assert_eq!(n, 0, "{name}");
                }
            });
        });
    if let Err(panic) = worker.join() {
        std::panic::resume_unwind(panic);
    }
}
