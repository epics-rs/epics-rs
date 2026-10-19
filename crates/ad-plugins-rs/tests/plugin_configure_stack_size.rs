//! A `*Configure` line's `priority` and `stackSize` are the band and stack
//! of the plugin's threads.
//!
//! C hands it to `NDPluginDriver`, which passes it to every thread it creates
//! (`asynNDArrayDriver.cpp:876-879`, `NDPluginDriver.cpp:1000`). Where it sits
//! differs per command, so every command is driven from its own C index.
//! `NDCircularBuffConfigure` takes none and C passes 2000000
//! (NDPluginCircularBuff.cpp:465).
// `ad_plugins_rs::ioc` mounts on the tokio-backend-only IOC runners, so
// this file is gated with the module it exercises.
#![cfg(tokio_backend)]
#![cfg(feature = "ioc")]

use std::sync::Arc;

use ad_core_rs::ioc::GenericDriverContext;
use ad_core_rs::ndarray_pool::DriverPool;
use ad_core_rs::plugin::channel::NDArrayOutput;
use ad_core_rs::plugin::runtime::plugin_threads;
use ad_plugins_rs::ioc::AdIoc;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::{ArgType, CommandOutcome};

fn ad_ioc_with_driver(upstream: &str) -> (AdIoc, IocShell) {
    let ioc = AdIoc::new();
    let pool = DriverPool::standalone(4_000_000);
    let output = Arc::new(parking_lot::Mutex::new(NDArrayOutput::new()));
    ioc.mgr().set_driver(Arc::new(GenericDriverContext::new(
        pool,
        output,
        upstream,
        ioc.mgr().wiring(),
    )));
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    // The shell's command handlers block on this runtime; it must outlive them.
    std::mem::forget(rt);
    let shell = IocShell::new(Arc::new(PvDatabase::new()), bridge);
    for def in ioc.app().startup_commands() {
        shell.register(def.clone());
    }
    (ioc, shell)
}

/// Where each `*Configure` command carries `stackSize`, from the ADCore
/// `iocshFuncDef` tables; `priority` is just before it in every one. `None` is
/// `NDCircularBuffConfigure`, which takes neither and passes 0 and 2000000.
const C_STACK_INDEX: [(&str, Option<usize>); 27] = [
    ("NDFileTIFFConfigure", Some(6)),
    ("NDFileJPEGConfigure", Some(6)),
    ("NDFileHDF5Configure", Some(6)),
    ("NDFileNetCDFConfigure", Some(6)),
    ("NDFileMagickConfigure", Some(6)),
    ("NDFileNexusConfigure", Some(6)),
    ("NDFileNullConfigure", Some(6)),
    ("NDGatherConfigure", Some(7)),
    ("NDBadPixelConfigure", Some(8)),
    ("NDCodecConfigure", Some(8)),
    ("NDColorConvertConfigure", Some(8)),
    ("NDFFTConfigure", Some(8)),
    ("NDPosPluginConfigure", Some(8)),
    ("NDProcessConfigure", Some(8)),
    ("NDROIConfigure", Some(8)),
    ("NDScatterConfigure", Some(8)),
    ("NDStatsConfigure", Some(8)),
    ("NDStdArraysConfigure", Some(8)),
    ("NDTransformConfigure", Some(8)),
    ("NDAttrConfigure", Some(9)),
    ("NDAttrPlotConfig", Some(9)),
    ("NDOverlayConfigure", Some(9)),
    ("NDPvaConfigure", Some(9)),
    ("NDPvxsConfigure", Some(9)),
    ("NDROIStatConfigure", Some(9)),
    ("NDTimeSeriesConfigure", Some(9)),
    ("NDCircularBuffConfigure", None),
];

/// A `name(...)` line for the registered command, `port` first, every other
/// string the upstream port, `queueSize` 20, `priority` and `stackSize` at
/// `stack.0 - 1` and `stack.0`, and every other integer 0 — so a handler
/// reading any other index sees 0.
fn configure_line(ioc: &AdIoc, name: &str, port: &str, stack: Option<(usize, i64, i64)>) -> String {
    let def = ioc
        .app()
        .startup_commands()
        .iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("{name} is not registered"));
    let args: Vec<String> = def
        .args
        .iter()
        .enumerate()
        .map(|(i, a)| match (i, &a.arg_type) {
            (0, _) => format!("\"{port}\""),
            _ if stack.is_some_and(|(idx, _, _)| idx == i) => stack.unwrap().2.to_string(),
            _ if stack.is_some_and(|(idx, _, _)| idx == i + 1) => stack.unwrap().1.to_string(),
            (_, ArgType::Int) if a.name == "queueSize" => "20".into(),
            (_, ArgType::Int) => "0".into(),
            (_, ArgType::Double) => "0".into(),
            _ => "\"SSUP\"".into(),
        })
        .collect();
    format!("{name}({})", args.join(", "))
}

#[test]
fn each_configure_line_s_stack_size_is_its_plugin_s_stack() {
    let (ioc, shell) = ad_ioc_with_driver("SSUP");
    let registered: Vec<String> = ioc
        .app()
        .startup_commands()
        .iter()
        .map(|d| d.name.clone())
        .filter(|n| n.starts_with("ND") && n.contains("Config"))
        .collect();
    let mut table: Vec<&str> = C_STACK_INDEX.iter().map(|(n, _)| *n).collect();
    let mut sorted = registered.clone();
    sorted.sort();
    table.sort();
    assert_eq!(
        sorted, table,
        "every registered Configure command has a C index"
    );

    for (i, (name, index)) in C_STACK_INDEX.iter().enumerate() {
        let port = format!("SS{i}");
        let bytes = 300_000 + i as i64 * 4096;
        let priority = 20 + i as i64;
        let line = configure_line(&ioc, name, &port, index.map(|idx| (idx, priority, bytes)));
        assert!(
            matches!(shell.execute_line(&line), Ok(CommandOutcome::Continue)),
            "`{line}` failed"
        );
        let handle = ioc
            .mgr()
            .plugin(&port)
            .unwrap_or_else(|| panic!("`{line}` registered no plugin {port}"));
        let want = match index {
            Some(_) => plugin_threads(priority, bytes),
            None => plugin_threads(0, 2_000_000),
        };
        assert_eq!(handle.threads(), want, "`{line}`");
    }
}

/// `priority` and `stackSize` <= 0 are Medium (`asynNDArrayDriver.cpp:876-878`);
/// for the stack that is not the Small a bare `epicsThreadCreate` 0 gives.
#[test]
fn an_omitted_stack_size_is_medium() {
    let (ioc, shell) = ad_ioc_with_driver("SSUP2");
    let line = r#"NDStatsConfigure("SSST", 20, 0, "SSUP2", 0)"#;
    assert!(matches!(
        shell.execute_line(line),
        Ok(CommandOutcome::Continue)
    ));
    assert_eq!(
        ioc.mgr().plugin("SSST").expect("SSST").threads(),
        ad_core_rs::plugin::runtime::DEFAULT_PLUGIN_THREADS
    );
}

/// A Stats upstream feeds the same `NDPluginTimeSeries` port a raw array
/// source does (NDPluginTimeSeries.cpp:537-541), with the line's `priority`
/// and `stackSize`.
#[test]
fn a_stats_fed_time_series_port_gets_the_line_s_priority_and_stack() {
    let (ioc, shell) = ad_ioc_with_driver("TSUP");
    for line in [
        r#"NDStatsConfigure("TSSTAT", 20, 0, "TSUP", 0)"#,
        r#"NDTimeSeriesConfigure("TSRX", 20, 0, "TSSTAT", 1, 23, 0, 0, 33, 3000000)"#,
    ] {
        assert!(
            matches!(shell.execute_line(line), Ok(CommandOutcome::Continue)),
            "`{line}` failed"
        );
    }
    let handle = ioc.mgr().plugin("TSRX").expect("TSRX is a plugin port");
    assert_eq!(handle.threads(), plugin_threads(33, 3_000_000));
}
