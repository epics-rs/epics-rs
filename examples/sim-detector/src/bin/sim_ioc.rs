//! SimDetector IOC binary.
//!
//! Uses IocApplication for st.cmd-style startup matching the C++ EPICS pattern.
//! Supports C-compatible plugin configure commands and `< commonPlugins.cmd` includes.
//!
//! Usage:
//!   cargo run --bin sim_ioc --features ioc -- st.cmd
//!   cargo run --bin sim_ioc --features ioc -- ioc/st.cmd

use std::sync::Arc;

use asyn_rs::port_handle::PortHandle;
use asyn_rs::trace::TraceManager;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::*;

use ad_core::plugin::channel::NDArrayOutput;
use ad_core::plugin::runtime::{PluginRuntimeHandle, create_plugin_runtime};
use ad_plugins::std_arrays::create_std_arrays_runtime;
use ad_plugins::stats::create_stats_runtime;
use sim_detector::driver::{create_sim_detector, SimDetectorRuntime};
use ad_core::plugin::registry::ParamRegistry;
use ad_plugins::stats::build_stats_registry;
use sim_detector::ioc_support::{build_param_registry_from_params, SimDeviceSupport};
use sim_detector::plugin_support::{build_plugin_base_registry, ArrayDataHandle, PluginDeviceSupport};

/// Info about a configured plugin (stored for device support wiring).
struct PluginInfo {
    dtyp_name: String,
    port_handle: PortHandle,
    registry: Arc<ParamRegistry>,
    array_data: Option<ArrayDataHandle>,
}

/// Shared state between startup commands and device support factory.
struct DriverHolder {
    port_handle: std::sync::Mutex<Option<PortHandle>>,
    registry: std::sync::Mutex<Option<Arc<ParamRegistry>>>,
    trace: Arc<TraceManager>,
    _runtime: std::sync::Mutex<Option<SimDetectorRuntime>>,
    plugins: std::sync::Mutex<Vec<PluginInfo>>,
    _plugin_handles: std::sync::Mutex<Vec<PluginRuntimeHandle>>,
}

impl DriverHolder {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            port_handle: std::sync::Mutex::new(None),
            registry: std::sync::Mutex::new(None),
            trace: Arc::new(TraceManager::new()),
            _runtime: std::sync::Mutex::new(None),
            plugins: std::sync::Mutex::new(Vec::new()),
            _plugin_handles: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn add_plugin(&self, dtyp: &str, handle: &PluginRuntimeHandle, array_data: Option<ArrayDataHandle>) {
        let registry = Arc::new(build_plugin_base_registry(handle));
        self.add_plugin_with_registry(dtyp, handle, registry, array_data);
    }

    fn add_plugin_with_registry(&self, dtyp: &str, handle: &PluginRuntimeHandle, registry: Arc<ParamRegistry>, array_data: Option<ArrayDataHandle>) {
        let port_handle = handle.port_runtime().port_handle().clone();
        let port_name = port_handle.port_name().to_string();
        self.plugins.lock().unwrap().push(PluginInfo {
            dtyp_name: dtyp.to_string(),
            port_handle: port_handle.clone(),
            registry,
            array_data,
        });
        self._plugin_handles.lock().unwrap().push(handle.clone());
        asyn_rs::asyn_record::register_port(&port_name, port_handle, self.trace.clone());
    }
}

// ===== Helpers =====

/// Extract port_name, queue_size, and ndarray_port from C-compatible plugin args.
/// C format: (portName, queueSize, blockingCallbacks, NDArrayPort, NDArrayAddr, ...)
fn extract_plugin_args(args: &[ArgValue]) -> Result<(String, usize, String), String> {
    let port_name = match &args[0] {
        ArgValue::String(s) => s.clone(),
        _ => return Err("portName required".into()),
    };
    let queue_size = match args.get(1) {
        Some(ArgValue::Int(n)) => *n as usize,
        _ => 20,
    };
    let ndarray_port = match args.get(3) {
        Some(ArgValue::String(s)) => s.clone(),
        _ => String::new(),
    };
    Ok((port_name, queue_size, ndarray_port))
}

/// Auto-derive DTYP from port name: "ROI1" → "asynROI1"
fn dtyp_from_port(port_name: &str) -> String {
    format!("asyn{port_name}")
}

/// Standard C-compatible arg descriptors for plugin configure commands.
fn plugin_args() -> Vec<ArgDesc> {
    vec![
        ArgDesc { name: "portName", arg_type: ArgType::String, optional: false },
        ArgDesc { name: "queueSize", arg_type: ArgType::Int, optional: true },
        ArgDesc { name: "blockingCallbacks", arg_type: ArgType::Int, optional: true },
        ArgDesc { name: "NDArrayPort", arg_type: ArgType::String, optional: true },
        ArgDesc { name: "NDArrayAddr", arg_type: ArgType::Int, optional: true },
        ArgDesc { name: "maxBuffers", arg_type: ArgType::Int, optional: true },
        ArgDesc { name: "maxMemory", arg_type: ArgType::Int, optional: true },
        ArgDesc { name: "priority", arg_type: ArgType::Int, optional: true },
        ArgDesc { name: "stackSize", arg_type: ArgType::Int, optional: true },
        ArgDesc { name: "maxThreads", arg_type: ArgType::Int, optional: true },
    ]
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

        asyn_rs::asyn_record::register_port(&port_name, port_handle.clone(), self.holder.trace.clone());

        *self.holder.port_handle.lock().unwrap() = Some(port_handle);
        *self.holder.registry.lock().unwrap() = Some(registry);
        *self.holder._runtime.lock().unwrap() = Some(runtime);

        Ok(CommandOutcome::Continue)
    }
}

/// Register all plugin configure commands on the IocApplication.
fn register_all_plugins(mut app: IocApplication, holder: &Arc<DriverHolder>) -> IocApplication {
    // --- NDStdArraysConfigure ---
    // C: NDStdArraysConfigure(portName, queueSize, blockingCallbacks, NDArrayPort, NDArrayAddr, maxBuffers, maxMemory)
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDStdArraysConfigure",
            plugin_args(),
            "NDStdArraysConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, _queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                let (handle, data, _jh) = create_std_arrays_runtime(&port_name, pool, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDStdArraysConfigure: port={port_name}");
                h.add_plugin(&dtyp, &handle, Some(data));
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDStatsConfigure ---
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDStatsConfigure",
            plugin_args(),
            "NDStatsConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                let (handle, _stats, stats_params, ts_runtime, ts_params, _jh, _ts_actor_jh, _ts_data_jh) =
                    create_stats_runtime(&port_name, pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDStatsConfigure: port={port_name}");

                // Register stats plugin port
                let registry = Arc::new(build_stats_registry(&handle, &stats_params));
                h.add_plugin_with_registry(&dtyp, &handle, registry, None);

                // Register TS port as a separate asyn port
                let ts_port_name = format!("{port_name}_TS");
                let ts_dtyp = dtyp_from_port(&ts_port_name);
                let ts_registry = Arc::new(ad_plugins::time_series::build_ts_registry(&ts_params));
                let ts_port_handle = ts_runtime.port_handle().clone();
                h.plugins.lock().unwrap().push(PluginInfo {
                    dtyp_name: ts_dtyp.clone(),
                    port_handle: ts_port_handle.clone(),
                    registry: ts_registry,
                    array_data: None,
                });
                asyn_rs::asyn_record::register_port(&ts_port_name, ts_port_handle, h.trace.clone());
                println!("  TimeSeries port: {ts_port_name} (DTYP: {ts_dtyp})");

                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDROIConfigure ---
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDROIConfigure",
            plugin_args(),
            "NDROIConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::roi::{ROIConfig, ROIProcessor};
                let (handle, _jh) = create_plugin_runtime(&port_name, ROIProcessor::new(ROIConfig::default()), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDROIConfigure: port={port_name}");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDProcessConfigure ---
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDProcessConfigure",
            plugin_args(),
            "NDProcessConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::process::{ProcessConfig, ProcessProcessor};
                let (handle, _jh) = create_plugin_runtime(&port_name, ProcessProcessor::new(ProcessConfig::default()), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDProcessConfigure: port={port_name}");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDTransformConfigure ---
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDTransformConfigure",
            plugin_args(),
            "NDTransformConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::transform::{TransformType, TransformProcessor};
                let (handle, _jh) = create_plugin_runtime(&port_name, TransformProcessor::new(TransformType::None), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDTransformConfigure: port={port_name}");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDColorConvertConfigure ---
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDColorConvertConfigure",
            plugin_args(),
            "NDColorConvertConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::color_convert::{ColorConvertConfig, ColorConvertProcessor};
                use ad_core::color::{NDColorMode, NDBayerPattern};
                let config = ColorConvertConfig { target_mode: NDColorMode::Mono, bayer_pattern: NDBayerPattern::RGGB, false_color: false };
                let (handle, _jh) = create_plugin_runtime(&port_name, ColorConvertProcessor::new(config), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDColorConvertConfigure: port={port_name}");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDOverlayConfigure ---
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDOverlayConfigure",
            plugin_args(),
            "NDOverlayConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::overlay::OverlayProcessor;
                let (handle, _jh) = create_plugin_runtime(&port_name, OverlayProcessor::new(vec![]), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDOverlayConfigure: port={port_name}");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDFFTConfigure ---
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDFFTConfigure",
            plugin_args(),
            "NDFFTConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::fft::{FFTMode, FFTProcessor};
                let (handle, _jh) = create_plugin_runtime(&port_name, FFTProcessor::new(FFTMode::Rows1D), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDFFTConfigure: port={port_name}");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDCircularBuffConfigure ---
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDCircularBuffConfigure",
            plugin_args(),
            "NDCircularBuffConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::circular_buff::{CircularBuffProcessor, TriggerCondition};
                let (handle, _jh) = create_plugin_runtime(&port_name, CircularBuffProcessor::new(100, 100, TriggerCondition::External), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDCircularBuffConfigure: port={port_name}");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDCodecConfigure ---
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDCodecConfigure",
            plugin_args(),
            "NDCodecConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::codec::{CodecMode, CodecProcessor};
                use ad_core::codec::CodecName;
                let (handle, _jh) = create_plugin_runtime(&port_name, CodecProcessor::new(CodecMode::Compress { codec: CodecName::LZ4, quality: 90 }), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDCodecConfigure: port={port_name}");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDScatterConfigure ---
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDScatterConfigure",
            plugin_args(),
            "NDScatterConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::scatter::ScatterProcessor;
                let (handle, _jh) = create_plugin_runtime(&port_name, ScatterProcessor::new(), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDScatterConfigure: port={port_name}");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDGatherConfigure ---
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDGatherConfigure",
            plugin_args(),
            "NDGatherConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::gather::GatherProcessor;
                let (handle, _jh) = create_plugin_runtime(&port_name, GatherProcessor::new(), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDGatherConfigure: port={port_name}");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDFileTIFFConfigure ---
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDFileTIFFConfigure",
            plugin_args(),
            "NDFileTIFFConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::file_tiff::TiffFileProcessor;
                let (handle, _jh) = create_plugin_runtime(&port_name, TiffFileProcessor::new(), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDFileTIFFConfigure: port={port_name}");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDFileJPEGConfigure ---
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDFileJPEGConfigure",
            plugin_args(),
            "NDFileJPEGConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::file_jpeg::JpegFileProcessor;
                let (handle, _jh) = create_plugin_runtime(&port_name, JpegFileProcessor::new(90), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDFileJPEGConfigure: port={port_name}");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDFileHDF5Configure ---
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDFileHDF5Configure",
            plugin_args(),
            "NDFileHDF5Configure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::file_hdf5::Hdf5FileProcessor;
                let (handle, _jh) = create_plugin_runtime(&port_name, Hdf5FileProcessor::new(), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDFileHDF5Configure: port={port_name}");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- Stub plugins (not yet fully implemented, use PassthroughProcessor) ---

    // NDROIStatConfigure
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDROIStatConfigure",
            plugin_args(),
            "NDROIStatConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::passthrough::PassthroughProcessor;
                let (handle, _jh) = create_plugin_runtime(&port_name, PassthroughProcessor::new("NDPluginROIStat"), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDROIStatConfigure: port={port_name} (stub)");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // NDAttrConfigure
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDAttrConfigure",
            plugin_args(),
            "NDAttrConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::passthrough::PassthroughProcessor;
                let (handle, _jh) = create_plugin_runtime(&port_name, PassthroughProcessor::new("NDPluginAttribute"), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDAttrConfigure: port={port_name} (stub)");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // NDBadPixelConfigure
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDBadPixelConfigure",
            plugin_args(),
            "NDBadPixelConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::passthrough::PassthroughProcessor;
                let (handle, _jh) = create_plugin_runtime(&port_name, PassthroughProcessor::new("NDPluginBadPixel"), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDBadPixelConfigure: port={port_name} (stub)");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // NDFileNetCDFConfigure
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDFileNetCDFConfigure",
            plugin_args(),
            "NDFileNetCDFConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::passthrough::PassthroughProcessor;
                let (handle, _jh) = create_plugin_runtime(&port_name, PassthroughProcessor::new("NDFileNetCDF"), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDFileNetCDFConfigure: port={port_name} (stub)");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // NDFileNexusConfigure
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDFileNexusConfigure",
            plugin_args(),
            "NDFileNexusConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::passthrough::PassthroughProcessor;
                let (handle, _jh) = create_plugin_runtime(&port_name, PassthroughProcessor::new("NDFileNexus"), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDFileNexusConfigure: port={port_name} (stub)");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // NDFileMagickConfigure
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDFileMagickConfigure",
            plugin_args(),
            "NDFileMagickConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::passthrough::PassthroughProcessor;
                let (handle, _jh) = create_plugin_runtime(&port_name, PassthroughProcessor::new("NDFileMagick"), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDFileMagickConfigure: port={port_name} (stub)");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // NDTimeSeriesConfigure (stub - time series is a helper, not a standalone plugin in C)
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDTimeSeriesConfigure",
            plugin_args(),
            "NDTimeSeriesConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::passthrough::PassthroughProcessor;
                let (handle, _jh) = create_plugin_runtime(&port_name, PassthroughProcessor::new("NDPluginTimeSeries"), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDTimeSeriesConfigure: port={port_name} (stub)");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // NDPvaConfigure (stub)
    {
        let h = holder.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDPvaConfigure",
            plugin_args(),
            "NDPvaConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let rt = h._runtime.lock().unwrap();
                let rt = rt.as_ref().ok_or("simDetectorConfig must be called first")?;
                let pool = rt.pool().clone();
                use ad_plugins::passthrough::PassthroughProcessor;
                let (handle, _jh) = create_plugin_runtime(&port_name, PassthroughProcessor::new("NDPluginPva"), pool, queue_size, &ndarray_port);
                rt.connect_downstream(handle.array_sender().clone());
                println!("NDPvaConfigure: port={port_name} (stub)");
                h.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- No-op commands used in C commonPlugins.cmd ---
    for cmd in &[
        "set_requestfile_path",
        "set_savefile_path",
        "set_pass0_restoreFile",
        "set_pass1_restoreFile",
        "save_restoreSet_status_prefix",
        "startPVAServer",
        "callbackSetQueueSize",
    ] {
        let name = *cmd;
        app = app.register_startup_command(CommandDef::new(
            name,
            vec![
                ArgDesc { name: "arg1", arg_type: ArgType::String, optional: true },
                ArgDesc { name: "arg2", arg_type: ArgType::String, optional: true },
            ],
            &format!("{name} [args...] - no-op (not implemented)"),
            move |_args: &[ArgValue], _ctx: &CommandContext| {
                // silently ignored
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    app
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
            println!("    - {} (DTYP: {})", p.port_handle.port_name(), p.dtyp_name);
        }
        Ok(CommandOutcome::Continue)
    }
}

#[tokio::main]
async fn main() -> CaResult<()> {
    let args: Vec<String> = std::env::args().collect();

    // Set C source tree paths for template include resolution
    if std::env::var_os("ADCORE").is_none() {
        unsafe { std::env::set_var("ADCORE", concat!(env!("CARGO_MANIFEST_DIR"), "/../ad-core")) };
    }
    if std::env::var_os("ADSIMDETECTOR").is_none() {
        unsafe { std::env::set_var("ADSIMDETECTOR", env!("CARGO_MANIFEST_DIR")) };
    }

    let script = if args.len() > 1 && !args[1].starts_with('-') {
        args[1].clone()
    } else {
        eprintln!("Usage: sim_ioc <st.cmd>");
        eprintln!();
        eprintln!("The st.cmd script should contain:");
        eprintln!(r#"  epicsEnvSet("PREFIX", "SIM1:")"#);
        eprintln!(r#"  simDetectorConfig("SIM1", 256, 256, 50000000)"#);
        eprintln!(r#"  dbLoadRecords("$(ADSIMDETECTOR)/db/simDetector.template", "P=$(PREFIX),R=cam1:,PORT=SIM1,DTYP=asynSimDetector")"#);
        std::process::exit(1);
    };

    // Register the full asynRecord type (overrides the minimal stub)
    asyn_rs::asyn_record::register_asyn_record_type();

    let holder = DriverHolder::new();
    let holder_for_config = holder.clone();
    let holder_for_factory = holder.clone();
    let holder_for_report = holder.clone();
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

    // Register all plugin configure commands
    app = register_all_plugins(app, &holder);

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

    // Device support: plugins (dynamic lookup by DTYP)
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
