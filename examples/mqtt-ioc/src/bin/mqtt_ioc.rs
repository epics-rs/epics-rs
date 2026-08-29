//! MQTT IOC — demonstrates mqtt-rs driver with Channel Access.
//!
//! Usage:
//!   cargo run --release -p mqtt-ioc --bin mqtt_ioc -- ioc/st.cmd
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
    // Install a tracing subscriber so mqtt-rs log lines (e.g.,
    // "MQTT connection error: ...", "MQTT connected, subscribing ...")
    // actually reach stdout. Controlled via `RUST_LOG` (defaults to info).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let args: Vec<String> = std::env::args().collect();

    epics_base_rs::runtime::env::set_default("MQTT_IOC", env!("CARGO_MANIFEST_DIR"));

    let script = if args.len() > 1 && !args[1].starts_with('-') {
        args[1].clone()
    } else {
        eprintln!("Usage: mqtt_ioc <st.cmd>");
        std::process::exit(1);
    };

    let trace = Arc::new(TraceManager::new());
    let handle = epics_base_rs::runtime::task::runtime_handle();

    // The server port comes from `IocApplication::new()`, which resolves
    // EPICS_CAS_SERVER_PORT / EPICS_CA_SERVER_PORT through C
    // `envGetInetPortConfigParam` (`runtime::net::cas_server_port`).
    let mut app = IocApplication::new();

    // Register universal asyn device support
    app = asyn_rs::adapter::register_asyn_device_support(app);

    // Register MQTT iocsh commands (mqttDriverConfigure)
    app = mqtt_rs::ioc::register_mqtt_commands(app, handle, trace);

    // Register Z2M device type builders
    app = mqtt_rs::z2m::register_z2m_commands(app);

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
        "mqtt_ioc needs the tokio backend; this build selects the \
         reactor-free execution model (EPICS_RS_BUILD_EXEC_BACKEND=thread)."
    );
    Ok(())
}
