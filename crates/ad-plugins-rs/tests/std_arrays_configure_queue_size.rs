//! `NDStdArraysConfigure`'s queueSize reaches the plugin's queue as in C,
//! where `NDPluginStdArrays` hands it to `NDPluginDriver`
//! (NDPluginStdArrays.cpp:321).
#![cfg(tokio_backend)]
#![cfg(feature = "ioc")]

use std::sync::Arc;

use ad_core_rs::ioc::GenericDriverContext;
use ad_core_rs::ndarray_pool::DriverPool;
use ad_core_rs::plugin::channel::NDArrayOutput;
use ad_plugins_rs::ioc::AdIoc;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::CommandOutcome;

#[test]
fn the_configured_queue_size_is_the_ports() {
    let ioc = AdIoc::new();
    let pool = DriverPool::standalone(4_000_000);
    let output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
    ioc.mgr().set_driver(Arc::new(GenericDriverContext::new(
        pool,
        output,
        "QS_DET",
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

    let line = r#"NDStdArraysConfigure("QS_ARR", 20, 0, "QS_DET", 0)"#;
    assert!(matches!(
        shell.execute_line(line),
        Ok(CommandOutcome::Continue)
    ));
    let plugin = ioc.mgr().plugin("QS_ARR").expect("the plugin");
    let ph = plugin.port_runtime().port_handle();
    assert_eq!(
        ph.read_int32_blocking(plugin.plugin_params.queue_size, 0)
            .unwrap(),
        20
    );
    assert_eq!(
        ph.read_int32_blocking(plugin.plugin_params.queue_use, 0)
            .unwrap(),
        20
    );
}
