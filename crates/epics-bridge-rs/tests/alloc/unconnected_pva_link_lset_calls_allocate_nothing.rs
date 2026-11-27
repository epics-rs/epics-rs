//! The per-link calls a record cycle makes on the pva lset allocate nothing
//! on the record thread for a link that never connects, on every config
//! route. A link's config is decided once, when its key first resolves, and
//! shared with every later call, so no call builds, parses or clones one.
//! Counted per thread with the alloc binary's allocator on an IOC thread.
#![cfg(feature = "pvalink")]

use std::sync::Arc;

use alloc_count::thread;
use epics_base_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};
use epics_base_rs::server::database::{LinkForward, LinkRef, LinkSet, PvDatabase};
use epics_base_rs::server::record::{JlinkValue, OpenLatch, pvajson_identity_key};
use epics_bridge_rs::pvalink::install_pvalink_resolver;
use serial_test::serial;

const CALLS: usize = 100;

/// Allocations `CALLS` runs of `calls` make on this thread, after one run
/// that decides every config it touches.
fn allocs(mut calls: impl FnMut()) -> u64 {
    calls();
    let ((), counts) = thread::counted(|| {
        for _ in 0..CALLS {
            calls();
        }
    });
    counts.allocs
}

/// Every per-link call a cycle makes on a link that never connects.
fn unconnected_calls(lset: &dyn LinkSet, name: LinkRef<'_>, forward: &dyn LinkForward) {
    assert!(!lset.is_connected(name));
    assert!(lset.get_cached_value(name).is_none());
    let _ = lset.put_admission(name);
    assert!(lset.alarm_message(name).is_none());
    assert!(lset.alarm_severity(name).is_none());
    assert!(lset.alarm_status(name).is_none());
    assert!(lset.remote_alarm(name).is_none());
    assert!(lset.time_stamp(name).is_none());
    let _ = lset.read_time_stamp(name);
    assert!(lset.link_metadata(name).is_none());
    let _ = forward.scan_forward();
}

// RTEMS-EXEC-MODEL-ALLOW(1): checked - runs and passes in the exec-backend
// build (EPICS_RS_BUILD_EXEC_BACKEND=thread); the runtime only drives setup.
#[test]
#[serial(epics_env)]
fn unconnected_pva_link_lset_calls_allocate_nothing() {
    let closed = named_port::HeldPort::closed_tcp();
    // SAFETY: every test of this binary that sets the PVA env is
    // `#[serial(epics_env)]`, and this one sets it before its runtime exists.
    // Nothing listens on `closed.port`, and the hold keeps a stranger from
    // binding it until the test ends.
    unsafe {
        std::env::set_var(
            "EPICS_PVA_NAME_SERVERS",
            format!("127.0.0.1:{}", closed.port),
        );
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
                let json = pvajson_identity_key("T:NONE", &[("Q".to_string(), JlinkValue::Int(7))]);
                for name in [
                    "T:NONE",
                    "T:NONE?sevr=MS&field=value",
                    "T:NONE?time=true",
                    json.as_str(),
                ] {
                    let open = OpenLatch::default();
                    let link = LinkRef::new(name.as_bytes(), &open);
                    resolver.connect_link(link, None).await;
                    let forward = resolver.forward_handle(link);
                    let n = allocs(|| unconnected_calls(&resolver, link, &*forward));
                    assert_eq!(n, 0, "{name}");
                }
            });
        });
    if let Err(panic) = worker.join() {
        std::panic::resume_unwind(panic);
    }
}
