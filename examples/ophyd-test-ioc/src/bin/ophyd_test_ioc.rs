//! ophyd test IOC binary.
//!
//! Provides all EPICS PVs expected by the ophyd test suite:
//!   - 6 motors (XF:31IDA-OP{Tbl-Ax:X1..X6}Mtr)
//!   - 2 sim motors (sim:mtr1, sim:mtr2)
//!   - 1 fake motor (XF:31IDA-OP{Tbl-Ax:FakeMtr})
//!   - 6 sensors (XF:31IDA-BI{Dev:1..6}E-I)
//!   - 1 AreaDetector with standard plugins (XF:31IDA-BI{Cam:Tbl}: and ADSIM:)
//!
//! Replaces the Docker-based epics-services-for-ophyd.
//!
//! Usage:
//!   cargo run -p ophyd-test-ioc --features ioc -- ioc/st.cmd

use std::sync::Arc;

use asyn_rs::trace::TraceManager;
use epics_base_rs::error::CaResult;
use epics_ca_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::*;

use ad_core_rs::ioc::{PluginManager, register_noop_commands};
use ad_core_rs::plugin::channel::NDArrayOutput;
use ad_core_rs::plugin::registry::ParamRegistry;

use motor_rs::ioc::SimMotorHolder;

use ophyd_test_ioc::physics::MovingDotImageConfig;
use ophyd_test_ioc::sim_detector::driver::{MovingDotRuntime, create_moving_dot_with_config};
use ophyd_test_ioc::sim_detector::ioc_support::{
    MovingDotDeviceSupport,
    build_param_registry as build_ad_registry,
};

fn env_i32(name: &str, default: i32) -> i32 {
    std::env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

struct AdHolder {
    runtime: std::sync::Mutex<Option<(MovingDotRuntime, Arc<ParamRegistry>)>>,
    trace: Arc<TraceManager>,
}

impl AdHolder {
    fn new(trace: Arc<TraceManager>) -> Arc<Self> {
        Arc::new(Self {
            runtime: std::sync::Mutex::new(None),
            trace,
        })
    }
}

#[epics_base_rs::epics_main]
async fn main() -> CaResult<()> {
    let args: Vec<String> = std::env::args().collect();

    epics_base_rs::runtime::env::set_default(
        "OPHYD_TEST_IOC",
        env!("CARGO_MANIFEST_DIR"),
    );
    epics_base_rs::runtime::env::set_default(
        "ADCORE",
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../crates/ad-core-rs"),
    );

    let script = if args.len() > 1 && !args[1].starts_with('-') {
        args[1].clone()
    } else {
        eprintln!("Usage: ophyd_test_ioc <st.cmd>");
        std::process::exit(1);
    };

    let trace = Arc::new(TraceManager::new());
    let mgr = PluginManager::new(trace.clone());
    let holder = AdHolder::new(trace.clone());

    let (asyn_name, asyn_factory) = asyn_rs::asyn_record::asyn_record_factory();
    let (motor_name, motor_factory) = motor_rs::motor_record_factory();

    let mut app = IocApplication::new();
    app = app.register_record_type(asyn_name, move || asyn_factory());
    app = app.register_record_type(motor_name, move || motor_factory());
    app = app.port(
        std::env::var("EPICS_CA_SERVER_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5064),
    );

    // ========================================================================
    // AreaDetector config command
    // ========================================================================
    {
        let mgr_c = mgr.clone();
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "ophydTestAdConfig",
            vec![],
            "ophydTestAdConfig - Configure simulated area detector for ophyd tests",
            move |_args: &[ArgValue], _ctx: &CommandContext| {
                let size_x = env_i32("XSIZE", 640);
                let size_y = env_i32("YSIZE", 480);
                let max_mem = env_u64("AD_MAX_MEMORY", 50_000_000) as usize;
                println!("ophydTestAdConfig: {}x{}, pool={}B", size_x, size_y, max_mem);

                let output = NDArrayOutput::new();
                let config = MovingDotImageConfig {
                    sigma_x: 50.0,
                    sigma_y: 25.0,
                    background: 1000.0,
                    n_per_i_per_s: 200.0,
                };

                let rt = create_moving_dot_with_config(
                    "SIM", size_x, size_y, max_mem, output, config,
                ).map_err(|e| format!("failed to create SimDetector: {e}"))?;

                let registry = Arc::new(build_ad_registry(&rt.ad_params, &rt.dot_params));
                let port_handle = rt.port_handle().clone();
                asyn_rs::asyn_record::register_port("SIM", port_handle, h.trace.clone());

                mgr_c.set_driver(Arc::new(ad_core_rs::ioc::GenericDriverContext::new(
                    rt.pool().clone(),
                    rt.array_output().clone(),
                    "SIM",
                    mgr_c.wiring(),
                )));

                *h.runtime.lock().unwrap() = Some((rt, registry));
                println!("ophydTestAdConfig: done");
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // AD plugin commands
    app = ad_plugins_rs::ioc::register_all_plugins(app, &mgr);
    app = register_noop_commands(app);

    // Device support for AD camera
    {
        let h = holder.clone();
        app = app.register_device_support("asynOphydTestAd", move || {
            let guard = h.runtime.lock().unwrap();
            let (rt, registry) = guard.as_ref().expect("ophydTestAdConfig must be called before iocInit");
            Box::new(MovingDotDeviceSupport::from_handle(
                rt.port_handle().clone(), registry.clone(),
            ))
        });
    }

    // Plugin device support (dynamic DTYP)
    app = mgr.register_device_support(app);

    // ========================================================================
    // Motors
    // ========================================================================
    epics_base_rs::runtime::env::set_default("MOTOR", motor_rs::MOTOR_IOC_DIR);
    let motor_holder = SimMotorHolder::new();
    app = app.register_startup_command(motor_holder.sim_motor_create_command());
    app = app.register_dynamic_device_support(motor_holder.device_support_factory());

    // ========================================================================
    // Run
    // ========================================================================
    app.startup_script(&script)
        .run(epics_ca_rs::server::run_ca_ioc)
        .await
}
