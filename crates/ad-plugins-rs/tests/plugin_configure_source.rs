//! A `*Configure` line's `NDArrayAddr` is the address the plugin starts on:
//! C seeds the param from it (NDPluginDriver.cpp:153) and the plugin
//! constructor connects there (e.g. NDPluginStats.cpp `connectToArrayPort`).
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

#[test]
fn configure_ndarray_addr_is_seeded_and_connected() {
    let ioc = AdIoc::new();
    ioc.mgr().set_driver(Arc::new(GenericDriverContext::new(
        DriverPool::standalone(4_000_000),
        Arc::new(parking_lot::Mutex::new(NDArrayOutput::new())),
        "CSUP",
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
    for line in [
        r#"NDROIStatConfigure("CSRS", 20, 0, "CSUP", 0, 2, 0, 0, 0, 0, 1)"#,
        r#"NDStatsConfigure("CSST", 20, 0, "CSRS", 1, 0, 0, 0, 0, 1)"#,
    ] {
        assert!(
            matches!(shell.execute_line(line), Ok(CommandOutcome::Continue)),
            "`{line}` failed"
        );
    }

    let port = asyn_rs::registry::get_port("CSST").expect("CSST").handle;
    let reason = port
        .drv_user_create_blocking(&DrvUserRequest::new("NDARRAY_ADDR", 0))
        .unwrap()
        .reason;
    assert_eq!(port.read_int32_blocking(reason, 0).unwrap(), 1);
    let wiring = ioc.mgr().wiring();
    let at_addr1 = wiring.output_at("CSRS", 1).expect("CSRS:1");
    assert_eq!(at_addr1.lock().num_senders(), 1);
}
