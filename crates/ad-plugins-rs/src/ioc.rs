//! IOC plugin registration and areaDetector IOC application framework.
//!
//! Provides:
//! - [`register_all_plugins`]: registers startup commands like
//!   `NDStatsConfigure`, `NDROIConfigure`, etc. on an `IocApplication`.
//! - [`AdIoc`]: pre-configured IOC application that handles all common
//!   areaDetector boilerplate (plugins, device support, asynRecord, etc.).

use std::sync::{Arc, Mutex};

use ad_core_rs::ioc::{
    PluginManager, attr_arg_defs, dtyp_from_port, extract_plugin_args, max_threads_arg,
    plugin_arg_defs, plugin_arg_defs_with_count, register_noop_commands,
};
use ad_core_rs::plugin::runtime::{create_plugin_runtime, create_plugin_runtime_multi_addr};
use ad_core_rs::plugin::wiring::WiringRegistry;
use asyn_rs::manager::PortManager;
use asyn_rs::trace::TraceManager;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::iocsh::registry::*;
use epics_ca_rs::server::ioc_app::IocApplication;

use crate::attr_plot_args::{AttrPlotArgs, parse_attr_plot_args};

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
                handle.set_max_threads(max_threads_arg(args, 9));
                if let Err(e) = m.add_plugin(&dtyp, &handle) {
                    eprintln!("NDStdArraysConfigure: {e}");
                    return Ok(CommandOutcome::Continue);
                }
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
                handle.set_max_threads(max_threads_arg(args, 9));

                if let Err(e) = m.add_plugin(&dtyp, &handle) {
                    eprintln!("NDStatsConfigure: {e}");
                    return Ok(CommandOutcome::Continue);
                }
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
                handle.set_max_threads(max_threads_arg(args, 9));
                if let Err(e) = m.add_plugin(&dtyp, &handle) {
                    eprintln!("NDROIConfigure: {e}");
                    return Ok(CommandOutcome::Continue);
                }
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
        plugin_arg_defs(),
        None,
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
        plugin_arg_defs(),
        Some(9),
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
        plugin_arg_defs(),
        Some(9),
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
        plugin_arg_defs_with_count("maxOverlays"),
        Some(10),
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
        plugin_arg_defs(),
        Some(9),
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::fft::FFTProcessor;
            create_plugin_runtime(
                port_name,
                FFTProcessor::new(),
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
        plugin_arg_defs(),
        None,
        |port_name, queue_size, ndarray_port, pool, wiring| {
            use crate::circular_buff::{CircularBuffProcessor, TriggerCondition};
            create_plugin_runtime(
                port_name,
                // queue_size is C `maxBuffers_` (NDCircularBuffConfigure
                // queueSize); the processor bounds the accepted pre-count by it.
                CircularBuffProcessor::new(100, 100, TriggerCondition::External, queue_size),
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
        plugin_arg_defs(),
        Some(9),
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
        plugin_arg_defs(),
        None,
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

                if let Err(e) = m.add_plugin(&dtyp, &handle) {
                    eprintln!("NDGatherConfigure: {e}");
                    return Ok(CommandOutcome::Continue);
                }
                println!("NDGatherConfigure: port={port_name}");
                Ok(CommandOutcome::Continue)
            },
        ));
    }
    // NDAttrPlotConfig: portName nAttributes cacheSize nDataBlocks NDArrayPort
    //                   [NDArrayAddr] [queueSize] [blockingCallbacks] [priority] [stackSize]
    // C arg order (NDPluginAttrPlot.cpp:308): port, n_attributes, cache_size,
    // n_selected_blocks, in_port, in_addr, queue_size, ... — distinct from the
    // generic portName/queueSize/blockingCallbacks/NDArrayPort layout, so this
    // command parses its own positional args. The plugin tracks up to
    // n_attributes numeric attributes and exposes n_data_blocks waveform blocks,
    // so the asyn port needs max(n_attributes, n_data_blocks) addresses (C uses
    // the same max, NDPluginAttrPlot.cpp:48).
    {
        let m = mgr.clone();
        let int = |name| ArgDesc {
            name,
            arg_type: ArgType::Int,
        };
        let arg_defs = vec![
            ArgDesc {
                name: "portName",
                arg_type: ArgType::String,
            },
            int("nAttributes"),
            int("cacheSize"),
            int("nDataBlocks"),
            ArgDesc {
                name: "NDArrayPort",
                arg_type: ArgType::String,
            },
            int("NDArrayAddr"),
            int("queueSize"),
            int("blockingCallbacks"),
            int("priority"),
            int("stackSize"),
        ];
        let taken = std::mem::replace(&mut app, IocApplication::new());
        app = taken.register_startup_command(CommandDef::new(
            "NDAttrPlotConfig",
            arg_defs,
            "NDAttrPlotConfig portName nAttributes cacheSize nDataBlocks NDArrayPort \
             [NDArrayAddr] [queueSize] [blockingCallbacks] [priority] [stackSize]"
                .to_string(),
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let AttrPlotArgs {
                    port_name,
                    n_attributes,
                    cache_size,
                    n_data_blocks,
                    in_port,
                    queue_size,
                } = parse_attr_plot_args(args)?;

                let dtyp = dtyp_from_port(&port_name);
                if asyn_rs::asyn_record::get_port(&port_name).is_some() {
                    println!("NDAttrPlotConfig: port={port_name} already configured, skipping");
                    return Ok(CommandOutcome::Continue);
                }
                let drv = m.driver()?;
                let pool = drv.pool();
                // Addr 0 always exists, so at least one address even for a
                // degenerate 0/0 request.
                let max_addr = n_attributes.max(n_data_blocks).max(1);
                let (handle, _jh) = create_plugin_runtime_multi_addr(
                    &port_name,
                    crate::attr_plot::AttrPlotProcessor::new(
                        n_attributes,
                        cache_size,
                        n_data_blocks,
                    ),
                    pool,
                    queue_size,
                    &in_port,
                    m.wiring().clone(),
                    max_addr,
                );
                if let Err(e) = m.add_plugin(&dtyp, &handle) {
                    eprintln!("NDAttrPlotConfig: {e}");
                    return Ok(CommandOutcome::Continue);
                }
                if let Err(e) = m.wiring().rewire(handle.array_sender(), "", &in_port) {
                    eprintln!("NDAttrPlotConfig: wiring failed: {e}");
                }
                println!("NDAttrPlotConfig: port={port_name}");
                Ok(CommandOutcome::Continue)
            },
        ));
    }
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDFileTIFFConfigure",
        plugin_arg_defs(),
        None,
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
        plugin_arg_defs(),
        None,
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
        plugin_arg_defs(),
        None,
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
        plugin_arg_defs(),
        None,
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
        plugin_arg_defs(),
        None,
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
            attr_arg_defs(),
            "NDAttrConfigure portName [queueSize] ...",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
                // C arg index 5 is maxAttributes (NDPluginAttribute.cpp:245); a
                // missing iocsh int arg defaults to 0, which C floors to 1.
                let max_attributes = match args.get(5) {
                    Some(ArgValue::Int(n)) => *n as i32,
                    _ => 0,
                };
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
                    max_attributes,
                );
                if let Err(e) = m.add_plugin(&dtyp, &handle) {
                    eprintln!("NDAttrConfigure: {e}");
                    return Ok(CommandOutcome::Continue);
                }
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
            plugin_arg_defs_with_count("maxROIs"),
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
                handle.set_max_threads(max_threads_arg(args, 10));
                if let Err(e) = m.add_plugin(&dtyp, &handle) {
                    eprintln!("NDROIStatConfigure: {e}");
                    return Ok(CommandOutcome::Continue);
                }
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
        plugin_arg_defs(),
        Some(9),
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
        plugin_arg_defs(),
        None,
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
                let (port_name, queue_size, ndarray_port) = extract_plugin_args(args)?;
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
                        // No upstream TS receiver registered: the upstream is a raw
                        // NDArray source, so stand up the standalone
                        // NDPluginTimeSeries which ingests the arrays directly
                        // (C NDPluginTimeSeries.cpp). C's NDTimeSeriesConfigure
                        // takes maxSignals at arg index 5.
                        let max_signals = match args.get(5) {
                            Some(ArgValue::Int(n)) if *n >= 1 => *n as usize,
                            _ => 1,
                        };
                        let drv = m.driver()?;
                        let pool = drv.pool();
                        let (handle, _jh) = create_plugin_runtime_multi_addr(
                            &port_name,
                            crate::time_series_plugin::TimeSeriesProcessor::new(max_signals),
                            pool,
                            queue_size,
                            &ndarray_port,
                            m.wiring().clone(),
                            // C maxAddr = maxSignals + 1 (the 2-D array callback
                            // address is reserved at addr == maxSignals).
                            max_signals + 1,
                        );
                        if let Err(e) = m.add_plugin(&dtyp, &handle) {
                            eprintln!("NDTimeSeriesConfigure: {e}");
                            return Ok(CommandOutcome::Continue);
                        }
                        if let Err(e) = m.wiring().rewire(handle.array_sender(), "", &ndarray_port)
                        {
                            eprintln!("NDTimeSeriesConfigure: wiring failed: {e}");
                        }
                        println!(
                            "NDTimeSeriesConfigure: port={port_name} \
                             (standalone raw-array, maxSignals={max_signals}, \
                             upstream={ndarray_port})"
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
                if let Err(e) = m.add_port(&dtyp, ts_runtime) {
                    eprintln!("NDTimeSeriesConfigure: {e}");
                    return Ok(CommandOutcome::Continue);
                }
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
                // If not provided, fall back to "{portName}:Image". Only the
                // `pva`-feature processor consumes it, so gate the binding to
                // avoid an unused-variable warning when `ioc` is built without `pva`.
                #[cfg(feature = "pva")]
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
                if let Err(e) = m.add_plugin(&dtyp, &handle) {
                    eprintln!("NDPvaConfigure: {e}");
                    return Ok(CommandOutcome::Continue);
                }
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

/// Register one plugin `*Configure` command whose only arguments this crate
/// reads are `portName`, `queueSize` and `NDArrayPort`.
///
/// `arg_defs` and `max_threads_idx` describe one C signature between them and
/// have to be given together: the index is only reachable if `arg_defs` is
/// long enough to accept an argument there, so a command carrying
/// [`plugin_arg_defs_with_count`] is exactly a command whose `maxThreads` is
/// at 10. `None` is for a C signature with no `maxThreads` at all — argument 9
/// is `stackSize` there, and [`max_threads_arg`] must not be pointed at it.
fn register_generic_plugin<F>(
    app: &mut IocApplication,
    mgr: &Arc<PluginManager>,
    cmd_name: &'static str,
    arg_defs: Vec<ArgDesc>,
    max_threads_idx: Option<usize>,
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
        arg_defs,
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
            if let Some(idx) = max_threads_idx {
                handle.set_max_threads(max_threads_arg(args, idx));
            }
            if let Err(e) = m.add_plugin(&dtyp, &handle) {
                eprintln!("{cmd_name}: {e}");
                return Ok(CommandOutcome::Continue);
            }
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
    ports: Arc<PortManager>,
    trace: Arc<TraceManager>,
    /// Resources kept alive for the IOC's lifetime (e.g. driver runtimes).
    _resources: Vec<Box<dyn std::any::Any + Send>>,
}

impl AdIoc {
    /// Create a new AdIoc with default configuration.
    pub fn new() -> Self {
        let trace = Arc::new(TraceManager::new());
        let mgr = PluginManager::new(trace.clone());
        // The asyn iocsh commands resolve ports and mutate trace state through
        // this manager, so it shares the IOC's `TraceManager` — a manager with
        // a `TraceManager` of its own would make `asynSetTrace*` mutate state
        // that no driver or plugin ever reads.
        let ports = Arc::new(PortManager::with_trace_manager(trace.clone()));

        asyn_rs::asyn_record::register_asyn_record_type();

        // `IocApplication::new()` already resolves the server port through
        // C `envGetInetPortConfigParam` (`runtime::net::cas_server_port`);
        // re-reading EPICS_CA_SERVER_PORT here with a strict parse dropped
        // that parity (no 5000 floor, no sscanf leniency, no diagnostics).
        let mut app = IocApplication::new();

        // `$(ADCORE)` resolves `$(ADCORE)/ioc/commonPlugins.cmd` and the
        // `$(ADCORE)/db` templates. Take the path from ad-core-rs itself rather
        // than guessing a sibling of this crate: only the owning crate can name
        // its own directory under a version-suffixed registry checkout.
        epics_base_rs::runtime::env::set_default("ADCORE", ad_core_rs::AD_CORE_DIR);

        // Everything the IOC always needs is wired here, once. The protocol
        // runner is the only thing `run` and `run_with_pva` disagree about;
        // configuring the application separately per runner is how a command
        // set ends up present on one path and missing from the other.
        app = register_all_plugins(app, &mgr);
        app = register_noop_commands(app);
        app = app.autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())));

        // Universal asyn device support — handles all standard asyn DTYPs
        // (asynInt32, asynFloat64, asynOctet, array types) via @asyn() links.
        app = asyn_rs::adapter::register_asyn_device_support(app);

        // The asyn iocsh command set: port creation (`drvAsynIPPortConfigure`
        // and friends), `asynOctetSetInputEos` / `asynOctetSetOutputEos`,
        // `asynSetOption`, `asynReport` and the trace mutators — on the startup
        // shell as well as the interactive one. A socket detector's st.cmd
        // creates its port and sets the EOS before `iocInit`.
        app = asyn_rs::iocsh::register_asyn_commands(app, ports.clone());

        // ad-core-rs's own templates use these record types — NDArrayBase
        // (so every plugin) declares `busy` fields, NDStats/NDProcess `sseq`
        // ones — the way every C ADCore IOC links busySupport.dbd and
        // calcSupport.dbd through commonDriverMakefile:303-310. Both were
        // dropped from the default registry with the stdRecords.dbd
        // manifest, so an AdIoc has to bring them back itself; a later
        // `register_record_type` call with the same name still overrides.
        app = app.register_record_type("busy", || {
            Box::new(epics_base_rs::server::records::busy::BusyRecord::default())
        });
        app = app.register_record_type("sseq", || {
            Box::new(epics_base_rs::server::records::sseq::SseqRecord::default())
        });

        Self {
            app: Some(app),
            mgr,
            ports,
            trace,
            _resources: Vec::new(),
        }
    }

    /// Access the shared `PluginManager`.
    pub fn mgr(&self) -> &Arc<PluginManager> {
        &self.mgr
    }

    /// Access the shared [`PortManager`] — the manager the asyn iocsh commands
    /// resolve against.
    ///
    /// A detector crate registers its driver port here (or through the
    /// `drvAsyn*PortConfigure` iocsh commands, which publish to the same
    /// registry) so that `asynReport`, `asynSetOption` and the EOS commands can
    /// act on it from st.cmd.
    pub fn ports(&self) -> &Arc<PortManager> {
        &self.ports
    }

    /// Access the shared `TraceManager`.
    pub fn trace(&self) -> &Arc<TraceManager> {
        &self.trace
    }

    /// The configured [`IocApplication`] this IOC will run.
    ///
    /// `AdIoc` wires the application at construction, so
    /// [`IocApplication::startup_commands`] here is exactly the surface the
    /// startup script will be executed against.
    pub fn app(&self) -> &IocApplication {
        self.app
            .as_ref()
            .expect("AdIoc application is taken only by run()")
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
        let app = self.app.unwrap();

        // The runner paired with C's `rsrvRegistrar`, because `casr` has to
        // answer from the script's first line and `.run(runner)` reaches only
        // the interactive shell.
        epics_ca_rs::server::run_ca_ioc_app(
            app.startup_script(script)
                // CA links resolve with zero further setup: the `ca` link set
                // installs at the base `AfterCaLinkInit` hook, before
                // `setup_cp_links` warms Passive CP holders.
                .register_link_set_installer(epics_ca_rs::calink::calink_link_set_install),
        )
        .await
    }

    /// Run the IOC with both CA and PVA protocols (QSRV bridge).
    ///
    /// Same as [`Self::run`] but uses
    /// `epics_bridge_rs::qsrv::run_ca_pva_qsrv_ioc_app` — named in a code span
    /// rather than linked because it is `tokio_backend`-only and this doc is
    /// rendered in configurations that do not have it —
    /// as the protocol runner, serving records over CA (default port 5064) and
    /// pvAccess (default port 5075) simultaneously. PVA plugin PVs (NTNDArray)
    /// registered during st.cmd are wired into the PVA server automatically.
    #[cfg(feature = "pva")]
    pub async fn run_with_pva(self, script: &str) -> CaResult<()> {
        let app = self.app.unwrap();

        // Runner plus both protocol registrars — see [`Self::run`].
        epics_bridge_rs::qsrv::run_ca_pva_qsrv_ioc_app(
            app.startup_script(script)
                // External links resolve with zero further setup: both link
                // sets install at the base `AfterCaLinkInit` hook — before
                // `setup_cp_links` warms Passive CP holders and before the
                // iocInit external-link wait, both `ca` and `pva`.
                .register_link_set_installer(epics_ca_rs::calink::calink_link_set_install)
                .register_link_set_installer(epics_bridge_rs::qsrv::pvalink_link_set_install),
        )
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
