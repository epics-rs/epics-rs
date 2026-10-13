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
use ad_core_rs::ndarray_pool::NDArrayPool;
use ad_core_rs::plugin::channel::NDArrayOutput;
use ad_core_rs::plugin::runtime::plugin_threads;
use ad_plugins_rs::ioc::AdIoc;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::{ArgType, CommandOutcome};

fn ad_ioc_with_driver(upstream: &str) -> (AdIoc, IocShell) {
    let ioc = AdIoc::new();
    let pool = NDArrayPool::new(4_000_000);
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
const C_STACK_INDEX: [(&str, Option<usize>); 24] = [
    ("NDFileTIFFConfigure", Some(6)),
    ("NDFileJPEGConfigure", Some(6)),
    ("NDFileHDF5Configure", Some(6)),
    ("NDFileNetCDFConfigure", Some(6)),
    ("NDFileMagickConfigure", Some(6)),
    ("NDFileNexusConfigure", Some(6)),
    ("NDGatherConfigure", Some(7)),
    ("NDBadPixelConfigure", Some(8)),
    ("NDCodecConfigure", Some(8)),
    ("NDColorConvertConfigure", Some(8)),
    ("NDFFTConfigure", Some(8)),
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

/// The stack of the thread `tid` of this process, parked in a syscall: the
/// mapping its stack pointer is in. `None` while it is running.
#[cfg(target_os = "linux")]
fn parked_thread_stack_bytes(tid: u64) -> Option<usize> {
    let syscall = std::fs::read_to_string(format!("/proc/self/task/{tid}/syscall")).ok()?;
    // `nr arg1..arg6 sp pc`; a running thread reads `running`.
    let fields: Vec<&str> = syscall.split_whitespace().collect();
    if fields.len() < 3 {
        return None;
    }
    let sp = usize::from_str_radix(fields[fields.len() - 2].trim_start_matches("0x"), 16).ok()?;
    std::fs::read_to_string("/proc/self/maps")
        .ok()?
        .lines()
        .filter_map(|l| {
            let (lo, hi) = l.split_whitespace().next()?.split_once('-')?;
            Some((
                usize::from_str_radix(lo, 16).ok()?,
                usize::from_str_radix(hi, 16).ok()?,
            ))
        })
        .find(|(lo, hi)| (*lo..*hi).contains(&sp))
        .map(|(lo, hi)| hi - lo)
}

/// The port `NDTimeSeriesConfigure` builds on a Stats/ROIStat/Attr upstream's
/// receiver has the line's `priority` and `stackSize` too: C builds the same
/// `NDPluginTimeSeries` whatever feeds it (NDPluginTimeSeries.cpp:537-541).
///
/// Its handles stay inside the command, so its thread is found by name in
/// `thread_report` and its stack read from the mapping the parked thread's
/// stack pointer is in. Each run is a child of its own: glibc hands a new
/// thread an exited thread's cached stack when it is big enough.
#[cfg(target_os = "linux")]
#[test]
fn the_time_series_receiver_port_gets_the_line_s_priority_and_stack() {
    const CHILD: &str = "AD_TS_RECEIVER_THREADS_CHILD";
    const TEST: &str = "the_time_series_receiver_port_gets_the_line_s_priority_and_stack";
    const BYTES: usize = 3_000_000;
    const PRIORITY: u32 = 33;

    if std::env::var_os(CHILD).is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }

    let (_ioc, shell) = ad_ioc_with_driver("TSUP");
    for line in [
        r#"NDStatsConfigure("TSSTAT", 20, 0, "TSUP", 0)"#.to_string(),
        format!(
            r#"NDTimeSeriesConfigure("TSRX", 20, 0, "TSSTAT", 0, 0, 0, 0, {PRIORITY}, {BYTES})"#
        ),
    ] {
        assert!(
            matches!(shell.execute_line(&line), Ok(CommandOutcome::Continue)),
            "`{line}` failed"
        );
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let (priority, stack) = loop {
        let row = epics_base_rs::runtime::task::thread_report()
            .into_iter()
            .find(|t| t.name() == "ts-data-TSRX");
        if let Some(row) = &row
            && let Some(stack) = parked_thread_stack_bytes(row.os_id())
        {
            break (row.epics_priority(), stack);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "ts-data-TSRX never parked: {}",
            row.is_some()
        );
        std::thread::yield_now();
    };
    assert_eq!(priority, PRIORITY);
    // The mapping holds the stack less glibc's guard page, rounded to pages.
    assert!(
        stack.abs_diff(BYTES) <= 64 * 1024,
        "ts-data-TSRX has a {stack}-byte stack mapping, want about {BYTES}"
    );
}
