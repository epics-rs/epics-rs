//! pvxs binds its server in `pvxsBaseRegistrar` (`ioc/iochooks.cpp:498`),
//! before the startup script, and starts it at `initHookAfterIocRunning`
//! (`:374-379`). Measured on softIocPVX 1.5.3: a held TCP 5075 falls back and
//! `PVXS_SERVER_PORT` names the fallback port from the script's first line; a
//! held UDP 5076 prints `Error in pvxsBaseRegistrar` before `Starting iocInit`
//! and the IOC runs without pvAccess.
#![cfg(tokio_backend)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::{
    ArgValue, CommandContext, CommandDef, CommandOutcome,
};
use epics_pva_rs::server::PvaServer;
use epics_pva_rs::server::iocsh::{PVXS_SERVER, PvxsServer, pvxs_base_registrar};

/// A startup command that records `PVXS_SERVER_PORT` as the script sees it.
fn read_port(into: Arc<Mutex<Option<String>>>) -> CommandDef {
    CommandDef::new(
        "readPort",
        vec![],
        "readPort",
        move |_: &[ArgValue], _: &CommandContext| {
            *into.lock().unwrap() = epics_base_rs::runtime::env::get_lossy("PVXS_SERVER_PORT");
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Loopback only, no beacons, both ports from `udp`.
fn pvas_env(udp: u16) {
    // SAFETY: nextest runs each test alone in its process.
    unsafe {
        std::env::set_var("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1");
        std::env::set_var("EPICS_PVAS_AUTO_BEACON_ADDR_LIST", "NO");
        std::env::set_var("EPICS_PVAS_BEACON_ADDR_LIST", "");
        std::env::set_var("EPICS_PVAS_SERVER_PORT", "0");
        std::env::set_var("EPICS_PVAS_BROADCAST_PORT", udp.to_string());
        std::env::remove_var("PVXS_SERVER_PORT");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_script_sees_the_port_the_registrar_holds() {
    pvas_env(0);
    let seen = Arc::new(Mutex::new(None));
    let app = pvxs_base_registrar(
        IocApplication::new()
            .port(0)
            .register_startup_command(read_port(seen.clone()))
            .startup_line("readPort")
            .startup_line("iocInit"),
    );
    let port = seen.clone();
    app.run(move |mut config| async move {
        let port: u16 = port
            .lock()
            .unwrap()
            .as_deref()
            .expect("PVXS_SERVER_PORT is set before the script")
            .parse()
            .unwrap();
        let _held = config
            .servers
            .take::<PvxsServer>(PVXS_SERVER)
            .expect("the registrar hands its server to the runner");
        // The listener the registrar bound queues the connection although
        // nothing serves it yet.
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("the registrar's listener holds the port");
        epics_base_rs::server::db_server::announce_serving();
        Ok(())
    })
    .await
    .expect("the IOC runs");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_held_search_port_leaves_the_ioc_without_pvaccess() {
    let held = std::net::UdpSocket::bind("127.0.0.1:0").expect("take a UDP port");
    pvas_env(held.local_addr().unwrap().port());
    let seen = Arc::new(Mutex::new(None));
    let app = pvxs_base_registrar(
        IocApplication::new()
            .port(0)
            .register_startup_command(read_port(seen.clone()))
            .startup_line("readPort")
            .startup_line("iocInit"),
    );
    app.run(move |mut config| async move {
        assert_eq!(*seen.lock().unwrap(), None, "no server, no port");
        let server = PvaServer::from_parts(config.db, None, config.acf, None, None)
            .bound_at_registrar(&mut config.servers);
        // Serving nothing is pending until the runner ends; a server that
        // bound for itself here would fail on the held port at once.
        assert!(
            tokio::time::timeout(Duration::from_millis(300), server.run())
                .await
                .is_err(),
            "the runner tried to serve pvAccess the registrar could not bind"
        );
        Ok(())
    })
    .await
    .expect("iocInit and iocRun do not fail on the PVA port");
    drop(held);
}
