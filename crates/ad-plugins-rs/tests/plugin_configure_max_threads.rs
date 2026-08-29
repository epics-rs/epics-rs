//! `maxThreads` is a configure-line argument and nothing else.
//!
//! C fixes the per-plugin thread ceiling in `NDPluginDriver`'s constructor —
//! floored at `:117`, published at `:158` — and never moves it again:
//! `writeInt32` has no `MaxThreads` arm, and `MaxThreads_RBV` is a `longin`
//! with no output partner (NDPluginBase.template:290-295). So the `maxThreads`
//! argument of the `*Configure` line is the only route an operator has to it,
//! and `NumThreads` is clamped to it on every write
//! (`createCallbackThreads`, NDPluginDriver.cpp:955-971).
//!
//! Unfixed, every one of our configure commands parsed that argument and threw
//! it away, so a C st.cmd asking for four threads got a plugin pinned at one
//! and no diagnostic — `NumThreads` writes silently clamped back to 1 and the
//! whole callback pool was unreachable from a C-compatible startup script.
// `ad_plugins_rs::ioc` mounts on the tokio-backend-only IOC runners, so
// this file is gated with the module it exercises.
#![cfg(tokio_backend)]
#![cfg(feature = "ioc")]

use std::sync::Arc;

use ad_core_rs::ioc::GenericDriverContext;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::channel::NDArrayOutput;
use ad_plugins_rs::ioc::AdIoc;
use asyn_rs::port::DrvUserRequest;
use asyn_rs::port_handle::PortHandle;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::CommandOutcome;

/// An `AdIoc` whose plugin commands can run: they all begin with `m.driver()?`,
/// which is `Err` until a detector has been configured.
fn ad_ioc_with_driver(upstream: &str) -> (AdIoc, IocShell) {
    let ioc = AdIoc::new();
    let pool = Arc::new(NDArrayPool::new(4_000_000));
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

fn run(shell: &IocShell, line: &str) {
    match shell.execute_line(line) {
        Ok(CommandOutcome::Continue) => {}
        Ok(CommandOutcome::Failed) => panic!("st.cmd line `{line}` failed"),
        Ok(CommandOutcome::Exit) => panic!("st.cmd line `{line}` ended the shell"),
        Err(e) => panic!("st.cmd line `{line}` failed: {e}"),
    }
}

/// The reason a record's `@asyn(PORT,ADDR)DRV_INFO` link resolves to — the
/// same bind `MaxThreads_RBV` performs at iocInit.
fn reason(port: &PortHandle, drv_info: &str) -> usize {
    port.drv_user_create_blocking(&DrvUserRequest::new(drv_info, 0))
        .unwrap_or_else(|e| panic!("{drv_info} does not resolve on {}: {e}", port.port_name()))
        .reason
}

fn plugin_port(name: &str) -> PortHandle {
    asyn_rs::registry::get_port(name)
        .unwrap_or_else(|| panic!("{name} was never registered"))
        .handle
}

/// `NDROIConfigure`'s `maxThreads` sits at C argument 9
/// (NDPluginROI.cpp:358-361), the layout eight of our ten commands share.
#[test]
fn configure_line_max_threads_reaches_the_plugin() {
    let (_ioc, shell) = ad_ioc_with_driver("MTUP1");
    run(
        &shell,
        r#"NDROIConfigure("MTROI1", 20, 0, "MTUP1", 0, 0, 0, 0, 0, 4)"#,
    );

    let port = plugin_port("MTROI1");
    assert_eq!(
        port.read_int32_blocking(reason(&port, "MAX_THREADS"), 0)
            .unwrap(),
        4,
        "MaxThreads_RBV must report the ceiling the configure line asked for"
    );

    // The ceiling is only worth publishing if it is the one NumThreads is
    // clamped against (C `createCallbackThreads`, NDPluginDriver.cpp:955-971).
    let num_threads = reason(&port, "NUM_THREADS");
    port.write_int32_blocking(num_threads, 0, 4).unwrap();
    assert_eq!(
        port.read_int32_blocking(num_threads, 0).unwrap(),
        4,
        "a NumThreads write inside the configured ceiling must be accepted"
    );
}

/// `NDROIStatConfigure` inserts `maxROIs` at argument 5
/// (NDPluginROIStat.cpp:590-593), so its `maxThreads` is argument 10. Reading
/// index 9 there would take `stackSize` for a thread count.
#[test]
fn roi_stat_max_threads_is_argument_ten() {
    let (_ioc, shell) = ad_ioc_with_driver("MTUP2");
    run(
        &shell,
        r#"NDROIStatConfigure("MTRS1", 20, 0, "MTUP2", 0, 8, 0, 0, 0, 40000, 3)"#,
    );

    let port = plugin_port("MTRS1");
    assert_eq!(
        port.read_int32_blocking(reason(&port, "MAX_THREADS"), 0)
            .unwrap(),
        3,
        "maxThreads came from the stackSize slot"
    );
}

/// C floors the argument (`if (maxThreads < 1) maxThreads = 1`,
/// NDPluginDriver.cpp:117), so an omitted or non-positive one leaves the
/// plugin single-threaded rather than pinning it at 0 threads.
#[test]
fn omitted_max_threads_leaves_the_c_default() {
    let (_ioc, shell) = ad_ioc_with_driver("MTUP3");
    run(&shell, r#"NDStatsConfigure("MTST1", 20, 0, "MTUP3", 0)"#);
    run(
        &shell,
        r#"NDStdArraysConfigure("MTSA1", 20, 0, "MTUP3", 0, 0, 0, 0, 0, 0)"#,
    );

    for name in ["MTST1", "MTSA1"] {
        let port = plugin_port(name);
        assert_eq!(
            port.read_int32_blocking(reason(&port, "MAX_THREADS"), 0)
                .unwrap(),
            1,
            "{name} must fall back to C's floor of 1"
        );
    }
}

/// A command whose C signature has no `maxThreads` must not grow one from the
/// generic layout: argument 9 of `NDProcessConfigure` is `stackSize`
/// (NDPluginProcess.cpp:436-439).
#[test]
fn a_command_without_the_argument_does_not_invent_one() {
    let (_ioc, shell) = ad_ioc_with_driver("MTUP4");
    run(
        &shell,
        r#"NDProcessConfigure("MTPR1", 20, 0, "MTUP4", 0, 0, 0, 0, 40000)"#,
    );

    let port = plugin_port("MTPR1");
    assert_eq!(
        port.read_int32_blocking(reason(&port, "MAX_THREADS"), 0)
            .unwrap(),
        1,
        "a stackSize of 40000 was read as a thread ceiling"
    );
}
