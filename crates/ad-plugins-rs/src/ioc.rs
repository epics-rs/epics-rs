//! IOC plugin registration and areaDetector IOC application framework.
//!
//! Provides:
//! - [`register_all_plugins`]: registers startup commands like
//!   `NDStatsConfigure`, `NDROIConfigure`, etc. on an `IocApplication`.
//! - [`AdIoc`]: pre-configured IOC application that handles all common
//!   areaDetector boilerplate (plugins, device support, asynRecord, etc.).

use std::sync::{Arc, Mutex};

use ad_core_rs::ioc::{
    PluginManager, attr_arg_defs, count_arg, extract_plugin_args, gather_arg_defs, max_memory_arg,
    max_threads_arg, plugin_arg_defs, plugin_arg_defs_with_count, register_noop_commands,
    threads_arg,
};
use ad_core_rs::plugin::runtime::{
    ArraySource, create_plugin_runtime, create_plugin_runtime_multi_addr,
};
use ad_core_rs::plugin::wiring::WiringRegistry;
use asyn_rs::asyn_record::PortClaim;
use asyn_rs::manager::{PortManager, claim_port_driver};
use asyn_rs::services::PortServices;
use asyn_rs::trace::TraceManager;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::iocsh::registry::*;
use epics_ca_rs::server::ioc_app::IocApplication;

use crate::attr_plot_args::{AttrPlotArgs, parse_attr_plot_args};

/// The `asynFlags` C's plugin hands its `NDPluginDriver` / `NDPluginFile`
/// constructor, for every `*Configure` command registered here: the command,
/// its ADCore `pluginSrc` file, and the argument as written there. NDROI's
/// `asynFlags` is `ASYN_DESTRUCTIBLE` where asyn defines it
/// (NDPluginROI.cpp:361-368).
/// C creates the port thread, at the line's `priority` and `stackSize`, only
/// for the can-block six (asynManager.c:2074).
///
/// | flags | commands |
/// |---|---|
/// | `ASYN_CANBLOCK` | NDFileTIFF, NDFileJPEG, NDFileHDF5, NDFileNetCDF, NDFileMagick, NDFileNexus |
/// | `ASYN_MULTIDEVICE` | NDGather, NDProcess, NDROI, NDScatter, NDTransform, NDAttr, NDAttrPlot, NDOverlay, NDROIStat, NDTimeSeries, NDPosPlugin |
/// | `0` | NDBadPixel, NDCodec, NDColorConvert, NDFFT, NDStats, NDStdArrays, NDPva, NDCircularBuff |
///
/// Each processor's `NDPluginProcess::ASYN_FLAGS` carries its row.
pub const C_PLUGIN_ASYN_FLAGS: [(&str, &str, &str); 25] = [
    ("NDFileTIFFConfigure", "NDFileTIFF.cpp", "ASYN_CANBLOCK"),
    ("NDFileJPEGConfigure", "NDFileJPEG.cpp", "ASYN_CANBLOCK"),
    ("NDFileHDF5Configure", "NDFileHDF5.cpp", "ASYN_CANBLOCK"),
    ("NDFileNetCDFConfigure", "NDFileNetCDF.cpp", "ASYN_CANBLOCK"),
    ("NDFileMagickConfigure", "NDFileMagick.cpp", "ASYN_CANBLOCK"),
    ("NDFileNexusConfigure", "NDFileNexus.cpp", "ASYN_CANBLOCK"),
    (
        "NDGatherConfigure",
        "NDPluginGather.cpp",
        "ASYN_MULTIDEVICE",
    ),
    (
        "NDProcessConfigure",
        "NDPluginProcess.cpp",
        "ASYN_MULTIDEVICE",
    ),
    (
        "NDROIConfigure",
        "NDPluginROI.cpp",
        "asynFlags | ASYN_MULTIDEVICE",
    ),
    (
        "NDScatterConfigure",
        "NDPluginScatter.cpp",
        "ASYN_MULTIDEVICE",
    ),
    (
        "NDTransformConfigure",
        "NDPluginTransform.cpp",
        "ASYN_MULTIDEVICE",
    ),
    (
        "NDAttrConfigure",
        "NDPluginAttribute.cpp",
        "ASYN_MULTIDEVICE",
    ),
    (
        "NDAttrPlotConfig",
        "NDPluginAttrPlot.cpp",
        "ASYN_MULTIDEVICE",
    ),
    (
        "NDOverlayConfigure",
        "NDPluginOverlay.cpp",
        "ASYN_MULTIDEVICE",
    ),
    (
        "NDROIStatConfigure",
        "NDPluginROIStat.cpp",
        "ASYN_MULTIDEVICE",
    ),
    (
        "NDTimeSeriesConfigure",
        "NDPluginTimeSeries.cpp",
        "ASYN_MULTIDEVICE",
    ),
    (
        "NDPosPluginConfigure",
        "NDPosPlugin.cpp",
        "ASYN_MULTIDEVICE",
    ),
    ("NDBadPixelConfigure", "NDPluginBadPixel.cpp", "0"),
    ("NDCodecConfigure", "NDPluginCodec.cpp", "0"),
    ("NDColorConvertConfigure", "NDPluginColorConvert.cpp", "0"),
    ("NDFFTConfigure", "NDPluginFFT.cpp", "0"),
    ("NDStatsConfigure", "NDPluginStats.cpp", "0"),
    ("NDStdArraysConfigure", "NDPluginStdArrays.cpp", "0"),
    ("NDPvaConfigure", "NDPluginPva.cpp", "0"),
    ("NDCircularBuffConfigure", "NDPluginCircularBuff.cpp", "0"),
];

/// Register all standard areaDetector plugin configure commands.
pub fn register_all_plugins(mut app: IocApplication, mgr: &Arc<PluginManager>) -> IocApplication {
    // --- NDStdArraysConfigure ---
    {
        let m = mgr.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDStdArraysConfigure",
            plugin_arg_defs(),
            "NDStdArraysConfigure portName [queueSize] ...",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let (port_name, _queue_size, source) = extract_plugin_args(args)?;
                let claim = match claim_port_driver(&port_name) {
                    Ok(claim) => claim,
                    Err(e) => return Ok(ctx.cxx_error(e)),
                };
                let (handle, _data, _jh) = crate::std_arrays::create_std_arrays_runtime(
                    &claim,
                    max_memory_arg(args, 6),
                    &source,
                    m.wiring().clone(),
                    threads_arg(args, 8),
                );
                handle.set_max_threads(max_threads_arg(args, 9));
                if let Err(e) =
                    m.add_plugin(claim, &handle, ctx.db().scan_control().interrupt_gate())
                {
                    return Ok(ctx.cxx_error(e));
                }
                if let Err(e) = handle.connect_to_array_port(m.wiring()) {
                    eprintln!("NDStdArraysConfigure: wiring failed: {e}");
                }
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
            move |args: &[ArgValue], ctx: &CommandContext| {
                let (port_name, queue_size, source) = extract_plugin_args(args)?;
                let claim = match claim_port_driver(&port_name) {
                    Ok(claim) => claim,
                    Err(e) => return Ok(ctx.cxx_error(e)),
                };
                let (handle, _stats, _stats_params, _jh) = crate::stats::create_stats_runtime(
                    &claim,
                    max_memory_arg(args, 6),
                    queue_size,
                    &source,
                    m.wiring().clone(),
                    threads_arg(args, 8),
                );
                handle.set_max_threads(max_threads_arg(args, 9));

                if let Err(e) =
                    m.add_plugin(claim, &handle, ctx.db().scan_control().interrupt_gate())
                {
                    return Ok(ctx.cxx_error(e));
                }
                if let Err(e) = handle.connect_to_array_port(m.wiring()) {
                    eprintln!("NDStatsConfigure: wiring failed: {e}");
                }

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
            move |args: &[ArgValue], ctx: &CommandContext| {
                let (port_name, queue_size, source) = extract_plugin_args(args)?;
                let claim = match claim_port_driver(&port_name) {
                    Ok(claim) => claim,
                    Err(e) => return Ok(ctx.cxx_error(e)),
                };
                let (handle, _roi_params, _jh) = crate::roi::create_roi_runtime(
                    &claim,
                    max_memory_arg(args, 6),
                    queue_size,
                    &source,
                    m.wiring().clone(),
                    threads_arg(args, 8),
                );
                handle.set_max_threads(max_threads_arg(args, 9));
                if let Err(e) =
                    m.add_plugin(claim, &handle, ctx.db().scan_control().interrupt_gate())
                {
                    return Ok(ctx.cxx_error(e));
                }
                if let Err(e) = handle.connect_to_array_port(m.wiring()) {
                    eprintln!("NDROIConfigure: wiring failed: {e}");
                }
                Ok(CommandOutcome::Continue)
            },
        ));
    }
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDProcessConfigure",
        plugin_arg_defs(),
        Some(6),
        None,
        PluginThreadArgs::Arg(8),
        |claim, queue_size, source, max_memory, wiring, threads| {
            use crate::process::{ProcessConfig, ProcessProcessor};
            create_plugin_runtime(
                claim,
                ProcessProcessor::new(ProcessConfig::default()),
                max_memory,
                queue_size,
                source,
                wiring,
                threads,
            )
        },
    );
    // NDPosPluginConfigure (NDPosPlugin.cpp:438-492): nine arguments, no
    // maxThreads; Mode starts at Discard, the unset param's 0.
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDPosPluginConfigure",
        plugin_arg_defs(),
        Some(6),
        None,
        PluginThreadArgs::Arg(8),
        |claim, queue_size, source, max_memory, wiring, threads| {
            use crate::pos_plugin::{PosMode, PosPluginProcessor};
            create_plugin_runtime(
                claim,
                PosPluginProcessor::new(PosMode::Discard),
                max_memory,
                queue_size,
                source,
                wiring,
                threads,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDTransformConfigure",
        plugin_arg_defs(),
        Some(6),
        Some(9),
        PluginThreadArgs::Arg(8),
        |claim, queue_size, source, max_memory, wiring, threads| {
            use crate::transform::{TransformProcessor, TransformType};
            create_plugin_runtime(
                claim,
                TransformProcessor::new(TransformType::None),
                max_memory,
                queue_size,
                source,
                wiring,
                threads,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDColorConvertConfigure",
        plugin_arg_defs(),
        Some(6),
        Some(9),
        PluginThreadArgs::Arg(8),
        |claim, queue_size, source, max_memory, wiring, threads| {
            use crate::color_convert::{ColorConvertConfig, ColorConvertProcessor};
            use ad_core_rs::color::{NDBayerPattern, NDColorMode};
            let config = ColorConvertConfig {
                target_mode: NDColorMode::Mono,
                bayer_pattern: NDBayerPattern::RGGB,
                false_color: 0,
            };
            create_plugin_runtime(
                claim,
                ColorConvertProcessor::new(config),
                max_memory,
                queue_size,
                source,
                wiring,
                threads,
            )
        },
    );
    // NDOverlayConfigure: maxOverlays is the port's address count
    // (NDPluginOverlay.cpp:403), one overlay per address as
    // NDOverlayN.template addresses them.
    {
        let m = mgr.clone();
        let taken = std::mem::replace(&mut app, IocApplication::new());
        app = taken.register_startup_command(CommandDef::new(
            "NDOverlayConfigure",
            plugin_arg_defs_with_count("maxOverlays"),
            "NDOverlayConfigure portName [queueSize] ...".to_string(),
            move |args: &[ArgValue], ctx: &CommandContext| {
                let (port_name, queue_size, source) = extract_plugin_args(args)?;
                let claim = match claim_port_driver(&port_name) {
                    Ok(claim) => claim,
                    Err(e) => return Ok(ctx.cxx_error(e)),
                };
                let max_overlays = count_arg(args, 5);
                let (handle, _jh) = create_plugin_runtime_multi_addr(
                    &claim,
                    crate::overlay::OverlayProcessor::new(max_overlays, vec![]),
                    max_memory_arg(args, 7),
                    queue_size,
                    &source,
                    m.wiring().clone(),
                    max_overlays,
                    threads_arg(args, 9),
                );
                handle.set_max_threads(max_threads_arg(args, 10));
                if let Err(e) =
                    m.add_plugin(claim, &handle, ctx.db().scan_control().interrupt_gate())
                {
                    return Ok(ctx.cxx_error(e));
                }
                if let Err(e) = handle.connect_to_array_port(m.wiring()) {
                    eprintln!("NDOverlayConfigure: wiring failed: {e}");
                }
                Ok(CommandOutcome::Continue)
            },
        ));
    }
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDFFTConfigure",
        plugin_arg_defs(),
        Some(6),
        Some(9),
        PluginThreadArgs::Arg(8),
        |claim, queue_size, source, max_memory, wiring, threads| {
            use crate::fft::FFTProcessor;
            create_plugin_runtime(
                claim,
                FFTProcessor::new(),
                max_memory,
                queue_size,
                source,
                wiring,
                threads,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDCircularBuffConfigure",
        plugin_arg_defs(),
        Some(6),
        None,
        PluginThreadArgs::Fixed(2_000_000),
        |claim, queue_size, source, max_memory, wiring, threads| {
            use crate::circular_buff::{CircularBuffProcessor, TriggerCondition};
            create_plugin_runtime(
                claim,
                // queue_size is C `maxBuffers_` (NDCircularBuffConfigure
                // queueSize); the processor bounds the accepted pre-count by it.
                CircularBuffProcessor::new(100, 100, TriggerCondition::default(), queue_size),
                max_memory,
                queue_size,
                source,
                wiring,
                threads,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDCodecConfigure",
        plugin_arg_defs(),
        Some(6),
        Some(9),
        PluginThreadArgs::Arg(8),
        |claim, queue_size, source, max_memory, wiring, threads| {
            use crate::codec::{CodecMode, CodecProcessor};
            use ad_core_rs::codec::CodecName;
            create_plugin_runtime(
                claim,
                CodecProcessor::new(CodecMode::Compress {
                    codec: CodecName::LZ4,
                    quality: 90,
                }),
                max_memory,
                queue_size,
                source,
                wiring,
                threads,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDScatterConfigure",
        plugin_arg_defs(),
        Some(6),
        None,
        PluginThreadArgs::Arg(8),
        |claim, queue_size, source, max_memory, wiring, threads| {
            use crate::scatter::ScatterProcessor;
            create_plugin_runtime(
                claim,
                ScatterProcessor::new(),
                max_memory,
                queue_size,
                source,
                wiring,
                threads,
            )
        },
    );
    // NDGatherConfigure: portName queueSize blockingCallbacks maxPorts
    // maxBuffers maxMemory priority stackSize (NDPluginGather.cpp:190-193).
    // The port has maxPorts addresses and is connected to nothing here: its
    // sources are the NDArrayPort/NDArrayAddr pairs NDGatherN.template
    // writes at addresses 0..maxPorts, and the runtime wires one input per
    // pair (`GatherProcessor::num_array_sources`).
    {
        let m = mgr.clone();
        let taken = std::mem::replace(&mut app, IocApplication::new());
        app = taken.register_startup_command(CommandDef::new(
            "NDGatherConfigure",
            gather_arg_defs(),
            "NDGatherConfigure portName [queueSize] [blockingCallbacks] [maxPorts]".to_string(),
            move |args: &[ArgValue], ctx: &CommandContext| {
                let (port_name, queue_size, _) = extract_plugin_args(args)?;
                let claim = match claim_port_driver(&port_name) {
                    Ok(claim) => claim,
                    Err(e) => return Ok(ctx.cxx_error(e)),
                };
                let max_ports = count_arg(args, 3);
                let (handle, _jh) = create_plugin_runtime_multi_addr(
                    &claim,
                    crate::gather::GatherProcessor::new(max_ports),
                    max_memory_arg(args, 5),
                    queue_size,
                    "",
                    m.wiring().clone(),
                    max_ports,
                    threads_arg(args, 7),
                );
                if let Err(e) =
                    m.add_plugin(claim, &handle, ctx.db().scan_control().interrupt_gate())
                {
                    return Ok(ctx.cxx_error(e));
                }
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
            move |args: &[ArgValue], ctx: &CommandContext| {
                let AttrPlotArgs {
                    port_name,
                    n_attributes,
                    cache_size,
                    n_data_blocks,
                    source,
                    queue_size,
                } = parse_attr_plot_args(args)?;

                let claim = match claim_port_driver(&port_name) {
                    Ok(claim) => claim,
                    Err(e) => return Ok(ctx.cxx_error(e)),
                };
                // Addr 0 always exists, so at least one address even for a
                // degenerate 0/0 request.
                let max_addr = n_attributes.max(n_data_blocks).max(1);
                let (handle, _jh) = create_plugin_runtime_multi_addr(
                    &claim,
                    crate::attr_plot::AttrPlotProcessor::new(
                        n_attributes,
                        cache_size,
                        n_data_blocks,
                    ),
                    usize::MAX,
                    queue_size,
                    &source,
                    m.wiring().clone(),
                    max_addr,
                    threads_arg(args, 9),
                );
                if let Err(e) =
                    m.add_plugin(claim, &handle, ctx.db().scan_control().interrupt_gate())
                {
                    return Ok(ctx.cxx_error(e));
                }
                if let Err(e) = handle.connect_to_array_port(m.wiring()) {
                    eprintln!("NDAttrPlotConfig: wiring failed: {e}");
                }
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
        None,
        PluginThreadArgs::Arg(6),
        |claim, queue_size, source, max_memory, wiring, threads| {
            use crate::file_tiff::TiffFileProcessor;
            create_plugin_runtime(
                claim,
                TiffFileProcessor::new(),
                max_memory,
                queue_size,
                source,
                wiring,
                threads,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDFileJPEGConfigure",
        plugin_arg_defs(),
        None,
        None,
        PluginThreadArgs::Arg(6),
        |claim, queue_size, source, max_memory, wiring, threads| {
            use crate::file_jpeg::JpegFileProcessor;
            create_plugin_runtime(
                claim,
                JpegFileProcessor::new(50),
                max_memory,
                queue_size,
                source,
                wiring,
                threads,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDFileHDF5Configure",
        plugin_arg_defs(),
        None,
        None,
        PluginThreadArgs::Arg(6),
        |claim, queue_size, source, max_memory, wiring, threads| {
            use crate::file_hdf5::Hdf5FileProcessor;
            create_plugin_runtime(
                claim,
                Hdf5FileProcessor::new(),
                max_memory,
                queue_size,
                source,
                wiring,
                threads,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDFileNetCDFConfigure",
        plugin_arg_defs(),
        None,
        None,
        PluginThreadArgs::Arg(6),
        |claim, queue_size, source, max_memory, wiring, threads| {
            use crate::file_netcdf::NetcdfFileProcessor;
            create_plugin_runtime(
                claim,
                NetcdfFileProcessor::new(),
                max_memory,
                queue_size,
                source,
                wiring,
                threads,
            )
        },
    );
    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDFileMagickConfigure",
        plugin_arg_defs(),
        None,
        None,
        PluginThreadArgs::Arg(6),
        |claim, queue_size, source, max_memory, wiring, threads| {
            use crate::file_magick::MagickFileProcessor;
            create_plugin_runtime(
                claim,
                MagickFileProcessor::new(),
                max_memory,
                queue_size,
                source,
                wiring,
                threads,
            )
        },
    );

    // --- NDAttrConfigure ---
    {
        let m = mgr.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDAttrConfigure",
            attr_arg_defs(),
            "NDAttrConfigure portName [queueSize] ...",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let (port_name, queue_size, source) = extract_plugin_args(args)?;
                // C arg index 5 is maxAttributes (NDPluginAttribute.cpp:245); a
                // missing iocsh int arg defaults to 0, which C floors to 1.
                let max_attributes = match args.get(5) {
                    Some(ArgValue::Int(n)) => *n as i32,
                    _ => 0,
                };
                let claim = match claim_port_driver(&port_name) {
                    Ok(claim) => claim,
                    Err(e) => return Ok(ctx.cxx_error(e)),
                };

                let (handle, _jh) = crate::attribute::create_attribute_runtime(
                    &claim,
                    max_memory_arg(args, 7),
                    queue_size,
                    &source,
                    m.wiring().clone(),
                    max_attributes,
                    threads_arg(args, 9),
                );
                if let Err(e) =
                    m.add_plugin(claim, &handle, ctx.db().scan_control().interrupt_gate())
                {
                    return Ok(ctx.cxx_error(e));
                }
                if let Err(e) = handle.connect_to_array_port(m.wiring()) {
                    eprintln!("NDAttrConfigure: wiring failed: {e}");
                }

                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // --- NDROIStatConfigure ---
    {
        let m = mgr.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDROIStatConfigure",
            plugin_arg_defs_with_count("maxROIs"),
            "NDROIStatConfigure portName [queueSize] ...",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let (port_name, queue_size, source) = extract_plugin_args(args)?;
                let claim = match claim_port_driver(&port_name) {
                    Ok(claim) => claim,
                    Err(e) => return Ok(ctx.cxx_error(e)),
                };
                let (handle, _roi_stat_params, _jh) = crate::roi_stat::create_roi_stat_runtime(
                    &claim,
                    max_memory_arg(args, 7),
                    queue_size,
                    &source,
                    m.wiring().clone(),
                    count_arg(args, 5),
                    threads_arg(args, 9),
                );
                handle.set_max_threads(max_threads_arg(args, 10));
                if let Err(e) =
                    m.add_plugin(claim, &handle, ctx.db().scan_control().interrupt_gate())
                {
                    return Ok(ctx.cxx_error(e));
                }
                if let Err(e) = handle.connect_to_array_port(m.wiring()) {
                    eprintln!("NDROIStatConfigure: wiring failed: {e}");
                }
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
        Some(6),
        Some(9),
        PluginThreadArgs::Arg(8),
        |claim, queue_size, source, max_memory, wiring, threads| {
            use crate::bad_pixel::BadPixelProcessor;
            create_plugin_runtime(
                claim,
                BadPixelProcessor::new(vec![]),
                max_memory,
                queue_size,
                source,
                wiring,
                threads,
            )
        },
    );

    app = register_generic_plugin(
        &mut app,
        mgr,
        "NDFileNexusConfigure",
        plugin_arg_defs(),
        None,
        None,
        PluginThreadArgs::Arg(6),
        |claim, queue_size, source, max_memory, wiring, threads| {
            use crate::file_nexus::NexusFileProcessor;
            create_plugin_runtime(
                claim,
                NexusFileProcessor::new(),
                max_memory,
                queue_size,
                source,
                wiring,
                threads,
            )
        },
    );

    // --- NDTimeSeriesConfigure ---
    // C NDTimeSeriesConfigure(portName, queueSize, blockingCallbacks,
    //   NDArrayPort, NDArrayAddr, maxSignals, ...): an NDPluginTimeSeries port.
    // Stats/Attribute feed it at their address 1; NDArrayAddress selects it.
    {
        let m = mgr.clone();
        app = app.register_startup_command(CommandDef::new(
            "NDTimeSeriesConfigure",
            plugin_arg_defs(),
            "NDTimeSeriesConfigure portName [queueSize] [blockingCallbacks] NDArrayPort",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let (port_name, queue_size, source) = extract_plugin_args(args)?;
                let claim = match claim_port_driver(&port_name) {
                    Ok(claim) => claim,
                    Err(e) => return Ok(ctx.cxx_error(e)),
                };
                let threads = threads_arg(args, 9);
                let max_signals = match args.get(5) {
                    Some(ArgValue::Int(n)) if *n >= 1 => *n as usize,
                    _ => 1,
                };
                let (handle, _jh) = create_plugin_runtime_multi_addr(
                    &claim,
                    crate::time_series_plugin::TimeSeriesProcessor::new(max_signals),
                    max_memory_arg(args, 7),
                    queue_size,
                    source,
                    m.wiring().clone(),
                    // C maxAddr = maxSignals + 1 (the 2-D array callback
                    // address is reserved at addr == maxSignals).
                    max_signals + 1,
                    threads,
                );
                if let Err(e) =
                    m.add_plugin(claim, &handle, ctx.db().scan_control().interrupt_gate())
                {
                    return Ok(ctx.cxx_error(e));
                }
                if let Err(e) = handle.connect_to_array_port(m.wiring()) {
                    eprintln!("NDTimeSeriesConfigure: wiring failed: {e}");
                }
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
            move |args: &[ArgValue], ctx: &CommandContext| {
                let (port_name, queue_size, source) = extract_plugin_args(args)?;
                let claim = match claim_port_driver(&port_name) {
                    Ok(claim) => claim,
                    Err(e) => return Ok(ctx.cxx_error(e)),
                };

                // C++ NDPvaConfigure args[5] is pvName (6th argument).
                // If not provided, fall back to "{portName}:Image". Only the
                // `pva`-feature processor consumes it, so gate the binding to
                // avoid an unused-variable warning when `ioc` is built without `pva`.
                #[cfg(feature = "pva")]
                let pva_pv_name = match args.get(5) {
                    Some(ArgValue::String(s)) if !s.is_empty() => s.to_str()?.to_owned(),
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
                    &claim,
                    processor,
                    max_memory_arg(args, 7),
                    queue_size,
                    &source,
                    m.wiring().clone(),
                    threads_arg(args, 9),
                );
                if let Err(e) =
                    m.add_plugin(claim, &handle, ctx.db().scan_control().interrupt_gate())
                {
                    return Ok(ctx.cxx_error(e));
                }
                if let Err(e) = handle.connect_to_array_port(m.wiring()) {
                    eprintln!("NDPvaConfigure: wiring failed: {e}");
                }
                #[cfg(not(feature = "pva"))]
                println!("NDPvaConfigure: port={port_name} (stub — enable 'pva' feature)");
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // ad-core-rs's own templates use these record types — NDArrayBase (so
    // every plugin) declares `busy` fields, NDStats/NDProcess `sseq` ones —
    // the way every C ADCore IOC links busySupport.dbd and calcSupport.dbd
    // through commonDriverMakefile:303-310. Both were dropped from the default
    // registry with the stdRecords.dbd manifest, so any IOC that registers
    // these plugins has to bring them back; registering here means every
    // caller of `register_all_plugins` gets them, not just `AdIoc`. A later
    // `register_record_type` call with the same name still overrides.
    app = app.register_record_type("busy", || {
        Box::new(epics_base_rs::server::records::busy::BusyRecord::default())
    });
    app = app.register_record_type("sseq", || {
        Box::new(epics_base_rs::server::records::sseq::SseqRecord::default())
    });

    app
}

/// Where a generic plugin's thread priority and stack come from.
#[derive(Clone, Copy)]
enum PluginThreadArgs {
    /// The command's `stackSize` argument at this index, `priority` before it.
    Arg(usize),
    /// A command with neither argument, whose C passes priority 0 and this
    /// stack (`NDCircularBuffConfigure`: 2000000, NDPluginCircularBuff.cpp:465).
    Fixed(i64),
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
    max_memory_idx: Option<usize>,
    max_threads_idx: Option<usize>,
    thread_args: PluginThreadArgs,
    factory: F,
) -> IocApplication
where
    F: Fn(
            &PortClaim,
            usize,
            &ArraySource,
            usize,
            Arc<WiringRegistry>,
            ad_core_rs::plugin::runtime::PluginThreads,
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
        move |args: &[ArgValue], ctx: &CommandContext| {
            let (port_name, queue_size, source) = extract_plugin_args(args)?;
            let claim = match claim_port_driver(&port_name) {
                Ok(claim) => claim,
                Err(e) => return Ok(ctx.cxx_error(e)),
            };
            // C's file plugins hand `NDPluginFile` a 0 (e.g. NDFileTIFF.cpp:614).
            let max_memory = max_memory_idx.map_or(0, |idx| max_memory_arg(args, idx));
            let threads = match thread_args {
                PluginThreadArgs::Arg(idx) => threads_arg(args, idx),
                PluginThreadArgs::Fixed(bytes) => {
                    ad_core_rs::plugin::runtime::plugin_threads(0, bytes)
                }
            };
            let (handle, _jh) = factory(
                &claim,
                queue_size,
                &source,
                max_memory,
                m.wiring().clone(),
                threads,
            );
            if let Some(idx) = max_threads_idx {
                handle.set_max_threads(max_threads_arg(args, idx));
            }
            if let Err(e) = m.add_plugin(claim, &handle, ctx.db().scan_control().interrupt_gate()) {
                return Ok(ctx.cxx_error(e));
            }
            if let Err(e) = handle.connect_to_array_port(m.wiring()) {
                eprintln!("{cmd_name}: wiring failed: {e}");
            }
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
        // The plugin and detector ports are built with `RuntimeConfig::default()`,
        // which binds them to `PortServices::global()`, and the asyn iocsh
        // commands `register_asyn_device_support` brings act on the process's
        // one port table, `PortManager::global()`. This IOC's ports are that
        // table: a manager of its own made `asynReport` list ports the shell
        // did not create and `asynSetTrace*` set masks nothing read.
        let trace = PortServices::global().trace().clone();
        let mgr = PluginManager::new();
        let ports = PortManager::global();

        asyn_rs::asyn_record::register_asyn_record_type();

        // `IocApplication::new()` already resolves the server port through
        // C `envGetInetPortConfigParam` (`runtime::net::cas_server_port`);
        // re-reading EPICS_CA_SERVER_PORT here with a strict parse dropped
        // that parity (no 5000 floor, no sscanf leniency, no diagnostics).
        let mut app = IocApplication::new();

        // `$(ADCORE)` resolves `$(ADCORE)/iocBoot/commonPlugins.cmd` and the
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
        // (asynInt32, asynFloat64, asynOctet, array types) via @asyn() links —
        // and with it the asyn iocsh command set: port creation
        // (`drvAsynIPPortConfigure` and friends), `asynOctetSetInputEos` /
        // `asynOctetSetOutputEos`, `asynSetOption`, `asynReport` and the trace
        // mutators, on the startup shell as well as the interactive one. A
        // socket detector's st.cmd creates its port and sets the EOS before
        // `iocInit`.
        app = asyn_rs::adapter::register_asyn_device_support(app);

        // `busy` and `sseq` — the record types every AD plugin's templates
        // need — are registered inside `register_all_plugins` above, so an
        // `AdIoc` picks them up along that path.

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

    /// The `TraceManager` every port of this IOC is bound to — the global
    /// services' (see `AdIoc::new`).
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

    /// Register a static device support: its factory under `devsup.choice`,
    /// and `devsup` as a row of `record_type`'s device menu.
    pub fn register_device_support<F>(
        &mut self,
        record_type: &str,
        devsup: &'static epics_base_rs::server::record::DevSup,
        factory: F,
    ) where
        F: Fn() -> Box<dyn epics_base_rs::server::device_support::DeviceSupport>
            + Send
            + Sync
            + 'static,
    {
        let app = self.app.take().unwrap();
        self.app = Some(app.register_device_support(record_type, devsup, factory));
    }

    /// Register a dynamic device support factory (dispatches by context) and
    /// the `device()` lines it serves.
    pub fn register_dynamic_device_support<F>(
        &mut self,
        devices: &[(&str, &'static [epics_base_rs::server::record::DevSup])],
        factory: F,
    ) where
        F: Fn(
                &epics_ca_rs::server::ioc_app::DeviceSupportContext,
            ) -> Option<Box<dyn epics_base_rs::server::device_support::DeviceSupport>>
            + Send
            + Sync
            + 'static,
    {
        let app = self.app.take().unwrap();
        self.app = Some(app.register_dynamic_device_support(devices, factory));
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
        epics_ca_rs::server::run_ca_ioc_app(app.startup_script(script)).await
    }

    /// Run the IOC with both CA and PVA protocols (QSRV bridge).
    ///
    /// Same as [`Self::run`] but uses
    /// [`epics_bridge_rs::qsrv::run_ca_pva_qsrv_ioc_app`] as the protocol runner, serving records over CA (default port 5064) and
    /// pvAccess (default port 5075) simultaneously. PVA plugin PVs (NTNDArray)
    /// registered during st.cmd are wired into the PVA server automatically.
    #[cfg(feature = "pva")]
    pub async fn run_with_pva(self, script: &str) -> CaResult<()> {
        let app = self.app.unwrap();

        // Runner plus both protocol registrars — see [`Self::run`].
        epics_bridge_rs::qsrv::run_ca_pva_qsrv_ioc_app(app.startup_script(script)).await
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
