//! `blockingCallbacks` is the `NDPluginDriver` constructor argument every
//! plugin `*Configure` passes, and C publishes it at once
//! (`setIntegerParam(BlockingCallbacks, blockingCallbacks)`,
//! NDPluginDriver.cpp:160). Unfixed, every configure command parsed past it
//! and the plugin started non-blocking whatever the st.cmd asked for.
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
use asyn_rs::port_handle::PortHandle;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::CommandOutcome;

/// An `AdIoc` with a detector configured, whose output the plugins read.
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

fn run(shell: &IocShell, line: &str) {
    match shell.execute_line(line) {
        Ok(CommandOutcome::Continue) => {}
        Ok(CommandOutcome::Failed) => panic!("st.cmd line `{line}` failed"),
        Ok(CommandOutcome::Exit) => panic!("st.cmd line `{line}` ended the shell"),
        Err(e) => panic!("st.cmd line `{line}` failed: {e}"),
    }
}

/// The reason a record's `@asyn(PORT,ADDR)DRV_INFO` link resolves to — the
/// same bind `BlockingCallbacks_RBV` performs at iocInit.
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

fn blocking_callbacks(name: &str) -> i32 {
    let port = plugin_port(name);
    port.read_int32_blocking(reason(&port, "BLOCKING_CALLBACKS"), 0)
        .unwrap()
}

/// One command per configure path: the two hand-written ones, the generic
/// one, and the ones whose own argument layouts follow argument 2.
#[test]
fn configure_line_blocking_callbacks_reaches_the_plugin() {
    let (_ioc, shell) = ad_ioc_with_driver("BCUP1");
    let lines = [
        (
            "BCSA1",
            r#"NDStdArraysConfigure("BCSA1", 20, 1, "BCUP1", 0, 0)"#,
        ),
        ("BCST1", r#"NDStatsConfigure("BCST1", 20, 1, "BCUP1", 0)"#),
        ("BCPR1", r#"NDProcessConfigure("BCPR1", 20, 1, "BCUP1", 0)"#),
        (
            "BCTS1",
            r#"NDTimeSeriesConfigure("BCTS1", 20, 1, "BCUP1", 0, 8)"#,
        ),
        (
            "BCOV1",
            r#"NDOverlayConfigure("BCOV1", 20, 1, "BCUP1", 0, 8)"#,
        ),
        ("BCGA1", r#"NDGatherConfigure("BCGA1", 20, 1, 2)"#),
        // NDAttrPlotConfig carries it at argument 7 (NDPluginAttrPlot.cpp:308-310).
        (
            "BCAP1",
            r#"NDAttrPlotConfig("BCAP1", 8, 100, 4, "BCUP1", 0, 20, 1)"#,
        ),
    ];
    for (_, line) in lines {
        run(&shell, line);
    }
    for (name, _) in lines {
        assert_eq!(
            blocking_callbacks(name),
            1,
            "{name}: BlockingCallbacks_RBV must report the configure line's argument"
        );
    }
}

/// An explicit or omitted 0 leaves the plugin non-blocking.
#[test]
fn zero_or_omitted_blocking_callbacks_stays_non_blocking() {
    let (_ioc, shell) = ad_ioc_with_driver("BCUP2");
    run(&shell, r#"NDProcessConfigure("BCPR2", 20, 0, "BCUP2", 0)"#);
    run(
        &shell,
        r#"NDAttrPlotConfig("BCAP2", 8, 100, 4, "BCUP2", 0, 20)"#,
    );
    assert_eq!(blocking_callbacks("BCPR2"), 0);
    assert_eq!(blocking_callbacks("BCAP2"), 0);
}
