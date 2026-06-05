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
    let ts_registry = Arc::new(crate::time_series::TsReceiverRegistry::new());

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
        let tsr = ts_registry.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDStatsConfigure",
            plugin_arg_defs(),
            "NDStatsConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let drv = m.driver()?;
                let pool = drv.pool();
                let (handle, _stats, _stats_params, _jh) = crate::stats::create_stats_runtime(
                    &port_name,
                    pool,
                    queue_size,
                    &ndarray_port,
                    m.wiring().clone(),
                    &tsr,
                );

                m.add_plugin(&dtyp, &handle);
                if let Err(e) = m.wiring().rewire(handle.array_sender(), "", &ndarray_port) {
                    eprintln!("NDStatsConfigure: wiring failed: {e}");
                }
                println!("NDStatsConfigure: port={port_name}");

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
                false_color: 0,
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
    // NDGatherConfigure: portName [queueSize] [blockingCallbacks] port1 [port2 ... portN]
    // Connects multiple upstream ports to a single Gather plugin.
    {
        let m = mgr.clone();
        let taken = std::mem::replace(&mut app, IocApplication::new());
        app = taken.register_startup_command(CommandDef::new(
            "NDGatherConfigure",
            plugin_arg_defs(),
            "NDGatherConfigure portName [queueSize] [blockingCallbacks] NDArrayPort [port2 ...]"
                .to_string(),
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, first_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                if asyn_rs::asyn_record::get_port(&port_name).is_some() {
                    println!("NDGatherConfigure: port={port_name} already configured, skipping");
                    return Ok(CommandOutcome::Continue);
                }
                let drv = m.driver()?;
                let pool = drv.pool();
                let wiring = m.wiring().clone();

                let (handle, _jh) = create_plugin_runtime(
                    &port_name,
                    crate::gather::GatherProcessor::new(),
                    pool,
                    queue_size,
                    &first_port,
                    wiring.clone(),
                );

                // Wire first upstream port
                if !first_port.is_empty() {
                    if let Err(e) = wiring.rewire(handle.array_sender(), "", &first_port) {
                        eprintln!("NDGatherConfigure: wiring to {first_port} failed: {e}");
                    }
                }

                // Wire additional upstream ports (args index 4+)
                for i in 4..args.len() {
                    if let ArgValue::String(upstream) = &args[i] {
                        if !upstream.is_empty() {
                            if let Some(upstream_output) = wiring.lookup_output(upstream) {
                                upstream_output.lock().add(handle.array_sender().clone());
                            } else {
                                eprintln!(
                                    "NDGatherConfigure: upstream port '{upstream}' not found"
                                );
                            }
                        }
                    }
                }

                m.add_plugin(&dtyp, &handle);
                println!("NDGatherConfigure: port={port_name}");
                Ok(CommandOutcome::Continue)
            },
        ));
    }
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
                JpegFileProcessor::new(50),
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
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDFileMagickConfigure",
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::file_magick::MagickFileProcessor;
            create_plugin_runtime(
                port_name,
                MagickFileProcessor::new(),
                pool,
                queue_size,
                ndarray_port,
                wiring,
            )
        },
    );

    // --- NDAttrConfigure ---
    {
        let m = mgr.clone();
        let tsr = ts_registry.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDAttrConfigure",
            plugin_arg_defs(),
            "NDAttrConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                let drv = m.driver()?;
                let pool = drv.pool();

                let (handle, _jh) = crate::attribute::create_attribute_runtime(
                    &port_name,
                    pool,
                    queue_size,
                    &ndarray_port,
                    m.wiring().clone(),
                    &tsr,
                );
                m.add_plugin(&dtyp, &handle);
                if let Err(e) = m.wiring().rewire(handle.array_sender(), "", &ndarray_port) {
                    eprintln!("NDAttrConfigure: wiring failed: {e}");
                }
                println!("NDAttrConfigure: port={port_name}");

                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDROIStatConfigure ---
    {
        let m = mgr.clone();
        let tsr = ts_registry.clone();
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
                    32,
                    &tsr,
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
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDBadPixelConfigure",
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::bad_pixel::BadPixelProcessor;
            create_plugin_runtime(
                port_name,
                BadPixelProcessor::new(vec![]),
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
        "NDFileNexusConfigure",
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::file_nexus::NexusFileProcessor;
            create_plugin_runtime(
                port_name,
                NexusFileProcessor::new(),
                pool,
                queue_size,
                ndarray_port,
                wiring,
            )
        },
    );

    // --- NDTimeSeriesConfigure ---
    // Picks up a pending TS receiver from the registry (stored by Stats/ROIStat/Attr)
    // and creates the TS port.
    {
        let m = mgr.clone();
        let tsr = ts_registry.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDTimeSeriesConfigure",
            plugin_arg_defs(),
            "NDTimeSeriesConfigure portName [queueSize] [blockingCallbacks] NDArrayPort",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, _queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                if asyn_rs::asyn_record::get_port(&port_name).is_some() {
                    println!(
                        "NDTimeSeriesConfigure: port={port_name} already configured, skipping"
                    );
                    return Ok(CommandOutcome::Continue);
                }

                // Look up the pending receiver from the upstream plugin
                let (ts_rx, channel_names) = match tsr.take(&ndarray_port) {
                    Some(entry) => entry,
                    None => {
                        eprintln!(
                            "NDTimeSeriesConfigure: no TS receiver for upstream '{ndarray_port}'. \
                             Ensure the upstream plugin is configured first."
                        );
                        return Ok(CommandOutcome::Continue);
                    }
                };

                let channel_name_refs: Vec<&str> =
                    channel_names.iter().map(|s| s.as_str()).collect();
                let (ts_runtime, _ts_params, _ts_actor_jh, _ts_data_jh) =
                    crate::time_series::create_ts_port_runtime(
                        &port_name,
                        &channel_name_refs,
                        2048,
                        ts_rx,
                    );
                m.add_port(&dtyp, ts_runtime);
                println!("NDTimeSeriesConfigure: port={port_name} (upstream={ndarray_port})");

                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDPvaConfigure ---
    // C++ signature: NDPvaConfigure(portName, queueSize, blockingCallbacks,
    //   NDArrayPort, NDArrayAddr, pvName, maxBuffers, maxMemory, priority, stackSize)
    // args[5] = pvName is a STRING, unlike standard plugin_arg_defs which has Int there.
    {
        let m = mgr.clone();
        let pva_arg_defs = {
            let mut defs = plugin_arg_defs();
            // Replace args[5] (maxBuffers/Int) with pvName/String
            if defs.len() > 5 {
                defs[5] = ArgDesc {
                    name: "pvName",
                    arg_type: ArgType::String,
                    optional: true,
                };
            }
            defs
        };
        app = app.register_startup_command(CommandDef::new(
            "NDPvaConfigure",
            pva_arg_defs,
            "NDPvaConfigure portName queueSize blockingCallbacks NDArrayPort NDArrayAddr pvName"
                .to_string(),
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                let dtyp = dtyp_from_port(&port_name);
                if asyn_rs::asyn_record::get_port(&port_name).is_some() {
                    println!("NDPvaConfigure: port={port_name} already configured, skipping");
                    return Ok(CommandOutcome::Continue);
                }
                let drv = m.driver()?;
                let pool = drv.pool();

                // C++ NDPvaConfigure args[5] is pvName (6th argument).
                // If not provided, fall back to "{portName}:Image".
                let pva_pv_name = match args.get(5) {
                    Some(ArgValue::String(s)) if !s.is_empty() => s.clone(),
                    _ => format!("{port_name}:Image"),
                };

                #[cfg(feature = "pva")]
                let processor = {
                    // The processor owns the validating `PvaPvHandle` (built
                    // with the canonical NTNDArray descriptor); registering a
                    // clone shares the same `latest`/`subscribers` state with
                    // the qsrv adapter. Producer posts and server reads flow
                    // through one validating owner — a frame that does not
                    // match the descriptor never becomes the served value.
                    let proc = crate::pva::PvaProcessor::new(pva_pv_name.clone());
                    epics_bridge_rs::qsrv::register_pva_pv_global(&pva_pv_name, proc.handle());
                    proc
                };
                #[cfg(not(feature = "pva"))]
                let processor = crate::passthrough::PassthroughProcessor::new("NDPvaConfigure");

                let (handle, _jh) = create_plugin_runtime(
                    &port_name,
                    processor,
                    pool,
                    queue_size,
                    &ndarray_port,
                    m.wiring().clone(),
                );
                m.add_plugin(&dtyp, &handle);
                if let Err(e) = m.wiring().rewire(handle.array_sender(), "", &ndarray_port) {
                    eprintln!("NDPvaConfigure: wiring failed: {e}");
                }
                #[cfg(feature = "pva")]
                println!("NDPvaConfigure: port={port_name}, PV={pva_pv_name}");
                #[cfg(not(feature = "pva"))]
                println!("NDPvaConfigure: port={port_name} (stub — enable 'pva' feature)");
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
            // Skip if port already exists (allows commonPlugins.cmd to be
            // loaded multiple times with different PREFIX for alias records).
            if asyn_rs::asyn_record::get_port(&port_name).is_some() {
                println!("{cmd_name}: port={port_name} already configured, skipping");
                return Ok(CommandOutcome::Continue);
            }
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
            // CA links resolve with zero further setup: the `ca` link set
            // installs at the base `AfterCaLinkInit` hook, before
            // `setup_cp_links` warms Passive CP holders
            // (R0604-CALINK-NOT-DEFAULT-WIRED-1).
            .register_link_set_installer(epics_ca_rs::calink::calink_link_set_install)
            .run(epics_ca_rs::server::run_ca_ioc)
            .await
    }

    /// Run the IOC with both CA and PVA protocols (QSRV bridge).
    ///
    /// Same as [`Self::run`] but uses [`epics_bridge_rs::qsrv::run_ca_pva_qsrv_ioc`]
    /// as the protocol runner, serving records over CA (default port 5064) and
    /// pvAccess (default port 5075) simultaneously. PVA plugin PVs (NTNDArray)
    /// registered during st.cmd are wired into the PVA server automatically.
    #[cfg(feature = "pva")]
    pub async fn run_with_pva(self, script: &str) -> CaResult<()> {
        let mut app = self.app.unwrap();

        app = register_all_plugins(app, &self.mgr);
        app = register_noop_commands(app);

        let autosave_config = Arc::new(Mutex::new(AutosaveStartupConfig::new()));
        app = app.autosave_startup(autosave_config);
        app = asyn_rs::adapter::register_asyn_device_support(app);

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
            // External links resolve with zero further setup: both link
            // sets install at the base `AfterCaLinkInit` hook — before
            // `setup_cp_links` warms Passive CP holders and before the
            // iocInit external-link wait. `ca`
            // (R0604-CALINK-NOT-DEFAULT-WIRED-1), `pva`
            // (R0604-BRPVALINK-NOT-SEAM-WIRED-1).
            .register_link_set_installer(epics_ca_rs::calink::calink_link_set_install)
            .register_link_set_installer(epics_bridge_rs::qsrv::pvalink_link_set_install)
            .run(epics_bridge_rs::qsrv::run_ca_pva_qsrv_ioc)
            .await
    }

    /// Run the IOC with PVA from command-line args.
    #[cfg(feature = "pva")]
    pub async fn run_from_args_with_pva(self) -> CaResult<()> {
        let args: Vec<String> = std::env::args().collect();
        let script = if args.len() > 1 && !args[1].starts_with('-') {
            args[1].clone()
        } else {
            let bin = args.first().map(|s| s.as_str()).unwrap_or("ioc");
            eprintln!("Usage: {bin} <st.cmd>");
            std::process::exit(1);
        };
        self.run_with_pva(&script).await
    }
}
