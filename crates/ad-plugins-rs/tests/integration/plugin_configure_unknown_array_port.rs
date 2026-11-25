//! A plugin configured against an array port that does not exist is still
//! created: C's constructor ends with `connectToArrayPort`, which prints the
//! `connectDevice` failure and returns (NDPluginDriver.cpp:605-610), and no
//! detector need have been configured first.
#![cfg(tokio_backend)]
#![cfg(feature = "ioc")]

use std::sync::Arc;

use ad_plugins_rs::ioc::AdIoc;
use asyn_rs::manager::PortManager;
use asyn_rs::trace::TraceFile;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::CommandOutcome;

#[test]
fn a_plugin_on_a_missing_array_port_prints_c_connect_device_error() {
    let log = std::env::temp_dir().join(format!("nocon-trace-{}.log", std::process::id()));
    let file = Arc::new(std::sync::Mutex::new(std::fs::File::create(&log).unwrap()));
    // asynSetTraceFile before the port exists, as an st.cmd may.
    for port in ["NOCON1", "NOCON2", "NOCON3"] {
        PortManager::global()
            .trace_manager()
            .set_trace_file(Some(port), TraceFile::File(file.clone()));
    }

    let ioc = AdIoc::new();
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

    for line in [
        r#"NDStdArraysConfigure("NOCON1", 3, 0, "NOSUCH", 0)"#,
        r#"NDStatsConfigure("NOCON2", 3, 0, "NOSUCH", 1)"#,
        r#"NDFileTIFFConfigure("NOCON3", 3, 0, "NOSUCH", 2)"#,
    ] {
        assert!(
            matches!(shell.execute_line(line), Ok(CommandOutcome::Continue)),
            "{line}"
        );
    }
    for port in ["NOCON1", "NOCON2", "NOCON3"] {
        assert!(asyn_rs::registry::get_port(port).is_some(), "{port}");
    }

    let trace = std::fs::read_to_string(&log).unwrap();
    let _ = std::fs::remove_file(&log);
    for addr in 0..3 {
        let line = format!(
            "NDPluginDriver::connectToArrayPort Error calling pasynManager->connectDevice to array port NOSUCH address {addr}, status=3, error=asynManager:connectDevice port NOSUCH not found\n"
        );
        assert!(trace.contains(&line), "missing {line:?} in\n{trace}");
    }
}
