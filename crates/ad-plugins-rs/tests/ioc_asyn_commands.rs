//! An `AdIoc` startup script must be able to create an asyn port and configure
//! it — the sequence every socket detector (Pilatus, marCCD, mar345 camserver)
//! opens its st.cmd with.
#![cfg(feature = "ioc")]

use std::sync::Arc;

use ad_plugins_rs::ioc::AdIoc;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

/// Build a shell carrying exactly the commands `AdIoc` puts on its startup
/// shell, which is the surface `st.cmd` is executed against.
fn shell_with_ad_ioc_startup_commands(ioc: &AdIoc) -> IocShell {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let db = Arc::new(PvDatabase::new());
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    // The shell's command handlers block on this runtime; it must outlive them.
    std::mem::forget(rt);

    let shell = IocShell::new(db, bridge);
    for def in ioc.app().startup_commands() {
        shell.register(def.clone());
    }
    shell
}

/// The asyn iocsh command set has to be on the *startup* shell, not just the
/// interactive one: `IocApplication` runs `st.cmd` against `startup_commands`
/// alone, so a command registered only as a shell command is an unknown command
/// in st.cmd — fatal, before `iocInit`.
///
/// Unfixed, `AdIoc` registered none of these (it built its `IocApplication`
/// internally and never called `asyn_rs::iocsh::register_asyn_commands`), so
/// every line below died with "unknown command".
#[test]
fn ad_ioc_startup_shell_carries_the_asyn_command_set() {
    let ioc = AdIoc::new();
    let names: Vec<&str> = ioc
        .app()
        .startup_commands()
        .iter()
        .map(|def| def.name.as_str())
        .collect();

    for required in [
        "drvAsynIPPortConfigure",
        "drvAsynSerialPortConfigure",
        "asynOctetSetInputEos",
        "asynOctetSetOutputEos",
        "asynSetOption",
        "asynReport",
        "asynSetTraceMask",
    ] {
        assert!(
            names.contains(&required),
            "st.cmd cannot call {required} — AdIoc's startup shell exposes {names:?}"
        );
    }
}

/// End to end: run the detector-style st.cmd prologue against the shell AdIoc
/// actually builds. Creating the port and then configuring it must both work,
/// and the port must be resolvable through the IOC's `PortManager` afterwards.
///
/// That last assertion is the one that catches the registry split: the
/// `drvAsyn*PortConfigure` commands publish the port they create into the
/// process port registry, while `asynSetOption` / the EOS commands resolve
/// through `PortManager`. With those two disjoint, every command here still
/// *executes* — they report "port not found" and continue — so a smoke test
/// that only checked for non-fatal execution would pass against a port nothing
/// could configure.
#[test]
fn ad_ioc_st_cmd_can_create_a_port_and_configure_it() {
    let ioc = AdIoc::new();
    let shell = shell_with_ad_ioc_startup_commands(&ioc);

    // noAutoConnect=1: the port is created but never dials, so no detector has
    // to be listening on the far end.
    let prologue = [
        r#"drvAsynIPPortConfigure("SMOKE_DET", "127.0.0.1:65432", 0, 1, 0)"#,
        r#"asynOctetSetInputEos("SMOKE_DET", 0, "\n")"#,
        r#"asynOctetSetOutputEos("SMOKE_DET", 0, "\r\n")"#,
        r#"asynSetTraceMask("SMOKE_DET", 0, 1)"#,
        "asynReport(1)",
    ];
    for line in prologue {
        shell
            .execute_line(line)
            .unwrap_or_else(|e| panic!("st.cmd line `{line}` failed: {e}"));
    }

    assert!(
        ioc.ports().find_port_handle("SMOKE_DET").is_ok(),
        "the port drvAsynIPPortConfigure created is invisible to the IOC's \
         PortManager, so asynSetOption and the EOS commands cannot act on it"
    );
    assert!(
        ioc.ports()
            .list_port_names()
            .iter()
            .any(|name| name == "SMOKE_DET"),
        "asynReport with no port argument must list a port created from st.cmd"
    );
}
