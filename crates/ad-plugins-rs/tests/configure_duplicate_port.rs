//! A `*Configure` whose port name is taken fails the st.cmd line: C's
//! plugin constructor throws out of `asynPortDriver` (asynPortDriver.cpp:4036-4040)
//! and iocsh marks the line errored (iocsh.cpp:1275-1279).
#![cfg(tokio_backend)]
#![cfg(feature = "ioc")]

use std::sync::Arc;

use ad_core_rs::ioc::GenericDriverContext;
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::channel::NDArrayOutput;
use ad_plugins_rs::ioc::AdIoc;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::CommandOutcome;

fn shell(ioc: &AdIoc) -> IocShell {
    let pool = NDArrayPool::new(4_000_000);
    let output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
    ioc.mgr().set_driver(Arc::new(GenericDriverContext::new(
        pool,
        output,
        "DUP_DET",
        ioc.mgr().wiring(),
    )));
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    // The shell's command handlers block on this runtime; it must outlive them.
    std::mem::forget(rt);
    let shell = IocShell::new(Arc::new(PvDatabase::new()), bridge);
    for def in ioc.app().startup_commands() {
        shell.register(def.clone());
    }
    shell
}

#[test]
fn a_second_configure_of_a_port_fails_the_line() {
    let ioc = AdIoc::new();
    let shell = shell(&ioc);

    for line in [
        r#"NDStatsConfigure("DUP_STATS", 20, 0, "DUP_DET", 0)"#,
        r#"NDStdArraysConfigure("DUP_ARR", 20, 0, "DUP_DET", 0)"#,
        r#"NDOverlayConfigure("DUP_OVER", 20, 0, "DUP_DET", 0, 8)"#,
        r#"NDGatherConfigure("DUP_GATHER", 20, 0, 4)"#,
        r#"NDFFTConfigure("DUP_FFT", 20, 0, "DUP_DET", 0)"#,
        r#"NDTimeSeriesConfigure("DUP_TS", 20, 0, "DUP_DET", 0)"#,
        r#"NDPvaConfigure("DUP_PVA", 20, 0, "DUP_DET", 0, "DUP:Pva")"#,
    ] {
        assert!(
            matches!(shell.execute_line(line), Ok(CommandOutcome::Continue)),
            "first `{line}`"
        );
        assert!(
            matches!(shell.execute_line(line), Ok(CommandOutcome::Failed)),
            "second `{line}` must fail the line"
        );
    }
}

/// The refused plugin is never built, so the name's wiring output stays the
/// real plugin's: a downstream plugin configured after the duplicate still
/// receives arrays.
#[test]
fn a_refused_configure_leaves_the_wiring_output_alone() {
    let ioc = AdIoc::new();
    let shell = shell(&ioc);
    let line = r#"NDStatsConfigure("WIRE_STATS", 20, 0, "DUP_DET", 0)"#;
    assert!(matches!(
        shell.execute_line(line),
        Ok(CommandOutcome::Continue)
    ));
    let real = ioc.mgr().plugin("WIRE_STATS").expect("the first plugin");
    assert!(matches!(
        shell.execute_line(line),
        Ok(CommandOutcome::Failed)
    ));

    let wired = ioc
        .mgr()
        .wiring()
        .lookup_output("WIRE_STATS")
        .expect("the port's output");
    assert!(Arc::ptr_eq(&wired, real.array_output()));
}

/// A refused duplicate NDTimeSeriesConfigure, or a refused duplicate of its
/// source, leaves the real TS port subscribed to the source's address 1.
#[test]
fn a_refused_duplicate_keeps_the_time_series_routing() {
    let ioc = AdIoc::new();
    let shell = shell(&ioc);
    let stats = r#"NDStatsConfigure("TSR_STATS", 20, 0, "DUP_DET", 0)"#;
    let ts = r#"NDTimeSeriesConfigure("TSR_TS", 20, 0, "TSR_STATS", 1, 23)"#;
    for line in [stats, ts] {
        assert!(matches!(
            shell.execute_line(line),
            Ok(CommandOutcome::Continue)
        ));
    }
    let real = ioc.mgr().plugin("TSR_STATS").expect("the first plugin");
    for line in [ts, stats] {
        assert!(matches!(
            shell.execute_line(line),
            Ok(CommandOutcome::Failed)
        ));
    }
    let wiring = ioc.mgr().wiring();
    let at1 = wiring
        .lookup_output(&wiring.key("TSR_STATS", 1))
        .expect("TSR_STATS:1");
    assert_eq!(at1.lock().num_senders(), 1);
    let at0 = wiring.lookup_output("TSR_STATS").expect("TSR_STATS");
    assert!(Arc::ptr_eq(&at0, real.array_output()));
    assert_eq!(at0.lock().num_senders(), 0);
}
