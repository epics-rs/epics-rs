//! The per-link calls a record cycle makes on the pva lset allocate nothing
//! on the record thread for a connected link, on every config route: the
//! config is shared, never cloned, and the channel lookup borrows its key.
//! Counted with this binary's own allocator on an IOC thread, while a real
//! PVA server serves the links.
//!
//! Tokio only: the PVA server is.
#![cfg(all(feature = "pvalink", tokio_backend))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};
use epics_base_rs::server::database::{LinkRef, LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{JlinkValue, OpenLatch, pvajson_identity_key};
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

/// Every per-link call a cycle makes on a connected link, by name.
const CONNECTED_CALLS: &[(&str, fn(&dyn LinkSet, LinkRef<'_>))] = &[
    ("is_connected", |l, n| assert!(l.is_connected(n))),
    ("get_cached_value", |l, n| {
        assert!(l.get_cached_value(n).is_some())
    }),
    ("put_admission", |l, n| {
        let _ = l.put_admission(n);
    }),
    ("alarm_status", |l, n| {
        let _ = l.alarm_status(n);
    }),
    ("alarm_severity", |l, n| {
        let _ = l.alarm_severity(n);
    }),
    ("alarm_message", |l, n| {
        let _ = l.alarm_message(n);
    }),
    ("remote_alarm", |l, n| {
        let _ = l.remote_alarm(n);
    }),
    ("time_stamp", |l, n| {
        let _ = l.time_stamp(n);
    }),
    ("read_time_stamp", |l, n| {
        let _ = l.read_time_stamp(n);
    }),
    ("link_metadata", |l, n| {
        assert!(l.link_metadata(n).is_some())
    }),
];

#[test]
fn connected_pva_link_lset_calls_allocate_nothing() {
    // The target IOC runs on its own runtime, off the counted thread.
    let server_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let server = server_rt.block_on(async move {
        let (remote, _) = IocBuilder::new()
            .db_string(
                r#"
                record(ai, "T:A") { field(VAL, "1.5") }
                record(calc, "T:H") { field(CALC, "5") }
                "#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        PvaServer::start(
            Arc::new(PvDatabaseSource::new(remote)),
            PvaServerConfig::isolated(),
        )
        .expect("target PVA server starts")
    });
    // SAFETY: before the PVA client exists; the server's threads read the
    // environment only through `std::env`.
    unsafe {
        std::env::set_var("EPICS_PVA_NAME_SERVERS", server.tcp_addr().to_string());
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
                let json = pvajson_identity_key("T:A", &[("Q".to_string(), JlinkValue::Int(7))]);
                // `T:H` is never processed, so it serves its UDF alarm and
                // message: the latch and the MS gate run on every read.
                for name in [
                    "T:A",
                    "T:A?sevr=MS&field=value",
                    "T:A?time=true",
                    json.as_str(),
                    "T:H?sevr=MS",
                ] {
                    let open = OpenLatch::default();
                    let link = LinkRef::new(name.as_bytes(), &open);
                    resolver.connect_link(link, None).await;
                    let deadline = Instant::now() + Duration::from_secs(20);
                    while !(resolver.is_connected(link)
                        && resolver.get_cached_value(link).is_some())
                    {
                        assert!(Instant::now() < deadline, "{name} never connected");
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    let counts: Vec<(&str, usize)> = CONNECTED_CALLS
                        .iter()
                        .map(|(call, f)| (*call, allocs(|| f(&resolver, link))))
                        // The trait hands a remote message back as an owned
                        // `String`; that copy is the caller's.
                        .filter(|(call, _)| {
                            !(name.contains("T:H")
                                && matches!(*call, "alarm_message" | "remote_alarm"))
                        })
                        .collect();
                    assert!(counts.iter().all(|(_, n)| *n == 0), "{name}: {counts:?}");
                }
            });
        });
    if let Err(panic) = worker.join() {
        std::panic::resume_unwind(panic);
    }
}
