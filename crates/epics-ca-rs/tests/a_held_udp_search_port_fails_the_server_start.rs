//! A search port that cannot be bound makes the CA server's start return an
//! error. C's `rsrv_init` prints and suspends the init thread instead
//! (`caservertask.c:135-141`, `:752`); the difference is a signed-off
//! deviation (docs/differences.md).

#[cfg(tokio_backend)]
#[tokio::test]
async fn a_held_udp_search_port_fails_from_parts_instead_of_hanging() {
    use epics_base_rs::server::database::PvDatabase;
    use epics_ca_rs::server::CaServer;
    use std::sync::Arc;

    // Held for the whole test, with no reuse flag, so the server's bind cannot
    // share it. On the server's own address: Windows lets a `SO_REUSEADDR`
    // bind on a specific address share a port held on the wildcard.
    let held = std::net::UdpSocket::bind("127.0.0.1:0").expect("take a UDP port");
    let port = held.local_addr().unwrap().port();
    // SAFETY: nextest runs this test alone in its process.
    unsafe {
        std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1");
    }
    let started = CaServer::from_parts(
        Arc::new(PvDatabase::new()),
        port,
        Some(0),
        epics_base_rs::server::access_security::new_acf_cell(None),
        None,
        None,
    )
    .await;
    assert!(started.is_err(), "the server started on a held UDP port");
}

/// Under `iocInit` the bind is RSRV's `dbInitServers()` init, so the held port
/// fails the script's `iocInit` line and the runner is never started, on
/// either backend.
#[epics_macros_rs::epics_test]
async fn a_held_udp_search_port_fails_ioc_init() {
    let held = std::net::UdpSocket::bind("127.0.0.1:0").expect("take a UDP port");
    let port = held.local_addr().unwrap().port();
    // SAFETY: nextest runs this test alone in its process.
    unsafe {
        std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1");
    }
    let script = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("held_port_ioc_init.cmd");
    std::fs::write(&script, "iocInit\n").expect("write the script");
    let app = epics_ca_rs::server::iocsh::register_rsrv_commands(
        epics_base_rs::server::ioc_app::IocApplication::new()
            .port(port)
            .tcp_port(0)
            .startup_script(&script.to_string_lossy()),
    );
    let err = app
        .run(|_config| async { panic!("the runner must not be started") })
        .await
        .expect_err("iocInit fails on the held port");
    // Windows refuses the `SO_REUSEADDR` bind with WSAEACCES.
    let err = err.to_string();
    assert!(
        named_port::verdict::held_texts()
            .iter()
            .any(|t| err.contains(t.as_str())),
        "got: {err}"
    );
}
