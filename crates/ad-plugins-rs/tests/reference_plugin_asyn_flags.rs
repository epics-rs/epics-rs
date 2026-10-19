//! `C_PLUGIN_ASYN_FLAGS` against ADCore: each row's flags are the `asynFlags`
//! argument of its plugin's constructor call, every registered `*Configure`
//! command has a row, and the port each command registers has those flags and
//! its file's PluginType.
#![cfg(tokio_backend)]
#![cfg(feature = "ioc")]

use std::sync::Arc;

use ad_core_rs::ioc::GenericDriverContext;
use ad_core_rs::ndarray_pool::DriverPool;
use ad_core_rs::plugin::channel::NDArrayOutput;
use ad_plugins_rs::ioc::{AdIoc, C_PLUGIN_ASYN_FLAGS};
use epics_base_rs::reference::{ReferenceTree, reference_path};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::{ArgType, CommandOutcome};

/// The argument `back` places before `priority` in the file's
/// `NDPluginDriver(` / `NDPluginFile(` constructor call, comments removed:
/// 2 is `asynFlags`, 7 `maxAddr`.
fn c_ctor_arg(file: &str, source: &str, back: usize) -> String {
    let mut code = String::new();
    let mut rest = source;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("//") {
            rest = r.find('\n').map_or("", |i| &r[i..]);
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.find("*/").map_or("", |i| &r[i + 2..]);
        } else {
            let c = rest.chars().next().unwrap();
            code.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    // The call, not a declaration: its argument list has a bare `priority`.
    let call = ["NDPluginDriver", "NDPluginFile"]
        .iter()
        .flat_map(|ctor| code.match_indices(ctor).map(|(i, m)| i + m.len()))
        .filter_map(|end| {
            let after = code[end..].trim_start().strip_prefix('(')?;
            let args = &after[..after.find(['{', ';'])?];
            args.split(',')
                .any(|a| a.trim() == "priority")
                .then_some(args)
        })
        .next()
        .unwrap_or_else(|| panic!("{file}: no constructor call passes priority"));
    let args: Vec<&str> = call.split(',').map(str::trim).collect();
    let at = args
        .iter()
        .position(|a| *a == "priority")
        .unwrap_or_else(|| panic!("{file}: priority is not an argument of {call:?}"));
    args[at - back].to_string()
}

#[test]
fn each_plugin_s_asyn_flags_are_c_s() {
    for (name, file, expr) in C_PLUGIN_ASYN_FLAGS {
        let path = reference_path(
            ReferenceTree::Modules,
            &format!("ADCore/ADApp/pluginSrc/{file}"),
        );
        let flags = c_ctor_arg(file, &std::fs::read_to_string(&path).unwrap(), 2);
        assert_eq!(flags, expr, "{name} ({file})");
    }
}

#[test]
fn every_registered_configure_command_has_a_row() {
    let ioc = AdIoc::new();
    let mut registered: Vec<String> = ioc
        .app()
        .startup_commands()
        .iter()
        .map(|d| d.name.clone())
        .filter(|n| n.starts_with("ND") && n.contains("Config"))
        .collect();
    let mut table: Vec<&str> = C_PLUGIN_ASYN_FLAGS.iter().map(|r| r.0).collect();
    registered.sort();
    table.sort();
    assert_eq!(registered, table);
}

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

/// A `name(...)` line for the registered command: `port` first, every other
/// string `upstream`, `queueSize` 20, every other number 0.
fn configure_line(ioc: &AdIoc, name: &str, port: &str, upstream: &str) -> String {
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
            (_, ArgType::Int) if a.name == "queueSize" => "20".into(),
            (_, ArgType::Int | ArgType::Double) => "0".into(),
            _ => format!("\"{upstream}\""),
        })
        .collect();
    format!("{name}({})", args.join(", "))
}

fn run(shell: &IocShell, line: &str) {
    assert!(
        matches!(shell.execute_line(line), Ok(CommandOutcome::Continue)),
        "`{line}` failed"
    );
}

/// `(can_block, multi_device, destructible)` of the registered port `port`.
/// Destructible is what `shutdownPort` accepts (asynManager.c:2273), so this
/// shuts a destructible port down: call it last.
fn port_flags(port: &str) -> (bool, bool, bool) {
    let entry = asyn_rs::registry::get_port(port).unwrap_or_else(|| panic!("no port {port}"));
    let destructible = entry
        .handle
        .submit_blocking(
            asyn_rs::request::RequestOp::ShutdownPort,
            asyn_rs::user::AsynUser::default(),
        )
        .is_ok();
    (
        entry.handle.can_block(),
        entry.handle.is_multi_device(),
        destructible,
    )
}

/// Each command's port is can-block, multi-device and destructible exactly
/// where its C `asynFlags` say, so only the seven NDFile* get a port thread.
/// Destructible is NDROIConfigure's own `ASYN_DESTRUCTIBLE`
/// (NDPluginROI.cpp:363-364), the one `pluginSrc` file that names it.
/// Multi-device also follows a `maxAddr` above 1, which asynPortDriver turns
/// into `ASYN_MULTIDEVICE` (asynPortDriver.cpp:4013-4014): NDStats' 2.
#[test]
fn each_configure_command_s_port_has_c_s_asyn_flags() {
    let (ioc, shell) = ad_ioc_with_driver("AFUP");
    for (i, (name, file, expr)) in C_PLUGIN_ASYN_FLAGS.iter().enumerate() {
        let port = format!("AF{i}");
        run(&shell, &configure_line(&ioc, name, &port, "AFUP"));
        let source = std::fs::read_to_string(reference_path(
            ReferenceTree::Modules,
            &format!("ADCore/ADApp/pluginSrc/{file}"),
        ))
        .unwrap();
        let max_addr = c_ctor_arg(file, &source, 7);
        let multi_device = expr.contains("ASYN_MULTIDEVICE")
            || max_addr
                .parse::<u32>()
                .unwrap_or_else(|_| panic!("{file}: maxAddr {max_addr} is not a literal"))
                > 1;
        let want = (
            expr.contains("ASYN_CANBLOCK"),
            multi_device,
            source.contains("ASYN_DESTRUCTIBLE"),
        );
        assert_eq!(port_flags(&port), want, "{name} ({file}: {expr})");
    }
    // NDTimeSeriesConfigure on a Stats upstream builds the same
    // NDPluginTimeSeries port, `ASYN_MULTIDEVICE` only (NDPluginTimeSeries.cpp:54).
    run(&shell, r#"NDStatsConfigure("AFST", 20, 0, "AFUP", 0)"#);
    run(
        &shell,
        &configure_line(&ioc, "NDTimeSeriesConfigure", "AFTS", "AFST"),
    );
    assert_eq!(
        port_flags("AFTS"),
        (false, true, false),
        "NDTimeSeries on a Stats upstream"
    );
}

/// A synchronous plugin's port has no thread, so a negative `priority` on its
/// line — `unsigned int` past every band, as `registerPort` reads it
/// (asynManager.c:2049) — is never asked of the scheduler and the IOC carries
/// on, as C's does. Under RT, with SCHED_FIFO, a can-block port would be
/// refused here. A child, because the RT switch is read once per process.
#[test]
fn a_negative_priority_on_a_synchronous_plugin_does_not_stop_the_ioc() {
    const CHILD: &str = "AD_NEG_PRIORITY_CHILD";
    const TEST: &str = "a_negative_priority_on_a_synchronous_plugin_does_not_stop_the_ioc";
    if std::env::var_os(CHILD).is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            // RT on under either switch name: C's, and this port's alias.
            .env("EPICS_ALLOW_POSIX_THREAD_PRIORITY_SCHEDULING", "YES")
            .env(epics_base_rs::runtime::task::RT_PRIORITY_ENV, "YES")
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
    let (ioc, shell) = ad_ioc_with_driver("NPUP");
    // NDStatsConfigure's priority is argument 7, stackSize 8.
    run(
        &shell,
        r#"NDStatsConfigure("NPST", 20, 0, "NPUP", 0, 0, 0, -1, 0)"#,
    );
    assert!(ioc.mgr().plugin("NPST").is_some());
    // Multi-device by NDStats' maxAddr 2 (NDPluginStats.cpp:777).
    assert_eq!(port_flags("NPST"), (false, true, false));
}

/// The PluginType each row's file sets (`setStringParam(NDPluginDriverPluginType,
/// "...")`) is what the port it registers reads back. NDFileHDF5 sets it from a
/// variable built with the HDF5 version (NDFileHDF5.cpp:2392) and is skipped.
#[test]
fn each_configure_command_s_port_has_c_s_plugin_type() {
    const SET: &str = "setStringParam(NDPluginDriverPluginType, \"";
    let (ioc, shell) = ad_ioc_with_driver("PTUP");
    let mut checked = 0;
    for (i, (name, file, _)) in C_PLUGIN_ASYN_FLAGS.iter().enumerate() {
        let source = std::fs::read_to_string(reference_path(
            ReferenceTree::Modules,
            &format!("ADCore/ADApp/pluginSrc/{file}"),
        ))
        .unwrap();
        let Some(at) = source.find(SET) else {
            continue;
        };
        let rest = &source[at + SET.len()..];
        let want = &rest[..rest.find('"').unwrap()];
        let port = format!("PT{i}");
        run(&shell, &configure_line(&ioc, name, &port, "PTUP"));
        let handle = asyn_rs::registry::get_port(&port).unwrap().handle;
        let reason = handle
            .drv_user_create_blocking(&asyn_rs::port::DrvUserRequest::new("PLUGIN_TYPE", 0))
            .unwrap()
            .reason;
        let got = handle.get_string_param_blocking(reason, 0, 64).unwrap();
        assert_eq!(String::from_utf8_lossy(&got), want, "{name} ({file})");
        checked += 1;
    }
    assert_eq!(checked, C_PLUGIN_ASYN_FLAGS.len() - 1);
}
