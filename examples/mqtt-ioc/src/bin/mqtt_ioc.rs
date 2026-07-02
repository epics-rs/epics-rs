//! MQTT IOC — demonstrates mqtt-rs driver with Channel Access.
//!
//! Usage:
//!   cargo run --release -p mqtt-ioc --bin mqtt_ioc -- ioc/st.cmd

use std::sync::Arc;

use asyn_rs::trace::TraceManager;
use epics_base_rs::error::CaResult;
use epics_ca_rs::server::ioc_app::IocApplication;

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

    let mut app = IocApplication::new();

    app = app.port(
        std::env::var("EPICS_CA_SERVER_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5064),
    );

    // Register universal asyn device support
    app = asyn_rs::adapter::register_asyn_device_support(app);

    // Register MQTT iocsh commands (mqttDriverConfigure)
    app = mqtt_rs::ioc::register_mqtt_commands(app, handle, trace);

    // Register Z2M device type builders
    app = mqtt_rs::z2m::register_z2m_commands(app);

    app.startup_script(&script)
        // External links resolve with zero further setup: both link sets
        // install at the base `AfterCaLinkInit` hook — before
        // `setup_cp_links` warms Passive CP holders and before the iocInit
        // external-link wait — the `ca` and `pva` link sets.
        .register_link_set_installer(epics_ca_rs::calink::calink_link_set_install)
        .register_link_set_installer(epics_bridge_rs::qsrv::pvalink_link_set_install)
        .run(epics_bridge_rs::qsrv::run_ca_pva_qsrv_ioc)
        .await
}
