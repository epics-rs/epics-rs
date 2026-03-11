//! IOC plugin registration and areaDetector IOC application framework.
//!
//! Provides:
//! - [`register_all_plugins`]: registers startup commands like
//!   `NDStatsConfigure`, `NDROIConfigure`, etc. on an `IocApplication`.
//! - [`AdIoc`]: pre-configured IOC application that handles all common
//!   areaDetector boilerplate (plugins, device support, asynRecord, etc.).

use std::sync::Arc;

use ad_core::ioc::{dtyp_from_port, extract_plugin_args, plugin_arg_defs, PluginManager, register_noop_commands};
use ad_core::plugin::runtime::create_plugin_runtime;
use asyn_rs::trace::TraceManager;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::*;

/// Register all standard areaDetector plugin configure commands.
///
/// The `PluginManager` must have its driver context set (via `set_driver()`)
/// before any of these commands are invoked from st.cmd.
pub fn register_all_plugins(
    mut app: IocApplication,
    mgr: &Arc<PluginManager>,
) -> IocApplication {
    // --- NDStdArraysConfigure ---
    {
        let m = mgr.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDStdArraysConfigure",
            plugin_arg_defs(),
            "NDStdArraysConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, _queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let drv = m.driver()?;
                let pool = drv.pool();
                let (handle, data, _jh) =
                    crate::std_arrays::create_std_arrays_runtime(&port_name, pool, &ndarray_port);
                drv.connect_downstream(handle.array_sender().clone());
                println!("NDStdArraysConfigure: port={port_name}");
                m.add_plugin(&dtyp, &handle, Some(data));
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDStatsConfigure ---
    {
        let m = mgr.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDStatsConfigure",
            plugin_arg_defs(),
            "NDStatsConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let drv = m.driver()?;
                let pool = drv.pool();
                let (handle, _stats, stats_params, ts_runtime, ts_params, _jh, _ts_actor_jh, _ts_data_jh) =
                    crate::stats::create_stats_runtime(&port_name, pool, queue_size, &ndarray_port);
                drv.connect_downstream(handle.array_sender().clone());
                println!("NDStatsConfigure: port={port_name}");

                let registry = Arc::new(crate::stats::build_stats_registry(&handle, &stats_params));
                m.add_plugin_with_registry(&dtyp, &handle, registry, None);

                // Register TimeSeries as a separate asyn port
                let ts_port_name = format!("{port_name}_TS");
                let ts_dtyp = dtyp_from_port(&ts_port_name);
                let ts_registry = Arc::new(crate::time_series::build_ts_registry(&ts_params));
                let ts_port_handle = ts_runtime.port_handle().clone();
                m.add_port(&ts_dtyp, ts_port_handle, ts_registry);
                println!("  TimeSeries port: {ts_port_name} (DTYP: {ts_dtyp})");

                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- Generic plugins using create_plugin_runtime ---
    app = register_generic_plugin(&mut app, mgr, "NDROIConfigure", |port_name, queue_size, ndarray_port, pool| {
        use crate::roi::{ROIConfig, ROIProcessor};
        create_plugin_runtime(port_name, ROIProcessor::new(ROIConfig::default()), pool, queue_size, ndarray_port)
    });
    app = register_generic_plugin(&mut app, mgr, "NDProcessConfigure", |port_name, queue_size, ndarray_port, pool| {
        use crate::process::{ProcessConfig, ProcessProcessor};
        create_plugin_runtime(port_name, ProcessProcessor::new(ProcessConfig::default()), pool, queue_size, ndarray_port)
    });
    app = register_generic_plugin(&mut app, mgr, "NDTransformConfigure", |port_name, queue_size, ndarray_port, pool| {
        use crate::transform::{TransformType, TransformProcessor};
        create_plugin_runtime(port_name, TransformProcessor::new(TransformType::None), pool, queue_size, ndarray_port)
    });
    app = register_generic_plugin(&mut app, mgr, "NDColorConvertConfigure", |port_name, queue_size, ndarray_port, pool| {
        use crate::color_convert::{ColorConvertConfig, ColorConvertProcessor};
        use ad_core::color::{NDColorMode, NDBayerPattern};
        let config = ColorConvertConfig { target_mode: NDColorMode::Mono, bayer_pattern: NDBayerPattern::RGGB, false_color: false };
        create_plugin_runtime(port_name, ColorConvertProcessor::new(config), pool, queue_size, ndarray_port)
    });
    app = register_generic_plugin(&mut app, mgr, "NDOverlayConfigure", |port_name, queue_size, ndarray_port, pool| {
        use crate::overlay::OverlayProcessor;
        create_plugin_runtime(port_name, OverlayProcessor::new(vec![]), pool, queue_size, ndarray_port)
    });
    app = register_generic_plugin(&mut app, mgr, "NDFFTConfigure", |port_name, queue_size, ndarray_port, pool| {
        use crate::fft::{FFTMode, FFTProcessor};
        create_plugin_runtime(port_name, FFTProcessor::new(FFTMode::Rows1D), pool, queue_size, ndarray_port)
    });
    app = register_generic_plugin(&mut app, mgr, "NDCircularBuffConfigure", |port_name, queue_size, ndarray_port, pool| {
        use crate::circular_buff::{CircularBuffProcessor, TriggerCondition};
        create_plugin_runtime(port_name, CircularBuffProcessor::new(100, 100, TriggerCondition::External), pool, queue_size, ndarray_port)
    });
    app = register_generic_plugin(&mut app, mgr, "NDCodecConfigure", |port_name, queue_size, ndarray_port, pool| {
        use crate::codec::{CodecMode, CodecProcessor};
        use ad_core::codec::CodecName;
        create_plugin_runtime(port_name, CodecProcessor::new(CodecMode::Compress { codec: CodecName::LZ4, quality: 90 }), pool, queue_size, ndarray_port)
    });
    app = register_generic_plugin(&mut app, mgr, "NDScatterConfigure", |port_name, queue_size, ndarray_port, pool| {
        use crate::scatter::ScatterProcessor;
        create_plugin_runtime(port_name, ScatterProcessor::new(), pool, queue_size, ndarray_port)
    });
    app = register_generic_plugin(&mut app, mgr, "NDGatherConfigure", |port_name, queue_size, ndarray_port, pool| {
        use crate::gather::GatherProcessor;
        create_plugin_runtime(port_name, GatherProcessor::new(), pool, queue_size, ndarray_port)
    });
    app = register_generic_plugin(&mut app, mgr, "NDFileTIFFConfigure", |port_name, queue_size, ndarray_port, pool| {
        use crate::file_tiff::TiffFileProcessor;
        create_plugin_runtime(port_name, TiffFileProcessor::new(), pool, queue_size, ndarray_port)
    });
    app = register_generic_plugin(&mut app, mgr, "NDFileJPEGConfigure", |port_name, queue_size, ndarray_port, pool| {
        use crate::file_jpeg::JpegFileProcessor;
        create_plugin_runtime(port_name, JpegFileProcessor::new(90), pool, queue_size, ndarray_port)
    });
    app = register_generic_plugin(&mut app, mgr, "NDFileHDF5Configure", |port_name, queue_size, ndarray_port, pool| {
        use crate::file_hdf5::Hdf5FileProcessor;
        create_plugin_runtime(port_name, Hdf5FileProcessor::new(), pool, queue_size, ndarray_port)
    });

    // --- Stub plugins (not yet fully implemented, use PassthroughProcessor) ---
    for name in &[
        "NDROIStatConfigure",
        "NDAttrConfigure",
        "NDBadPixelConfigure",
        "NDFileNetCDFConfigure",
        "NDFileNexusConfigure",
        "NDFileMagickConfigure",
        "NDTimeSeriesConfigure",
        "NDPvaConfigure",
    ] {
        let cmd_name = *name;
        let m = mgr.clone();
        app = app.register_startup_command(CommandDef::new(
            cmd_name,
            plugin_arg_defs(),
            &format!("{cmd_name} portName [queueSize] ... (stub)"),
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let drv = m.driver()?;
                let pool = drv.pool();
                use crate::passthrough::PassthroughProcessor;
                let (handle, _jh) = create_plugin_runtime(
                    &port_name,
                    PassthroughProcessor::new(cmd_name),
                    pool,
                    queue_size,
                    &ndarray_port,
                );
                drv.connect_downstream(handle.array_sender().clone());
                println!("{cmd_name}: port={port_name} (stub)");
                m.add_plugin(&dtyp, &handle, None);
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    app
}

/// Helper: register a generic plugin configure command that follows the standard pattern.
fn register_generic_plugin<F>(
    app: &mut IocApplication,
    mgr: &Arc<PluginManager>,
    cmd_name: &'static str,
    factory: F,
) -> IocApplication
where
    F: Fn(
            &str,
            usize,
            &str,
            Arc<ad_core::ndarray_pool::NDArrayPool>,
        ) -> (
            ad_core::plugin::runtime::PluginRuntimeHandle,
            std::thread::JoinHandle<()>,
        ) + Send
        + Sync
        + 'static,
{
    let m = mgr.clone();
    // Take ownership of app temporarily via a dummy
    let taken = std::mem::replace(app, IocApplication::new());
    let result = taken.register_startup_command(CommandDef::new(
        cmd_name,
        plugin_arg_defs(),
        &format!("{cmd_name} portName [queueSize] ..."),
        move |args: &[ArgValue], _ctx: &CommandContext| {
            let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
            let dtyp = dtyp_from_port(&port_name);
            let drv = m.driver()?;
            let pool = drv.pool();
            let (handle, _jh) = factory(&port_name, queue_size, &ndarray_port, pool);
            drv.connect_downstream(handle.array_sender().clone());
            println!("{cmd_name}: port={port_name}");
            m.add_plugin(&dtyp, &handle, None);
            Ok(CommandOutcome::Continue)
        },
    ));
    result
}

// ============================================================================
// AdIoc — Pre-configured IOC application for areaDetector-based systems
// ============================================================================

/// A pre-configured IOC application for areaDetector-based systems.
///
/// Handles all common boilerplate:
/// - `IocApplication` creation with CA server port
/// - `TraceManager` and `PluginManager`
/// - `asynRecord` registration
/// - All NDPlugin configure commands (`NDStdArraysConfigure`, `NDStatsConfigure`, etc.)
/// - No-op commands from commonPlugins.cmd
/// - Plugin device support (dynamic DTYP dispatch)
/// - Report shell command
///
/// Detector libraries register their configure commands and device support
/// via [`register_startup_command`] and [`register_device_support`], then
/// call [`run_from_args`] to start the IOC.
///
/// # Example
///
/// ```rust,ignore
/// #[tokio::main]
/// async fn main() -> CaResult<()> {
///     epics_base_rs::runtime::env::set_default("MYDET", env!("CARGO_MANIFEST_DIR"));
///
///     let mut ioc = AdIoc::new();
///     my_detector::ioc_support::register(&mut ioc);
///     ioc.run_from_args().await
/// }
/// ```
pub struct AdIoc {
    app: Option<IocApplication>,
    mgr: Arc<PluginManager>,
    trace: Arc<TraceManager>,
}

impl AdIoc {
    /// Create a new AdIoc with default configuration.
    pub fn new() -> Self {
        let trace = Arc::new(TraceManager::new());
        let mgr = PluginManager::new(trace.clone());

        asyn_rs::asyn_record::register_asyn_record_type();

        let app = IocApplication::new().port(
            std::env::var("EPICS_CA_SERVER_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5064),
        );

        // Set ADCORE path for commonPlugins.cmd resolution
        epics_base_rs::runtime::env::set_default(
            "ADCORE",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../ad-core"),
        );

        Self { app: Some(app), mgr, trace }
    }

    /// Access the shared `PluginManager`.
    pub fn mgr(&self) -> &Arc<PluginManager> {
        &self.mgr
    }

    /// Access the shared `TraceManager`.
    pub fn trace(&self) -> &Arc<TraceManager> {
        &self.trace
    }

    /// Register a startup command (e.g., detector configure command).
    pub fn register_startup_command(&mut self, cmd: CommandDef) {
        let app = self.app.take().unwrap();
        self.app = Some(app.register_startup_command(cmd));
    }

    /// Register a static device support factory for a fixed DTYP name.
    pub fn register_device_support<F>(&mut self, dtyp: &str, factory: F)
    where
        F: Fn() -> Box<dyn epics_base_rs::server::device_support::DeviceSupport>
            + Send
            + Sync
            + 'static,
    {
        let app = self.app.take().unwrap();
        self.app = Some(app.register_device_support(dtyp, factory));
    }

    /// Register a dynamic device support factory (dispatches by DTYP name).
    pub fn register_dynamic_device_support<F>(&mut self, factory: F)
    where
        F: Fn(&str) -> Option<Box<dyn epics_base_rs::server::device_support::DeviceSupport>>
            + Send
            + Sync
            + 'static,
    {
        let app = self.app.take().unwrap();
        self.app = Some(app.register_dynamic_device_support(factory));
    }

    /// Register a shell command.
    pub fn register_shell_command(&mut self, cmd: CommandDef) {
        let app = self.app.take().unwrap();
        self.app = Some(app.register_shell_command(cmd));
    }

    /// Register an inline EPICS record.
    pub fn record(&mut self, name: &str, record: impl epics_base_rs::server::record::Record) {
        let app = self.app.take().unwrap();
        self.app = Some(app.record(name, record));
    }

    /// Parse command-line args for the startup script path and run.
    pub async fn run_from_args(self) -> CaResult<()> {
        let args: Vec<String> = std::env::args().collect();
        let script = if args.len() > 1 && !args[1].starts_with('-') {
            args[1].clone()
        } else {
            let bin = args.first().map(|s| s.as_str()).unwrap_or("ioc");
            eprintln!("Usage: {bin} <st.cmd>");
            std::process::exit(1);
        };
        self.run(&script).await
    }

    /// Run the IOC with a given startup script path.
    pub async fn run(self, script: &str) -> CaResult<()> {
        let mut app = self.app.unwrap();

        // Register all standard plugin configure commands
        app = register_all_plugins(app, &self.mgr);
        app = register_noop_commands(app);

        // Plugin device support (dynamic DTYP dispatch)
        app = self.mgr.register_device_support(app);

        // asynReport shell command
        let mgr_r = self.mgr.clone();
        app = app.register_shell_command(CommandDef::new(
            "asynReport",
            vec![ArgDesc { name: "level", arg_type: ArgType::Int, optional: true }],
            "asynReport [level] - Report registered ports and plugins",
            move |_args: &[ArgValue], _ctx: &CommandContext| {
                mgr_r.report();
                Ok(CommandOutcome::Continue)
            },
        ));

        app.startup_script(script).run().await
    }
}
