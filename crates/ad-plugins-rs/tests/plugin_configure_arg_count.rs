//! Every plugin `*Configure` command declares the arguments its C
//! `iocshFuncDef` registers, which is what `help` prints and how many
//! arguments iocsh converts.
// `ad_plugins_rs::ioc` mounts on the tokio-backend-only IOC runners, so
// this file is gated with the module it exercises.
#![cfg(tokio_backend)]
#![cfg(feature = "ioc")]

use ad_plugins_rs::ioc::AdIoc;

/// C `nargs` per command, from each plugin's `iocshFuncDef`.
const C_NARGS: [(&str, usize); 25] = [
    ("NDFileTIFFConfigure", 7),
    ("NDFileJPEGConfigure", 7),
    ("NDFileHDF5Configure", 7),
    ("NDFileNetCDFConfigure", 7),
    ("NDFileMagickConfigure", 7),
    ("NDFileNexusConfigure", 7),
    ("NDCircularBuffConfigure", 7),
    ("NDGatherConfigure", 8),
    ("NDPosPluginConfigure", 9),
    ("NDProcessConfigure", 9),
    ("NDScatterConfigure", 9),
    // C registers 9 but reads a 10th, maxThreads (NDPluginBadPixel.cpp:402-408).
    ("NDBadPixelConfigure", 10),
    ("NDCodecConfigure", 10),
    ("NDColorConvertConfigure", 10),
    ("NDFFTConfigure", 10),
    ("NDROIConfigure", 10),
    ("NDStatsConfigure", 10),
    ("NDStdArraysConfigure", 10),
    ("NDTransformConfigure", 10),
    ("NDAttrConfigure", 10),
    ("NDAttrPlotConfig", 10),
    ("NDPvaConfigure", 10),
    ("NDTimeSeriesConfigure", 10),
    ("NDOverlayConfigure", 11),
    ("NDROIStatConfigure", 11),
];

#[test]
fn each_configure_command_declares_c_s_argument_count() {
    let ioc = AdIoc::new();
    let defs = ioc.app().startup_commands();
    let mut registered: Vec<&str> = defs
        .iter()
        .map(|d| d.name.as_str())
        .filter(|n| n.starts_with("ND") && n.contains("Config"))
        .collect();
    let mut table: Vec<&str> = C_NARGS.iter().map(|(n, _)| *n).collect();
    registered.sort();
    table.sort();
    assert_eq!(
        registered, table,
        "every registered Configure command has a C count"
    );

    let wrong: Vec<String> = C_NARGS
        .iter()
        .filter_map(|&(name, nargs)| {
            let def = defs.iter().find(|d| d.name == name).unwrap();
            (def.args.len() != nargs).then(|| format!("{name}: {} != C {nargs}", def.args.len()))
        })
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// The file plugins take `priority, stackSize` right after `NDArrayAddr`
/// (NDFileTIFF.cpp:651-660), not the generic `maxBuffers, maxMemory`.
#[test]
fn file_plugins_name_priority_and_stack_size_at_five_and_six() {
    let ioc = AdIoc::new();
    for def in ioc
        .app()
        .startup_commands()
        .iter()
        .filter(|d| d.name.starts_with("NDFile"))
    {
        let names: Vec<&str> = def.args[5..].iter().map(|a| a.name).collect();
        assert_eq!(names, ["priority", "stackSize"], "{}", def.name);
    }
}
