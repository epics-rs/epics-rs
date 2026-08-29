//! Modbus IOC — demonstrates the modbus-rs driver with Channel Access.
//!
//! Usage:
//!   cargo run --release -p modbus-ioc --bin modbus_ioc --features ioc -- ioc/st.cmd
//!
//! The startup script creates an IP octet port, a Modbus driver port, and
//! loads `db/modbus.db`. A Modbus/TCP server (a real PLC or a simulator such
//! as `diagslave` / pymodbus) must be reachable at the address in `st.cmd`.
// On `exec_backend` this program's `main` refuses instead of running, so
// everything below is unreachable in that configuration by construction.
// The default build still lints the file in full.
#![cfg_attr(exec_backend, allow(dead_code, unused_imports))]

use std::sync::Arc;

use asyn_rs::trace::TraceManager;
use epics_base_rs::error::CaResult;
use epics_ca_rs::server::ioc_app::IocApplication;

#[cfg(tokio_backend)]
#[epics_base_rs::epics_main]
async fn main() -> CaResult<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let args: Vec<String> = std::env::args().collect();

    epics_base_rs::runtime::env::set_default("MODBUS_IOC", env!("CARGO_MANIFEST_DIR"));

    let script = if args.len() > 1 && !args[1].starts_with('-') {
        args[1].clone()
    } else {
        eprintln!("Usage: modbus_ioc <st.cmd>");
        std::process::exit(1);
    };

    let trace = Arc::new(TraceManager::new());
    let handle = epics_base_rs::runtime::task::runtime_handle();

    // The server port comes from `IocApplication::new()`, which resolves
    // EPICS_CAS_SERVER_PORT / EPICS_CA_SERVER_PORT through C
    // `envGetInetPortConfigParam` (`runtime::net::cas_server_port`).
    let mut app = IocApplication::new();

    // Universal asyn record device support.
    app = asyn_rs::adapter::register_asyn_device_support(app);

    // Standard asyn iocsh commands — this also registers
    // `drvAsynIPPortConfigure`, used to create the underlying octet port.
    let port_manager = std::sync::Arc::new(asyn_rs::manager::PortManager::new());
    app = asyn_rs::iocsh::register_asyn_commands(app, port_manager);

    // Modbus iocsh commands: modbusInterposeConfig, drvModbusAsynConfigure.
    app = modbus_rs::ioc::register_modbus_commands(app, handle, trace);

    // The runner paired with the two protocol registrars, because `casr` and
    // `pvxsr` have to answer from the script's first line and `.run(runner)`
    // reaches only the interactive shell.
    epics_bridge_rs::qsrv::run_ca_pva_qsrv_ioc_app(
        app.startup_script(&script)
            // External links resolve with zero further setup: both link sets
            // install at the base `AfterCaLinkInit` hook — before
            // `setup_cp_links` warms Passive CP holders and before the iocInit
            // external-link wait — the `ca` and `pva` link sets.
            .register_link_set_installer(epics_ca_rs::calink::calink_link_set_install)
            .register_link_set_installer(epics_bridge_rs::qsrv::pvalink_link_set_install),
    )
    .await
}

/// The `exec_backend` arm: the dual-protocol runner this IOC hands its
/// `IocApplication` to is compiled only on the tokio backend.
#[cfg(exec_backend)]
fn main() -> CaResult<()> {
    eprintln!(
        "modbus_ioc needs the tokio backend; this build selects the \
         reactor-free execution model (EPICS_RS_BUILD_EXEC_BACKEND=thread)."
    );
    Ok(())
}
