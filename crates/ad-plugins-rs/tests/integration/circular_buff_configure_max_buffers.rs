//! `NDCircularBuffConfigure`'s `maxBuffers`, not its `queueSize`, bounds the
//! pre-count: C stores the argument as `maxBuffers_`
//! (NDPluginCircularBuff.cpp:407, :466) and refuses `preCount > maxBuffers_ - 1`
//! without storing it (:284-286).
// `ad_plugins_rs::ioc` mounts on the tokio-backend-only IOC runners, so
// this file is gated with the module it exercises.
#![cfg(tokio_backend)]
#![cfg(feature = "ioc")]

use std::sync::Arc;

use ad_core_rs::ioc::GenericDriverContext;
use ad_core_rs::ndarray_pool::DriverPool;
use ad_core_rs::plugin::channel::NDArrayOutput;
use ad_plugins_rs::ioc::AdIoc;
use asyn_rs::port::DrvUserRequest;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::CommandOutcome;

fn ad_ioc_with_driver(upstream: &str) -> (AdIoc, IocShell) {
    let ioc = AdIoc::new();
    let pool = DriverPool::standalone(4_000_000);
    let output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
    ioc.mgr().set_driver(Arc::new(GenericDriverContext::new(
        pool,
        output,
        upstream,
        ioc.mgr().wiring(),
    )));

    let rt = tokio::runtime::Runtime::new().unwrap();
    let db = Arc::new(PvDatabase::new());
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    // The shell's command handlers block on this runtime; it must outlive them.
    std::mem::forget(rt);

    let shell = IocShell::new(db, bridge);
    for def in ioc.app().startup_commands() {
        shell.register(def.clone());
    }
    (ioc, shell)
}

/// Configure `line`, then write each pre-count in turn and return what
/// `PreCount` reads back after each.
fn pre_counts(shell: &IocShell, port: &str, line: &str, writes: &[i32]) -> Vec<i32> {
    match shell.execute_line(line) {
        Ok(CommandOutcome::Continue) => {}
        Ok(_) => panic!("`{line}` did not continue"),
        Err(e) => panic!("`{line}` failed: {e}"),
    }
    let handle = asyn_rs::registry::get_port(port).unwrap().handle;
    let pre = handle
        .drv_user_create_blocking(&DrvUserRequest::new("CIRC_BUFF_PRE_TRIGGER", 0))
        .unwrap()
        .reason;
    writes
        .iter()
        .map(|&value| {
            handle.write_int32_blocking(pre, 0, value).unwrap();
            handle.read_int32_blocking(pre, 0).unwrap()
        })
        .collect()
}

#[test]
fn max_buffers_bounds_the_pre_count() {
    let (_ioc, shell) = ad_ioc_with_driver("CBUP1");
    // A queue of 20 would take a pre-count of 3; maxBuffers 3 stops at 2.
    assert_eq!(
        pre_counts(
            &shell,
            "CBMB3",
            r#"NDCircularBuffConfigure("CBMB3", 20, 0, "CBUP1", 0, 3, 0)"#,
            &[2, 3],
        ),
        vec![2, 2]
    );
}

/// iocsh hands C an omitted `maxBuffers` as 0, which refuses every
/// pre-count, 0 included (`0 > -1`).
#[test]
fn an_omitted_max_buffers_refuses_any_pre_count() {
    let (_ioc, shell) = ad_ioc_with_driver("CBUP2");
    let read = pre_counts(
        &shell,
        "CBMB0",
        r#"NDCircularBuffConfigure("CBMB0", 20, 0, "CBUP2", 0)"#,
        &[5, 0],
    );
    assert_eq!(read[0], read[1], "a refused write is never stored");
    assert!(!read.contains(&5) && !read.contains(&0), "stored: {read:?}");
}
