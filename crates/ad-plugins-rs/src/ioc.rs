//! IOC plugin registration and areaDetector IOC application framework.
//!
//! Provides:
//! - [`register_all_plugins`]: registers startup commands like
//!   `NDStatsConfigure`, `NDROIConfigure`, etc. on an `IocApplication`.
//! - [`AdIoc`]: pre-configured IOC application that handles all common
//!   areaDetector boilerplate (plugins, device support, asynRecord, etc.).

use std::sync::{Arc, Mutex};

use ad_core_rs::ioc::{
    PluginManager, dtyp_from_port, extract_plugin_args, plugin_arg_defs, register_noop_commands,
};
use ad_core_rs::plugin::runtime::create_plugin_runtime;
use ad_core_rs::plugin::wiring::WiringRegistry;
use asyn_rs::trace::TraceManager;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::iocsh::registry::*;
use epics_ca_rs::server::ioc_app::IocApplication;

/// Register all standard areaDetector plugin configure commands.
///
/// The `PluginManager` must have its driver context set (via `set_driver()`)
/// before any of these commands are invoked from st.cmd.
pub fn register_all_plugins(mut app: IocApplication, mgr: &Arc<PluginManager>) -> IocApplication {
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
                let (handle, _data, _jh) = crate::std_arrays::create_std_arrays_runtime(
                    &port_name,
                    pool,
                    &ndarray_port,
                    m.wiring().clone(),
                );
                m.add_plugin(&dtyp, &handle);
                if let Err(e) = m.wiring().rewire(handle.array_sender(), "", &ndarray_port) {
                    eprintln!("NDStdArraysConfigure: wiring failed: {e}");
                }
                println!("NDStdArraysConfigure: port={port_name}");
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
                let (
                    handle,
                    _stats,
                    _stats_params,
                    ts_runtime,
                    _ts_params,
                    _jh,
                    _ts_actor_jh,
                    _ts_data_jh,
                ) = crate::stats::create_stats_runtime(
                    &port_name,
                    pool,
                    queue_size,
                    &ndarray_port,
                    m.wiring().clone(),
                );
                println!("NDStatsConfigure: port={port_name}");

                m.add_plugin(&dtyp, &handle);
                if let Err(e) = m.wiring().rewire(handle.array_sender(), "", &ndarray_port) {
                    eprintln!("NDStatsConfigure: wiring failed: {e}");
                }

                // Register TimeSeries as a separate asyn port
                let ts_port_name = format!("{port_name}_TS");
                let ts_dtyp = dtyp_from_port(&ts_port_name);
                m.add_port(&ts_dtyp, ts_runtime);
                println!("  TimeSeries port: {ts_port_name} (DTYP: {ts_dtyp})");

                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- Generic plugins using create_plugin_runtime ---
    {
        let m = mgr.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDROIConfigure",
            plugin_arg_defs(),
            "NDROIConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let drv = m.driver()?;
                let pool = drv.pool();
                let (handle, _roi_params, _jh) = crate::roi::create_roi_runtime(
                    &port_name,
                    pool,
                    queue_size,
                    &ndarray_port,
                    m.wiring().clone(),
                );
                m.add_plugin(&dtyp, &handle);
                if let Err(e) = m.wiring().rewire(handle.array_sender(), "", &ndarray_port) {
                    eprintln!("NDROIConfigure: wiring failed: {e}");
                }
                println!("NDROIConfigure: port={port_name}");
                Ok(CommandOutcome::Continue)
            },
        ));
    }
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDProcessConfigure",
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::process::{ProcessConfig, ProcessProcessor};
            create_plugin_runtime(
                port_name,
                ProcessProcessor::new(ProcessConfig::default()),
                pool,
                queue_size,
                ndarray_port,
                wiring,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDTransformConfigure",
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::transform::{TransformProcessor, TransformType};
            create_plugin_runtime(
                port_name,
                TransformProcessor::new(TransformType::None),
                pool,
                queue_size,
                ndarray_port,
                wiring,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDColorConvertConfigure",
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::color_convert::{ColorConvertConfig, ColorConvertProcessor};
            use ad_core_rs::color::{NDBayerPattern, NDColorMode};
            let config = ColorConvertConfig {
                target_mode: NDColorMode::Mono,
                bayer_pattern: NDBayerPattern::RGGB,
                false_color: false,
            };
            create_plugin_runtime(
                port_name,
                ColorConvertProcessor::new(config),
                pool,
                queue_size,
                ndarray_port,
                wiring,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDOverlayConfigure",
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::overlay::OverlayProcessor;
            create_plugin_runtime(
                port_name,
                OverlayProcessor::new(vec![]),
                pool,
                queue_size,
                ndarray_port,
                wiring,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDFFTConfigure",
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::fft::{FFTMode, FFTProcessor};
            create_plugin_runtime(
                port_name,
                FFTProcessor::new(FFTMode::Rows1D),
                pool,
                queue_size,
                ndarray_port,
                wiring,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDCircularBuffConfigure",
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::circular_buff::{CircularBuffProcessor, TriggerCondition};
            create_plugin_runtime(
                port_name,
                CircularBuffProcessor::new(100, 100, TriggerCondition::External),
                pool,
                queue_size,
                ndarray_port,
                wiring,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDCodecConfigure",
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::codec::{CodecMode, CodecProcessor};
            use ad_core_rs::codec::CodecName;
            create_plugin_runtime(
                port_name,
                CodecProcessor::new(CodecMode::Compress {
                    codec: CodecName::LZ4,
                    quality: 90,
                }),
                pool,
                queue_size,
                ndarray_port,
                wiring,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDScatterConfigure",
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::scatter::ScatterProcessor;
            create_plugin_runtime(
                port_name,
                ScatterProcessor::new(),
                pool,
                queue_size,
                ndarray_port,
                wiring,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDGatherConfigure",
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::gather::GatherProcessor;
            create_plugin_runtime(
                port_name,
                GatherProcessor::new(),
                pool,
                queue_size,
                ndarray_port,
                wiring,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDFileTIFFConfigure",
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::file_tiff::TiffFileProcessor;
            create_plugin_runtime(
                port_name,
                TiffFileProcessor::new(),
                pool,
                queue_size,
                ndarray_port,
                wiring,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDFileJPEGConfigure",
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::file_jpeg::JpegFileProcessor;
            create_plugin_runtime(
                port_name,
                JpegFileProcessor::new(90),
                pool,
                queue_size,
                ndarray_port,
                wiring,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDFileHDF5Configure",
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::file_hdf5::Hdf5FileProcessor;
            create_plugin_runtime(
                port_name,
                Hdf5FileProcessor::new(),
                pool,
                queue_size,
                ndarray_port,
                wiring,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDFileNetCDFConfigure",
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::file_netcdf::NetcdfFileProcessor;
            create_plugin_runtime(
                port_name,
                NetcdfFileProcessor::new(),
                pool,
                queue_size,
                ndarray_port,
                wiring,
            )
        },
    );

    // --- NDAttrConfigure (stub with TS port) ---
    {
        let m = mgr.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDAttrConfigure",
            plugin_arg_defs(),
            "NDAttrConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let drv = m.driver()?;
                let pool = drv.pool();
                use crate::passthrough::PassthroughProcessor;
                let (handle, _jh) = create_plugin_runtime(
                    &port_name,
                    PassthroughProcessor::new("NDAttrConfigure"),
                    pool,
                    queue_size,
                    &ndarray_port,
                    m.wiring().clone(),
                );
                m.add_plugin(&dtyp, &handle);
                if let Err(e) = m.wiring().rewire(handle.array_sender(), "", &ndarray_port) {
                    eprintln!("NDAttrConfigure: wiring failed: {e}");
                }
                println!("NDAttrConfigure: port={port_name}");

                // Register TimeSeries as a separate asyn port (stub — no data flow)
                let ts_port_name = format!("{port_name}_TS");
                let ts_dtyp = dtyp_from_port(&ts_port_name);
                let (_ts_tx, ts_rx) = tokio::sync::mpsc::channel(16);
                let (ts_runtime, _ts_params, _ts_actor_jh, _ts_data_jh) =
                    crate::time_series::create_ts_port_runtime(
                        &ts_port_name,
                        &["TSArrayValue"],
                        2048,
                        ts_rx,
                    );
                m.add_port(&ts_dtyp, ts_runtime);
                println!("  TimeSeries port: {ts_port_name} (DTYP: {ts_dtyp})");

                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDROIStatConfigure ---
    {
        let m = mgr.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDROIStatConfigure",
            plugin_arg_defs(),
            "NDROIStatConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let drv = m.driver()?;
                let pool = drv.pool();
                let (handle, _roi_stat_params, _jh) = crate::roi_stat::create_roi_stat_runtime(
                    &port_name,
                    pool,
                    queue_size,
                    &ndarray_port,
                    m.wiring().clone(),
                    8,
                );
                m.add_plugin(&dtyp, &handle);
                if let Err(e) = m.wiring().rewire(handle.array_sender(), "", &ndarray_port) {
                    eprintln!("NDROIStatConfigure: wiring failed: {e}");
                }
                println!("NDROIStatConfigure: port={port_name}");
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- Stub plugins (not yet fully implemented, use PassthroughProcessor) ---
    for name in &[
        "NDBadPixelConfigure",
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
            format!("{cmd_name} portName [queueSize] ... (stub)"),
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
                    m.wiring().clone(),
                );
                m.add_plugin(&dtyp, &handle);
                if let Err(e) = m.wiring().rewire(handle.array_sender(), "", &ndarray_port) {
                    eprintln!("{cmd_name}: wiring failed: {e}");
                }
                println!("{cmd_name}: port={port_name} (stub)");
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
            Arc<ad_core_rs::ndarray_pool::NDArrayPool>,
            Arc<WiringRegistry>,
        ) -> (
            ad_core_rs::plugin::runtime::PluginRuntimeHandle,
            std::thread::JoinHandle<()>,
        ) + Send
        + Sync
        + 'static,
{
    let m = mgr.clone();
    // Take ownership of app temporarily via a dummy
    let taken = std::mem::replace(app, IocApplication::new());
    taken.register_startup_command(CommandDef::new(
        cmd_name,
        plugin_arg_defs(),
        format!("{cmd_name} portName [queueSize] ..."),
        move |args: &[ArgValue], _ctx: &CommandContext| {
            let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
            let dtyp = dtyp_from_port(&port_name);
            let drv = m.driver()?;
            let pool = drv.pool();
            let (handle, _jh) = factory(
                &port_name,
                queue_size,
                &ndarray_port,
                pool,
                m.wiring().clone(),
            );
            m.add_plugin(&dtyp, &handle);
            if let Err(e) = m.wiring().rewire(handle.array_sender(), "", &ndarray_port) {
                eprintln!("{cmd_name}: wiring failed: {e}");
            }
            println!("{cmd_name}: port={port_name}");
            Ok(CommandOutcome::Continue)
        },
    ))
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
/// - Universal asyn device support (handles all @asyn() linked records)
/// - Report shell command
///
/// Detector libraries register their configure commands via
/// `register_startup_command`, then call `run_from_args` to start the IOC.
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
    /// Resources kept alive for the IOC's lifetime (e.g. driver runtimes).
    _resources: Vec<Box<dyn std::any::Any + Send>>,
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

        // Set crate paths for commonPlugins.cmd and .req file resolution
        epics_base_rs::runtime::env::set_default(
            "ADCORE",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../ad-core-rs"),
        );
        epics_base_rs::runtime::env::set_default(
            "CALC",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../calc"),
        );
        epics_base_rs::runtime::env::set_default(
            "BUSY",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../busy"),
        );
        epics_base_rs::runtime::env::set_default(
            "AUTOSAVE",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../autosave"),
        );

        Self {
            app: Some(app),
            mgr,
            trace,
            _resources: Vec::new(),
        }
    }

    /// Access the shared `PluginManager`.
    pub fn mgr(&self) -> &Arc<PluginManager> {
        &self.mgr
    }

    /// Access the shared `TraceManager`.
    pub fn trace(&self) -> &Arc<TraceManager> {
        &self.trace
    }

    /// Register a record type (equivalent to C EPICS dbd record type registration).
    pub fn register_record_type(
        &mut self,
        name: &str,
        factory: epics_base_rs::server::RecordFactory,
    ) {
        let app = self.app.take().unwrap();
        self.app = Some(app.register_record_type(name, move || factory()));
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

    /// Register a dynamic device support factory (dispatches by context).
    pub fn register_dynamic_device_support<F>(&mut self, factory: F)
    where
        F: Fn(
                &epics_ca_rs::server::ioc_app::DeviceSupportContext,
            ) -> Option<Box<dyn epics_base_rs::server::device_support::DeviceSupport>>
            + Send
            + Sync
            + 'static,
    {
        let app = self.app.take().unwrap();
        self.app = Some(app.register_dynamic_device_support(factory));
    }

    /// Keep a resource alive for the IOC's lifetime.
    ///
    /// Use this for driver runtimes that must not be dropped while the IOC is
    /// running. The resource is stored until `run()` returns.
    pub fn keep_alive<T: Send + 'static>(&mut self, resource: T) {
        self._resources.push(Box::new(resource));
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

        // Enable autosave startup commands (C-compatible iocsh commands)
        let autosave_config = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
        app = app.autosave_startup(autosave_config);

        // Universal asyn device support — handles all standard asyn DTYPs
        // (asynInt32, asynFloat64, asynOctet, array types) via @asyn() links.
        app = asyn_rs::adapter::register_asyn_device_support(app);

        // asynReport shell command
        let mgr_r = self.mgr.clone();
        app = app.register_shell_command(CommandDef::new(
            "asynReport",
            vec![ArgDesc {
                name: "level",
                arg_type: ArgType::Int,
                optional: true,
            }],
            "asynReport [level] - Report registered ports and plugins",
            move |_args: &[ArgValue], _ctx: &CommandContext| {
                mgr_r.report();
                Ok(CommandOutcome::Continue)
            },
        ));

        app.startup_script(script)
            .run(epics_ca_rs::server::run_ca_ioc)
            .await
    }
}
