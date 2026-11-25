//! Loading `asyn.dbd` gives a C IOC the asyn shell commands along with the
//! device support: the `asynRegister` registrar (asynShellCommands.c:1349-1382)
//! runs on the dbd load. Here `register_asyn_device_support` was the dbd
//! load's device half only; an IOC that did not also call
//! `register_asyn_commands` had no `asynSetTraceMask`, and one that did, on a
//! manager it built, set masks on a trace its ports were not bound to.

use std::sync::Arc;

use asyn_rs::adapter::register_asyn_device_support;
use asyn_rs::manager::PortManager;
use asyn_rs::services::PortServices;
use epics_base_rs::server::ioc_app::IocApplication;

#[test]
fn device_support_registration_carries_the_asyn_command_set() {
    let app = register_asyn_device_support(IocApplication::new());
    let names: Vec<&str> = app
        .startup_commands()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    for expected in [
        "asynReport",
        "asynSetTraceMask",
        "asynSetTraceFile",
        "asynOctetSetInputEos",
        "drvAsynIPPortConfigure",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
}

/// The commands act on the process's port table, whose trace is the one a
/// port built with `RuntimeConfig::default()` is bound to.
#[test]
fn the_global_port_table_traces_through_the_global_services() {
    assert!(Arc::ptr_eq(
        PortManager::global().services().trace(),
        PortServices::global().trace()
    ));
    assert!(Arc::ptr_eq(&PortManager::global(), &PortManager::global()));
}
