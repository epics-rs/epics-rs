//! XRT Beamline IOC binary.
//!
//! Simulated beamline: Undulator → DCM Si(111) → VFM → Sample (AreaDetector)
//!
//! Motors drive xrt-rs ray tracing simulation in real time (~10 Hz).
//! The beam profile at sample position is published as an AreaDetector image.
//!
//! Usage:
//!   cargo run -p xrt-beamline --features ioc --bin xrt_ioc -- ioc/st.cmd
// On `exec_backend` this program's `main` refuses instead of running, so
// everything below is unreachable in that configuration by construction.
// The default build still lints the file in full.
#![cfg_attr(exec_backend, allow(dead_code, unused_imports))]

use std::sync::Arc;

use asyn_rs::trace::TraceManager;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::iocsh::registry::*;
use epics_ca_rs::server::ioc_app::IocApplication;

use ad_core_rs::ioc::{PluginManager, register_noop_commands};
use ad_core_rs::plugin::channel::NDArrayOutput;

use motor_rs::ioc::SimMotorHolder;

use xrt_beamline::beamline_sim::SimConfig;
use xrt_beamline::detector::{XrtDetectorRuntime, create_xrt_detector};

// ============================================================================
// Environment helpers
// ============================================================================

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_i32(name: &str, default: i32) -> i32 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

// ============================================================================
// Phase bridge: st.cmd thread → iocInit thread
// ============================================================================

struct BeamlineHolder {
    xrt_runtime: std::sync::Mutex<Option<XrtDetectorRuntime>>,
    trace: Arc<TraceManager>,
}

impl BeamlineHolder {
    fn new(trace: Arc<TraceManager>) -> Arc<Self> {
        Arc::new(Self {
            xrt_runtime: std::sync::Mutex::new(None),
            trace,
        })
    }
}

// ============================================================================
// Main
// ============================================================================

#[cfg(tokio_backend)]
#[epics_base_rs::epics_main]
async fn main() -> CaResult<()> {
    let args: Vec<String> = std::env::args().collect();

    epics_base_rs::runtime::env::set_default("XRT_BEAMLINE", env!("CARGO_MANIFEST_DIR"));
    epics_base_rs::runtime::env::set_default("ADCORE", ad_core_rs::AD_CORE_DIR);
    epics_base_rs::runtime::env::set_default("MOTOR", motor_rs::MOTOR_IOC_DIR);

    let script = if args.len() > 1 && !args[1].starts_with('-') {
        args[1].clone()
    } else {
        eprintln!("Usage: xrt_ioc <st.cmd>");
        std::process::exit(1);
    };

    let trace = Arc::new(TraceManager::new());
    let mgr = PluginManager::new(trace.clone());
    let holder = BeamlineHolder::new(trace.clone());

    let autosave_config = Arc::new(std::sync::Mutex::new(
        epics_base_rs::server::autosave::startup::AutosaveStartupConfig::new(),
    ));

    let (asyn_name, asyn_factory) = asyn_rs::asyn_record::asyn_record_factory();
    let (motor_name, motor_factory) = motor_rs::motor_record_factory();

    // The server port comes from `IocApplication::new()`, which resolves
    // EPICS_CAS_SERVER_PORT / EPICS_CA_SERVER_PORT through C
    // `envGetInetPortConfigParam` (`runtime::net::cas_server_port`).
    let mut app = IocApplication::new();
    app = app.register_record_type(asyn_name, move || asyn_factory());
    app = app.register_record_type(motor_name, move || motor_factory());
    app = app.autosave_startup(autosave_config);

    // ========================================================================
    // Startup command: xrtBeamlineConfig
    // ========================================================================
    {
        let h = holder.clone();
        let mgr_c = mgr.clone();
        app = app.register_startup_command(CommandDef::new(
            "xrtBeamlineConfig",
            vec![],
            "xrtBeamlineConfig - Configure XRT beamline simulation IOC",
            move |_args: &[ArgValue], _ctx: &CommandContext| {
                println!("xrtBeamlineConfig: setting up beamline...");

                let sim_config = SimConfig {
                    nrays: env_usize("XRT_NRAYS", 5000),
                    screen_nx: env_usize("XRT_SCREEN_NX", 128),
                    screen_nz: env_usize("XRT_SCREEN_NZ", 128),
                    screen_dx: env_f64("XRT_SCREEN_DX", 5.0),
                    screen_dz: env_f64("XRT_SCREEN_DZ", 5.0),
                    source_div_x: env_f64("XRT_SRC_DIV_X", 50e-6),
                    source_div_z: env_f64("XRT_SRC_DIV_Z", 20e-6),
                    source_size_x: env_f64("XRT_SRC_SIZE_X", 0.3),
                    source_size_z: env_f64("XRT_SRC_SIZE_Z", 0.02),
                    energy_bandwidth: env_f64("XRT_ENERGY_BW", 0.02),
                    ..Default::default()
                };

                let size_x = env_i32("XRT_SIZE_X", sim_config.screen_nx as i32);
                let size_y = env_i32("XRT_SIZE_Y", sim_config.screen_nz as i32);
                let max_mem = env_usize("XRT_MAX_MEMORY", 50_000_000);

                println!(
                    "  Sim config: {} rays, {}x{} screen, bandwidth={:.1}%",
                    sim_config.nrays,
                    sim_config.screen_nx,
                    sim_config.screen_nz,
                    sim_config.energy_bandwidth * 100.0,
                );

                let xrt_output = NDArrayOutput::new();
                let xrt_rt =
                    create_xrt_detector("XRT", size_x, size_y, max_mem, xrt_output, sim_config)
                        .map_err(|e| format!("failed to create XRT detector: {e}"))?;

                let xrt_handle = xrt_rt.port_handle().clone();
                asyn_rs::asyn_record::register_port("XRT", xrt_handle, h.trace.clone())
                    .map_err(|e| e.to_string())?;

                mgr_c.set_driver(Arc::new(ad_core_rs::ioc::GenericDriverContext::new(
                    xrt_rt.pool().clone(),
                    xrt_rt.array_output().clone(),
                    "XRT",
                    mgr_c.wiring(),
                )));

                *h.xrt_runtime.lock().unwrap() = Some(xrt_rt);
                println!("  XRT detector created");

                println!("xrtBeamlineConfig: done");
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // ========================================================================
    // areaDetector plugin commands
    // ========================================================================

    app = ad_plugins_rs::ioc::register_all_plugins(app, &mgr);
    app = register_noop_commands(app);

    // ========================================================================
    // Universal asyn device support
    // ========================================================================

    app = asyn_rs::adapter::register_asyn_device_support(app);

    // ========================================================================
    // Simulated motors
    // ========================================================================

    let motor_holder = SimMotorHolder::new();
    app = app.register_startup_command(motor_holder.sim_motor_create_command());
    app = app.register_dynamic_device_support(motor_holder.device_support_factory());

    let motor_h = motor_holder.clone();
    app = app.register_after_init(move || {
        motor_h.start_all_polling();
    });

    // ========================================================================
    // Run: execute st.cmd → iocInit → interactive shell
    // ========================================================================

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
        "xrt_ioc needs the tokio backend; this build selects the \
         reactor-free execution model (EPICS_RS_BUILD_EXEC_BACKEND=thread)."
    );
    Ok(())
}
