//! Scope Simulator IOC — st.cmd-style startup using IocApplication.
//!
//! Port of EPICS testAsynPortDriver as an IOC with Channel Access.
//! Uses universal asyn device support (standard asyn DTYPs with @asyn() links).
//!
//! Usage:
//!   cargo run --release -p scope-ioc --features ioc --bin scope_ioc -- ioc/st.cmd

use std::sync::Arc;

use epics_base_rs::runtime::sync::Notify;

use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::{PortRuntimeHandle, create_port_runtime};
use asyn_rs::trace::TraceManager;
use scope_ioc::driver::*;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::iocsh::registry::*;
use epics_ca_rs::server::ioc_app::IocApplication;

// ========== DriverHolder + Command Handlers ==========

struct DriverHolder {
    runtime: std::sync::Mutex<Option<PortRuntimeHandle>>,
    notify: std::sync::Mutex<Option<Arc<Notify>>>,
    indices: std::sync::Mutex<Option<ParamIndices>>,
}

impl DriverHolder {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            runtime: std::sync::Mutex::new(None),
            notify: std::sync::Mutex::new(None),
            indices: std::sync::Mutex::new(None),
        })
    }
}

struct ConfigHandler {
    holder: Arc<DriverHolder>,
    handle: epics_base_rs::runtime::task::RuntimeHandle,
    trace: Arc<TraceManager>,
}

impl CommandHandler for ConfigHandler {
    fn call(&self, args: &[ArgValue], _ctx: &CommandContext) -> CommandResult {
        let port_name = match &args[0] {
            ArgValue::String(s) => s.clone(),
            _ => return Err("portName required".into()),
        };

        println!("scopeSimulatorConfig: port={port_name}");

        let notify = Arc::new(Notify::new());
        let driver = ScopeSimulator::new(&port_name, notify.clone());
        let indices = driver.param_indices();

        // Create the port runtime (actor thread + PortHandle)
        let (runtime_handle, _actor_jh) = create_port_runtime(driver, RuntimeConfig::default());

        // Register port in global registry so universal asyn device support can find it
        let port_handle = runtime_handle.port_handle().clone();
        asyn_rs::asyn_record::register_port(&port_name, port_handle.clone(), self.trace.clone());

        // Start background simulation task using the PortHandle API
        let sim_notify = notify.clone();
        self.handle.spawn(async move {
            sim_task_handle(port_handle, sim_notify, indices).await;
        });

        *self.holder.runtime.lock().unwrap() = Some(runtime_handle);
        *self.holder.notify.lock().unwrap() = Some(notify);
        *self.holder.indices.lock().unwrap() = Some(indices);

        Ok(CommandOutcome::Continue)
    }
}

struct ReportHandler {
    holder: Arc<DriverHolder>,
}

impl CommandHandler for ReportHandler {
    fn call(&self, _args: &[ArgValue], _ctx: &CommandContext) -> CommandResult {
        let guard = self.holder.runtime.lock().unwrap();
        let runtime = match guard.as_ref() {
            Some(r) => r,
            None => {
                println!("No ScopeSimulator configured");
                return Ok(CommandOutcome::Continue);
            }
        };

        let indices = self
            .holder
            .indices
            .lock()
            .unwrap()
            .expect("indices not set");

        let handle = runtime.port_handle();

        let run = handle.read_int32_blocking(indices.p_run, 0).unwrap_or(0);
        let max_pts = handle
            .read_int32_blocking(indices.p_max_points, 0)
            .unwrap_or(0);
        let update_t = handle
            .read_float64_blocking(indices.p_update_time, 0)
            .unwrap_or(0.0);
        let vpd = handle
            .read_float64_blocking(indices.p_volts_per_div, 0)
            .unwrap_or(0.0);
        let tpd = handle
            .read_float64_blocking(indices.p_time_per_div, 0)
            .unwrap_or(0.0);
        let noise = handle
            .read_float64_blocking(indices.p_noise_amplitude, 0)
            .unwrap_or(0.0);
        let offset = handle
            .read_float64_blocking(indices.p_volt_offset, 0)
            .unwrap_or(0.0);
        let min_v = handle
            .read_float64_blocking(indices.p_min_value, 0)
            .unwrap_or(0.0);
        let max_v = handle
            .read_float64_blocking(indices.p_max_value, 0)
            .unwrap_or(0.0);
        let mean_v = handle
            .read_float64_blocking(indices.p_mean_value, 0)
            .unwrap_or(0.0);

        println!("ScopeSimulator Report");
        println!("  Run:            {}", if run != 0 { "Yes" } else { "No" });
        println!("  MaxPoints:      {max_pts}");
        println!("  UpdateTime:     {update_t:.3} s");
        println!("  VoltsPerDiv:    {vpd:.3} V");
        println!("  TimePerDiv:     {tpd:.4} s");
        println!("  VoltOffset:     {offset:.3} V");
        println!("  NoiseAmplitude: {noise:.3} V");
        println!("  MinValue:       {min_v:.4}");
        println!("  MaxValue:       {max_v:.4}");
        println!("  MeanValue:      {mean_v:.4}");

        Ok(CommandOutcome::Continue)
    }
}

// ========== Main ==========

#[epics_base_rs::epics_main]
async fn main() -> CaResult<()> {
    let args: Vec<String> = std::env::args().collect();

    epics_base_rs::runtime::env::set_default("SCOPE_IOC", env!("CARGO_MANIFEST_DIR"));

    let script = if args.len() > 1 && !args[1].starts_with('-') {
        args[1].clone()
    } else {
        eprintln!("Usage: scope_ioc <st.cmd>");
        std::process::exit(1);
    };

    let trace = Arc::new(TraceManager::new());
    let holder = DriverHolder::new();
    let holder_for_config = holder.clone();
    let holder_for_report = holder.clone();
    let handle = epics_base_rs::runtime::task::runtime_handle();

    let mut app = IocApplication::new();

    app = app.port(
        std::env::var("EPICS_CA_SERVER_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5064),
    );

    // Register universal asyn device support (lowest priority — registered first)
    app = asyn_rs::adapter::register_asyn_device_support(app);

    app = app.register_startup_command(CommandDef::new(
        "scopeSimulatorConfig",
        vec![ArgDesc {
            name: "portName",
            arg_type: ArgType::String,
            optional: false,
        }],
        "scopeSimulatorConfig portName - Configure scope simulator driver",
        ConfigHandler {
            holder: holder_for_config,
            handle,
            trace,
        },
    ));

    app = app.register_shell_command(CommandDef::new(
        "scopeSimulatorReport",
        vec![ArgDesc {
            name: "level",
            arg_type: ArgType::Int,
            optional: true,
        }],
        "scopeSimulatorReport [level] - Report scope simulator status",
        ReportHandler {
            holder: holder_for_report,
        },
    ));

    app.startup_script(&script)
        .run(epics_ca_rs::server::run_ca_ioc)
        .await
}
