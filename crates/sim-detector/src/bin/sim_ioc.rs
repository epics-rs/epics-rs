//! SimDetector IOC binary.
//!
//! Uses IocApplication for st.cmd-style startup matching the C++ EPICS pattern.
//!
//! Usage:
//!   cargo run --bin sim_ioc --features ioc -- st.cmd
//!   cargo run --bin sim_ioc --features ioc -- ioc/st.cmd
//!
//! The st.cmd script can use:
//!   epicsEnvSet, dbLoadRecords, simDetectorConfig,
//!   NDStdArraysConfigure, NDStatsConfigure, NDROIConfigure, NDProcessConfigure

use std::sync::Arc;

use asyn_rs::port_handle::PortHandle;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::*;

use ad_core::plugin::channel::NDArrayOutput;
use ad_core::plugin::runtime::PluginRuntimeHandle;
use ad_plugins::std_arrays::create_std_arrays_runtime;
use ad_plugins::stats::create_stats_runtime;
use ad_plugins::roi::{ROIConfig, ROIProcessor};
use ad_plugins::process::{ProcessConfig, ProcessProcessor};
use sim_detector::driver::{create_sim_detector, SimDetectorRuntime};
use sim_detector::ioc_support::{build_param_registry_from_params, ParamRegistry, SimDeviceSupport};
use sim_detector::plugin_support::{build_plugin_base_registry, ArrayDataHandle, PluginDeviceSupport};

/// Info about a configured plugin (stored for device support wiring).
struct PluginInfo {
    dtyp_name: String,
    port_handle: PortHandle,
    registry: Arc<ParamRegistry>,
    /// Handle to latest NDArray data (only for StdArrays plugins).
    array_data: Option<ArrayDataHandle>,
}

/// Shared state between startup commands and device support factory.
struct DriverHolder {
    port_handle: std::sync::Mutex<Option<PortHandle>>,
    registry: std::sync::Mutex<Option<Arc<ParamRegistry>>>,
    /// Keep runtime alive to prevent shutdown
    _runtime: std::sync::Mutex<Option<SimDetectorRuntime>>,
    /// Plugin info for device support registration
    plugins: std::sync::Mutex<Vec<PluginInfo>>,
    /// Keep plugin runtime handles alive
    _plugin_handles: std::sync::Mutex<Vec<PluginRuntimeHandle>>,
}

impl DriverHolder {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            port_handle: std::sync::Mutex::new(None),
            registry: std::sync::Mutex::new(None),
            _runtime: std::sync::Mutex::new(None),
            plugins: std::sync::Mutex::new(Vec::new()),
            _plugin_handles: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn add_plugin(&self, dtyp: &str, handle: &PluginRuntimeHandle, array_data: Option<ArrayDataHandle>) {
        let port_handle = handle.port_runtime().port_handle().clone();
        let registry = Arc::new(build_plugin_base_registry(handle));
        self.plugins.lock().unwrap().push(PluginInfo {
            dtyp_name: dtyp.to_string(),
            port_handle,
            registry,
            array_data,
        });
        self._plugin_handles.lock().unwrap().push(handle.clone());
    }
}

// ===== simDetectorConfig =====

struct SimDetectorConfigHandler {
    holder: Arc<DriverHolder>,
}

impl CommandHandler for SimDetectorConfigHandler {
    fn call(&self, args: &[ArgValue], _ctx: &CommandContext) -> CommandResult {
        let port_name = match &args[0] {
            ArgValue::String(s) => s.clone(),
            _ => return Err("portName required".into()),
        };
        let size_x = match &args[1] {
            ArgValue::Int(n) => *n as i32,
            _ => 256,
        };
        let size_y = match &args[2] {
            ArgValue::Int(n) => *n as i32,
            _ => 256,
        };
        let max_memory = match &args[3] {
            ArgValue::Int(n) => *n as usize,
            _ => 50_000_000,
        };

        println!("simDetectorConfig: port={port_name}, size={size_x}x{size_y}, maxMemory={max_memory}");

        let array_output = NDArrayOutput::new();
        let runtime = create_sim_detector(&port_name, size_x, size_y, max_memory, array_output)
            .map_err(|e| format!("failed to create SimDetector: {e}"))?;

        let registry = Arc::new(build_param_registry_from_params(&runtime.ad_params, &runtime.sim_params));
        let port_handle = runtime.port_handle().clone();

        *self.holder.port_handle.lock().unwrap() = Some(port_handle);
        *self.holder.registry.lock().unwrap() = Some(registry);
        *self.holder._runtime.lock().unwrap() = Some(runtime);

        Ok(CommandOutcome::Continue)
    }
}

// ===== NDStdArraysConfigure =====

struct NDStdArraysConfigHandler {
    holder: Arc<DriverHolder>,
}

impl CommandHandler for NDStdArraysConfigHandler {
    fn call(&self, args: &[ArgValue], _ctx: &CommandContext) -> CommandResult {
        let port_name = match &args[0] {
            ArgValue::String(s) => s.clone(),
            _ => return Err("portName required".into()),
        };
        let dtyp = match &args[1] {
            ArgValue::String(s) => s.clone(),
            _ => return Err("DTYP name required".into()),
        };

        let runtime = self.holder._runtime.lock().unwrap();
        let runtime = runtime.as_ref().ok_or("simDetectorConfig must be called first")?;
        let pool = runtime.pool().clone();

        let (handle, data, _jh) = create_std_arrays_runtime(&port_name, pool);

        // Wire to detector
        runtime.connect_downstream(handle.array_sender().clone());

        println!("NDStdArraysConfigure: port={port_name}, dtyp={dtyp}");
        self.holder.add_plugin(&dtyp, &handle, Some(data));

        Ok(CommandOutcome::Continue)
    }
}

// ===== NDStatsConfigure =====

struct NDStatsConfigHandler {
    holder: Arc<DriverHolder>,
}

impl CommandHandler for NDStatsConfigHandler {
    fn call(&self, args: &[ArgValue], _ctx: &CommandContext) -> CommandResult {
        let port_name = match &args[0] {
            ArgValue::String(s) => s.clone(),
            _ => return Err("portName required".into()),
        };
        let dtyp = match &args[1] {
            ArgValue::String(s) => s.clone(),
            _ => return Err("DTYP name required".into()),
        };
        let queue_size = match args.get(2) {
            Some(ArgValue::Int(n)) => *n as usize,
            _ => 10,
        };

        let runtime = self.holder._runtime.lock().unwrap();
        let runtime = runtime.as_ref().ok_or("simDetectorConfig must be called first")?;
        let pool = runtime.pool().clone();

        let (handle, _stats_data, _jh) = create_stats_runtime(&port_name, pool, queue_size);
        runtime.connect_downstream(handle.array_sender().clone());

        println!("NDStatsConfigure: port={port_name}, dtyp={dtyp}, queueSize={queue_size}");
        self.holder.add_plugin(&dtyp, &handle, None);

        Ok(CommandOutcome::Continue)
    }
}

// ===== NDROIConfigure =====

struct NDROIConfigHandler {
    holder: Arc<DriverHolder>,
}

impl CommandHandler for NDROIConfigHandler {
    fn call(&self, args: &[ArgValue], _ctx: &CommandContext) -> CommandResult {
        let port_name = match &args[0] {
            ArgValue::String(s) => s.clone(),
            _ => return Err("portName required".into()),
        };
        let dtyp = match &args[1] {
            ArgValue::String(s) => s.clone(),
            _ => return Err("DTYP name required".into()),
        };
        let queue_size = match args.get(2) {
            Some(ArgValue::Int(n)) => *n as usize,
            _ => 10,
        };

        let runtime = self.holder._runtime.lock().unwrap();
        let runtime = runtime.as_ref().ok_or("simDetectorConfig must be called first")?;
        let pool = runtime.pool().clone();

        let processor = ROIProcessor::new(ROIConfig::default());
        let (handle, _jh) = ad_core::plugin::runtime::create_plugin_runtime(
            &port_name, processor, pool, queue_size,
        );
        runtime.connect_downstream(handle.array_sender().clone());

        println!("NDROIConfigure: port={port_name}, dtyp={dtyp}");
        self.holder.add_plugin(&dtyp, &handle, None);

        Ok(CommandOutcome::Continue)
    }
}

// ===== NDProcessConfigure =====

struct NDProcessConfigHandler {
    holder: Arc<DriverHolder>,
}

impl CommandHandler for NDProcessConfigHandler {
    fn call(&self, args: &[ArgValue], _ctx: &CommandContext) -> CommandResult {
        let port_name = match &args[0] {
            ArgValue::String(s) => s.clone(),
            _ => return Err("portName required".into()),
        };
        let dtyp = match &args[1] {
            ArgValue::String(s) => s.clone(),
            _ => return Err("DTYP name required".into()),
        };
        let queue_size = match args.get(2) {
            Some(ArgValue::Int(n)) => *n as usize,
            _ => 10,
        };

        let runtime = self.holder._runtime.lock().unwrap();
        let runtime = runtime.as_ref().ok_or("simDetectorConfig must be called first")?;
        let pool = runtime.pool().clone();

        let processor = ProcessProcessor::new(ProcessConfig::default());
        let (handle, _jh) = ad_core::plugin::runtime::create_plugin_runtime(
            &port_name, processor, pool, queue_size,
        );
        runtime.connect_downstream(handle.array_sender().clone());

        println!("NDProcessConfigure: port={port_name}, dtyp={dtyp}");
        self.holder.add_plugin(&dtyp, &handle, None);

        Ok(CommandOutcome::Continue)
    }
}

// ===== simDetectorReport =====

struct ReportHandler {
    holder: Arc<DriverHolder>,
}

impl CommandHandler for ReportHandler {
    fn call(&self, _args: &[ArgValue], _ctx: &CommandContext) -> CommandResult {
        let guard = self.holder.port_handle.lock().unwrap();
        if guard.is_none() {
            println!("No SimDetector configured");
            return Ok(CommandOutcome::Continue);
        }

        let plugins = self.holder.plugins.lock().unwrap();
        println!("SimDetector Report");
        println!("  Plugins: {}", plugins.len());
        for p in plugins.iter() {
            println!("    - {} (DTYP: {})", "port", p.dtyp_name);
        }
        Ok(CommandOutcome::Continue)
    }
}

#[tokio::main]
async fn main() -> CaResult<()> {
    let args: Vec<String> = std::env::args().collect();

    // Set C source tree paths for template include resolution
    // Computed from CARGO_MANIFEST_DIR at compile time so they work regardless of CWD
    // External env vars take priority; only set defaults for development
    if std::env::var_os("ADCORE").is_none() {
        unsafe { std::env::set_var("ADCORE", concat!(env!("CARGO_MANIFEST_DIR"), "/../../ADCore")) };
    }
    if std::env::var_os("ADSIMDETECTOR").is_none() {
        unsafe { std::env::set_var("ADSIMDETECTOR", concat!(env!("CARGO_MANIFEST_DIR"), "/../../ADSimDetector")) };
    }

    let script = if args.len() > 1 && !args[1].starts_with('-') {
        args[1].clone()
    } else {
        eprintln!("Usage: sim_ioc <st.cmd>");
        eprintln!();
        eprintln!("The st.cmd script should contain:");
        eprintln!(r#"  epicsEnvSet("PREFIX", "SIM1:")"#);
        eprintln!(r#"  simDetectorConfig("SIM1", 256, 256, 50000000)"#);
        eprintln!(r#"  dbLoadRecords("$(ADSIMDETECTOR)/simDetectorApp/Db/simDetector.template", "P=$(PREFIX),R=cam1:,PORT=SIM1,DTYP=asynSimDetector")"#);
        std::process::exit(1);
    };

    let holder = DriverHolder::new();
    let holder_for_config = holder.clone();
    let holder_for_factory = holder.clone();
    let holder_for_report = holder.clone();
    let holder_for_image = holder.clone();
    let holder_for_stats = holder.clone();
    let holder_for_roi = holder.clone();
    let holder_for_process = holder.clone();
    let holder_for_plugins = holder.clone();

    let mut app = IocApplication::new();
    app = app.port(
        std::env::var("EPICS_CA_SERVER_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5064),
    );

    // Phase 1: startup script commands
    app = app.register_startup_command(CommandDef::new(
        "simDetectorConfig",
        vec![
            ArgDesc { name: "portName", arg_type: ArgType::String, optional: false },
            ArgDesc { name: "sizeX", arg_type: ArgType::Int, optional: true },
            ArgDesc { name: "sizeY", arg_type: ArgType::Int, optional: true },
            ArgDesc { name: "maxMemory", arg_type: ArgType::Int, optional: true },
        ],
        "simDetectorConfig portName [sizeX] [sizeY] [maxMemory]",
        SimDetectorConfigHandler { holder: holder_for_config },
    ));

    app = app.register_startup_command(CommandDef::new(
        "NDStdArraysConfigure",
        vec![
            ArgDesc { name: "portName", arg_type: ArgType::String, optional: false },
            ArgDesc { name: "DTYP", arg_type: ArgType::String, optional: false },
        ],
        "NDStdArraysConfigure portName DTYP",
        NDStdArraysConfigHandler { holder: holder_for_image },
    ));

    app = app.register_startup_command(CommandDef::new(
        "NDStatsConfigure",
        vec![
            ArgDesc { name: "portName", arg_type: ArgType::String, optional: false },
            ArgDesc { name: "DTYP", arg_type: ArgType::String, optional: false },
            ArgDesc { name: "queueSize", arg_type: ArgType::Int, optional: true },
        ],
        "NDStatsConfigure portName DTYP [queueSize]",
        NDStatsConfigHandler { holder: holder_for_stats },
    ));

    app = app.register_startup_command(CommandDef::new(
        "NDROIConfigure",
        vec![
            ArgDesc { name: "portName", arg_type: ArgType::String, optional: false },
            ArgDesc { name: "DTYP", arg_type: ArgType::String, optional: false },
            ArgDesc { name: "queueSize", arg_type: ArgType::Int, optional: true },
        ],
        "NDROIConfigure portName DTYP [queueSize]",
        NDROIConfigHandler { holder: holder_for_roi },
    ));

    app = app.register_startup_command(CommandDef::new(
        "NDProcessConfigure",
        vec![
            ArgDesc { name: "portName", arg_type: ArgType::String, optional: false },
            ArgDesc { name: "DTYP", arg_type: ArgType::String, optional: false },
            ArgDesc { name: "queueSize", arg_type: ArgType::Int, optional: true },
        ],
        "NDProcessConfigure portName DTYP [queueSize]",
        NDProcessConfigHandler { holder: holder_for_process },
    ));

    // Device support: detector
    app = app.register_device_support("asynSimDetector", move || {
        let handle = holder_for_factory.port_handle.lock().unwrap()
            .as_ref()
            .expect("simDetectorConfig must be called before iocInit")
            .clone();
        let registry = holder_for_factory.registry.lock().unwrap()
            .as_ref()
            .expect("simDetectorConfig must be called before iocInit")
            .clone();
        Box::new(SimDeviceSupport::from_handle(handle, registry))
    });

    // Device support: plugins (register factories for each configured plugin)
    app = app.register_dynamic_device_support(move |dtyp_name| {
        let plugins = holder_for_plugins.plugins.lock().unwrap();
        for p in plugins.iter() {
            if p.dtyp_name == dtyp_name {
                let handle = p.port_handle.clone();
                let registry = p.registry.clone();
                let dtyp = p.dtyp_name.clone();
                let array_data = p.array_data.clone();
                return Some(Box::new(PluginDeviceSupport::new(handle, registry, &dtyp, array_data))
                    as Box<dyn epics_base_rs::server::device_support::DeviceSupport>);
            }
        }
        None
    });

    // Phase 3: interactive shell
    app = app.register_shell_command(CommandDef::new(
        "simDetectorReport",
        vec![ArgDesc { name: "level", arg_type: ArgType::Int, optional: true }],
        "simDetectorReport [level] - Report SimDetector status",
        ReportHandler { holder: holder_for_report },
    ));

    app.startup_script(&script)
        .run()
        .await
}
