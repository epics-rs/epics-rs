//! Mini-beamline IOC binary.
//!
//! This file wires together all the simulated hardware for the mini-beamline:
//! beam current source, 3 point detectors, 1 area detector (MovingDot), 5 motors,
//! and the standard areaDetector plugin chain.
//!
//! The structure follows the C EPICS IOC pattern:
//!   1. Register iocsh commands and device support factories (Rust side)
//!   2. Execute st.cmd which calls those commands and loads .db/.template files
//!   3. iocInit wires device support to records, starts I/O Intr scanning
//!   4. Interactive iocsh shell for runtime inspection
//!
//! Usage:
//!   cargo run -p mini-beamline --features ioc --bin mini_ioc -- ioc/st.cmd
// On `exec_backend` this program's `main` refuses instead of running, so
// everything below is unreachable in that configuration by construction.
// The default build still lints the file in full.
#![cfg_attr(exec_backend, allow(dead_code, unused_imports))]

use std::collections::HashMap;
use std::sync::Arc;

use asyn_rs::trace::TraceManager;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::iocsh::registry::*;
use epics_ca_rs::server::ioc_app::IocApplication;

use ad_core_rs::ioc::{PluginManager, register_noop_commands};
use ad_core_rs::plugin::channel::NDArrayOutput;

use motor_rs::ioc::SimMotorHolder;

use mini_beamline::beam_current::ioc_support::BeamCurrentDeviceSupport;
use mini_beamline::beam_current::{self, BeamCurrentValue};
use mini_beamline::moving_dot::driver::{MovingDotRuntime, create_moving_dot_with_config};
use mini_beamline::physics::{BeamCurrentConfig, DetectorMode, MovingDotImageConfig};
use mini_beamline::point_detector::{self, PointDetectorRuntime};
use mini_beamline::random_waveform::ioc_support::RandomWaveformFactory;

// ============================================================================
// Environment helpers
// ============================================================================

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
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
//
// IOC startup has a timing problem: st.cmd runs first (Phase 1) and creates
// driver runtimes, but device support factories run later during iocInit
// (Phase 2). BeamlineHolder bridges this gap — the config command stores
// runtimes into it, and the factories read them back out.
//
// This is the Rust equivalent of the global variables that C EPICS IOCs use
// to pass driver handles from xxxConfigure() to xxxDeviceSupport::init().
// ============================================================================

struct BeamlineHolder {
    beam_value: Arc<BeamCurrentValue>,
    beam_rx: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    pd_runtimes: std::sync::Mutex<HashMap<String, PointDetectorRuntime>>,
    md_runtime: std::sync::Mutex<Option<MovingDotRuntime>>,
    trace: Arc<TraceManager>,
}

impl BeamlineHolder {
    fn new(trace: Arc<TraceManager>) -> Arc<Self> {
        Arc::new(Self {
            beam_value: Arc::new(BeamCurrentValue::new()),
            beam_rx: std::sync::Mutex::new(None),
            pd_runtimes: std::sync::Mutex::new(HashMap::new()),
            md_runtime: std::sync::Mutex::new(None),
            trace,
        })
    }
}

// ============================================================================
// DriverContext: connects MovingDot to the areaDetector plugin chain
//
// When st.cmd runs NDStdArraysConfigure/NDStatsConfigure/etc., the plugin
// manager needs to know where image data comes from (which NDArrayPool)
// and how to subscribe to new frames (connect_downstream). GenericDriverContext
// also registers the driver port in the global wiring registry so that
// plugins can rewire their NDArrayPort at runtime.
// ============================================================================

// ============================================================================
// Main
// ============================================================================

#[cfg(tokio_backend)]
#[epics_base_rs::epics_main]
async fn main() -> CaResult<()> {
    let args: Vec<String> = std::env::args().collect();

    // Set macro paths so st.cmd can resolve $(MINI_BEAMLINE)/db/..., $(ADCORE)/ioc/..., $(OPTICS)/db/...
    epics_base_rs::runtime::env::set_default("MINI_BEAMLINE", env!("CARGO_MANIFEST_DIR"));
    epics_base_rs::runtime::env::set_default("ADCORE", ad_core_rs::AD_CORE_DIR);
    epics_base_rs::runtime::env::set_default(
        "OPTICS",
        optics_rs::OPTICS_DB_DIR.trim_end_matches("/db"),
    );

    let script = if args.len() > 1 && !args[1].starts_with('-') {
        args[1].clone()
    } else {
        eprintln!("Usage: mini_ioc <st.cmd>");
        std::process::exit(1);
    };

    let trace = Arc::new(TraceManager::new());
    let mgr = PluginManager::new(trace.clone());
    let holder = BeamlineHolder::new(trace.clone());

    // Enable autosave startup commands (set_savefile_path, create_monitor_set, etc.)
    let autosave_config = Arc::new(std::sync::Mutex::new(
        epics_base_rs::server::autosave::startup::AutosaveStartupConfig::new(),
    ));

    // Register record types via injection (not global registry)
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
    // Startup command: miniBeamlineConfig
    //
    // Called from st.cmd as `miniBeamlineConfig()`. Creates all simulated
    // hardware and stores handles in BeamlineHolder for later device support
    // wiring. This runs on the st.cmd thread (std::thread, not tokio).
    //
    // Creates:
    //   - Beam current simulator thread (sine wave, configurable via env vars)
    //   - 3 point detectors (PinHole, Edge, Slit) as asyn ports
    //   - 1 MovingDot area detector as an asyn port + plugin data source
    // ========================================================================
    {
        let h = holder.clone();
        let mgr_c = mgr.clone();
        app = app.register_startup_command(CommandDef::new(
            "miniBeamlineConfig",
            vec![],
            "miniBeamlineConfig - Configure mini beamline IOC",
            move |_args: &[ArgValue], _ctx: &CommandContext| {
                println!("miniBeamlineConfig: setting up beamline...");

                // --- Beam current: background thread producing sine-wave values ---
                let beam_config = BeamCurrentConfig {
                    offset: env_f64("BEAM_OFFSET", 500.0),
                    amplitude: env_f64("BEAM_AMPLITUDE", 25.0),
                    period: env_f64("BEAM_PERIOD", 4.0),
                };
                let beam_interval = env_u64("BEAM_UPDATE_MS", 100);
                println!(
                    "  Beam config: offset={}, amp={}, period={}s, interval={}ms",
                    beam_config.offset, beam_config.amplitude, beam_config.period, beam_interval
                );

                let (_beam_jh, beam_rx) = beam_current::start_beam_current_thread(
                    h.beam_value.clone(),
                    beam_config,
                    beam_interval,
                );
                *h.beam_rx.lock().unwrap() = Some(beam_rx);
                println!("  Beam current thread started");

                // --- Point detectors: 3 asyn ports with different slit geometries ---
                let pd_configs = [
                    ("PD_PH", DetectorMode::PinHole),
                    ("PD_EDGE", DetectorMode::Edge),
                    ("PD_SLIT", DetectorMode::Slit),
                ];
                for (port, mode) in &pd_configs {
                    let rt = point_detector::create_point_detector(port, *mode)
                        .map_err(|e| format!("failed to create PointDetector {port}: {e}"))?;
                    let port_handle = rt.port_handle().clone();
                    asyn_rs::asyn_record::register_port(port, port_handle, h.trace.clone())
                        .map_err(|e| e.to_string())?;
                    h.pd_runtimes.lock().unwrap().insert(port.to_string(), rt);
                    println!("  PointDetector '{port}' created");
                }

                // --- MovingDot: 2D Gaussian area detector + plugin data source ---
                let dot_size_x = env_i32("DOT_SIZE_X", 640);
                let dot_size_y = env_i32("DOT_SIZE_Y", 480);
                let dot_max_mem = env_u64("DOT_MAX_MEMORY", 50_000_000) as usize;
                let dot_image_config = MovingDotImageConfig {
                    sigma_x: env_f64("DOT_SIGMA_X", 50.0),
                    sigma_y: env_f64("DOT_SIGMA_Y", 25.0),
                    background: env_f64("DOT_BACKGROUND", 1000.0),
                    n_per_i_per_s: env_f64("DOT_N_PER_I_PER_S", 200.0),
                };
                println!(
                    "  MovingDot config: {}x{}, sigma=({},{}), bg={}, N/I/s={}",
                    dot_size_x,
                    dot_size_y,
                    dot_image_config.sigma_x,
                    dot_image_config.sigma_y,
                    dot_image_config.background,
                    dot_image_config.n_per_i_per_s
                );

                let dot_output = NDArrayOutput::new();
                let dot_rt = create_moving_dot_with_config(
                    "DOT",
                    dot_size_x,
                    dot_size_y,
                    dot_max_mem,
                    dot_output,
                    dot_image_config,
                )
                .map_err(|e| format!("failed to create MovingDot: {e}"))?;
                let dot_handle = dot_rt.port_handle().clone();
                asyn_rs::asyn_record::register_port("DOT", dot_handle, h.trace.clone())
                    .map_err(|e| e.to_string())?;

                // Connect MovingDot as the data source for the plugin chain
                // (NDStdArraysConfigure etc. will call connect_downstream on this)
                // GenericDriverContext also registers "DOT" in the wiring registry
                // so plugins can rewire their NDArrayPort at runtime.
                mgr_c.set_driver(Arc::new(ad_core_rs::ioc::GenericDriverContext::new(
                    dot_rt.pool().clone(),
                    dot_rt.array_output().clone(),
                    "DOT",
                    mgr_c.wiring(),
                )));

                *h.md_runtime.lock().unwrap() = Some(dot_rt);
                println!("  MovingDot detector created");

                println!("miniBeamlineConfig: done");
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // ========================================================================
    // areaDetector plugin commands
    //
    // Register st.cmd commands for standard AD plugins (NDStdArraysConfigure,
    // NDStatsConfigure, NDROIConfigure, etc.) so that commonPlugins.cmd works.
    // Also register no-op stubs for commands we don't implement (e.g. set_requestfile_path).
    // ========================================================================

    app = ad_plugins_rs::ioc::register_all_plugins(app, &mgr);
    app = register_noop_commands(app);

    // ========================================================================
    // Universal asyn device support (lowest priority — registered first)
    //
    // Handles all standard asyn DTYPs (asynInt32, asynFloat64, asynOctet, etc.)
    // by parsing @asyn(PORT,ADDR,TIMEOUT)DRVINFO links and dispatching to the
    // port driver. This is the Rust equivalent of C EPICS's standard asyn
    // device support.
    // ========================================================================

    app = asyn_rs::adapter::register_asyn_device_support(app);

    // ========================================================================
    // Device support factories
    //
    // Each factory maps a DTYP string to a DeviceSupport constructor.
    // During iocInit (Phase 2), records with matching DTYP get wired to
    // these factories. The factory closures capture BeamlineHolder to
    // retrieve the runtime handles created by miniBeamlineConfig.
    //
    // DTYP mapping:
    //   "miniBeamCurrent"   → BeamCurrentDeviceSupport (ai record)
    //   "asynFloat64" etc.  → universal asyn factory (all asyn port records)
    // ========================================================================

    {
        let h = holder.clone();
        app = app.register_device_support("miniBeamCurrent", move || {
            let rx = h
                .beam_rx
                .lock()
                .unwrap()
                .take()
                .expect("miniBeamlineConfig must be called before iocInit");
            Box::new(BeamCurrentDeviceSupport::new(h.beam_value.clone(), rx))
        });
    }

    // ========================================================================
    // Random-waveform device support (for bulk-transfer benchmarks).
    //
    // Each `mini:wf<N>` record refreshes its slot on every 1 Hz scan;
    // `mini:wf:bundle` re-publishes the full structure once all 10
    // records have processed in the same cycle. CA per-record monitors
    // and the PVA bundle therefore see different interleavings — the
    // bundle is atomic per-cycle, the CA stream is per-record.
    // ========================================================================
    {
        let factory = Arc::new(RandomWaveformFactory::new_with_count(10_000, 10));
        factory.register_pva_bundle("mini:wf:bundle", "wf");
        app = app.register_device_support("miniRandomWaveform", move || factory.make());
    }
    // PointDetector and MovingDot records now use standard asyn DTYPs
    // (asynFloat64, asynInt32) with @asyn(PORT,...) links. The universal
    // asyn factory handles them via drv_user_create → find_param.

    // Plugin ports are registered in the global asyn port registry by
    // PluginManager::add_plugin(). The universal asyn factory handles all
    // plugin records via @asyn(PORT,...) links — no plugin-specific factory needed.

    // ========================================================================
    // Simulated motors (st.cmd driven)
    //
    // Motors use the template-based pattern: st.cmd calls simMotorCreate()
    // to create a SimMotor driver + poll loop, then dbLoadRecords() loads
    // motor.template which creates a MotorRecord with matching DTYP.
    // During iocInit, the dynamic factory wires them together, and
    // DeviceSupport::init() injects the SharedDeviceState into the record.
    //
    // This replaces ~60 lines of hardcoded motor setup with 4 lines here
    // + configuration in st.cmd (simMotorCreate + dbLoadRecords per motor).
    // ========================================================================

    epics_base_rs::runtime::env::set_default("MOTOR", motor_rs::MOTOR_IOC_DIR);
    let motor_holder = SimMotorHolder::new();
    app = app.register_startup_command(motor_holder.sim_motor_create_command());
    app = app.register_dynamic_device_support(motor_holder.device_support_factory());

    // ========================================================================
    // Optics serial device drivers (SimHsc slit, SimQxbpm beam position monitor)
    //
    // Called from st.cmd as:
    //   simHscCreate("HSC1", 100)
    //   simQxbpmCreate("QXBPM1", 0.0, 0.0, 100)
    // ========================================================================

    let hsc_holder = optics_rs::drivers::hsc::HscHolder::new();
    app = app.register_startup_command(hsc_holder.sim_hsc_create_command());

    let qxbpm_holder = optics_rs::drivers::qxbpm::QxbpmHolder::new();
    app = app.register_startup_command(qxbpm_holder.sim_qxbpm_create_command());

    // Start all polling after PINI processing (not during st.cmd or iocInit)
    let motor_h = motor_holder.clone();
    let hsc_h = hsc_holder.clone();
    let qxbpm_h = qxbpm_holder.clone();
    app = app.register_after_init(move || {
        motor_h.start_all_polling();
        hsc_h.start_all_polling();
        qxbpm_h.start_all_polling();
    });

    // ========================================================================
    // Optics SNL program launcher (replaces C EPICS `seq &program, "macros"`)
    //
    // Called from st.cmd as:
    //   seqStart("kohzuCtl", "P=mini:,M_THETA=dcm:theta,M_Y=dcm:y,M_Z=dcm:z")
    //   seqStart("hrCtl", "P=mini:,N=1,M_PHI1=hr:phi1,M_PHI2=hr:phi2")
    //   seqStart("orient", "P=mini:,PM=mini:,mTTH=tth,mTH=th,mCHI=chi,mPHI=phi")
    // ========================================================================

    app = app.register_startup_command(CommandDef::new(
        "seqStart",
        vec![
            ArgDesc {
                name: "program",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "macros",
                arg_type: ArgType::String,
            },
        ],
        "seqStart program macros - Start an optics SNL program",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let program = match &args[0] {
                ArgValue::String(s) => s.clone(),
                _ => return Err("expected program name".into()),
            };
            let macros = match &args[1] {
                ArgValue::String(s) => s.clone(),
                _ => return Err("expected macros string".into()),
            };
            optics_rs::seq_runner::seq_start(&program, &macros, ctx.bridge(), ctx.db())?;
            Ok(CommandOutcome::Continue)
        },
    ));

    // ========================================================================
    // Shell commands (available after iocInit in the interactive REPL)
    // ========================================================================

    {
        let mgr_r = mgr.clone();
        app = app.register_shell_command(CommandDef::new(
            "miniBeamlineReport",
            vec![ArgDesc {
                name: "level",
                arg_type: ArgType::Int,
            }],
            "miniBeamlineReport [level] - Report beamline status",
            move |_args: &[ArgValue], _ctx: &CommandContext| {
                println!("Mini Beamline Report");
                mgr_r.report();
                Ok(CommandOutcome::Continue)
            },
        ));
    }

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
        "mini_ioc needs the tokio backend; this build selects the \
         reactor-free execution model (EPICS_RS_BUILD_EXEC_BACKEND=thread)."
    );
    Ok(())
}
