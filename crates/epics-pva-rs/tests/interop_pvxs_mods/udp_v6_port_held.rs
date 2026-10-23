//! A configured search port another process holds on UDP6 only. softIocPVX
//! 1.5.3 with `EPICS_PVAS_INTF_ADDR_LIST="0.0.0.0 ::"` binds its `[::]`
//! listener on that number (`server.cpp:421-426`), the bind throws out of the
//! `Server` constructor and qsrv runs with no server: "Error in
//! pvxsBaseRegistrar : Address already in use", `pvxsr` "No Instance". With
//! `0.0.0.0` alone it binds no v6 listener and starts on the same number. The
//! Rust server on the same two lists refuses and starts the same.

// `server_native::PvaServer` exists only on the tokio backend.
#![cfg(tokio_backend)]

use super::interop_helpers::{SOFT_IOC_PVX, pvxs_command, pvxs_dbd_dir, require_pvxs};
use epics_pva_rs::error::PvaError;
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig, SharedSource};
use socket2::{Domain, Protocol, Socket, Type};
use std::io::Write;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

/// softIocPVX's console after `pvxsr 1`, its search port `port` and its
/// interface list `intf`. Not through `rerun_pvxs`, whose "Address already in
/// use" is what this file measures: rerun only when qsrv's own client lost
/// its random search port (`client.cpp:578-594`), which aborts the IOC.
fn soft_ioc_pvx(ioc: &Path, db: &Path, port: u16, intf: &str) -> String {
    for _ in 0..4 {
        let out = soft_ioc_pvx_once(ioc, db, port, intf);
        if !out.contains("Unable to bind random UDP")
            && named_port::verdict::softioc_verdict(&out)
                != named_port::verdict::SoftIocVerdict::PortTaken
        {
            return out;
        }
    }
    panic!("softIocPVX lost its random UDP or its CA port in 4 tries");
}

fn soft_ioc_pvx_once(ioc: &Path, db: &Path, port: u16, intf: &str) -> String {
    let ca = named_port::free_for_tcp_and_udp();
    let mut child = pvxs_command(ioc)
        .arg("-D")
        .arg(pvxs_dbd_dir().join("softIocPVX.dbd"))
        .arg("-d")
        .arg(db)
        .env("EPICS_PVAS_BROADCAST_PORT", port.to_string())
        .env("EPICS_PVAS_SERVER_PORT", "0")
        .env("EPICS_PVAS_INTF_ADDR_LIST", intf)
        .env("EPICS_PVAS_AUTO_BEACON_ADDR_LIST", "NO")
        // A named CA port: `0` falls back to 5064 (`envSubr.c:410`).
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_SERVER_PORT", ca.to_string())
        .env("EPICS_CA_SERVER_PORT", ca.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn softIocPVX");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"pvxsr 1\nexit\n")
        .expect("softIocPVX stdin");
    // RSRV that lost its CA port suspends iocInit (`caservertask.c:752`), and
    // the shell never reads the `exit`.
    for _ in 0..100 {
        if child.try_wait().expect("poll softIocPVX").is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = child.kill();
    let out = child.wait_with_output().expect("softIocPVX output");
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

fn rs_start(port: u16, v6: bool) -> Result<PvaServer, PvaError> {
    let intf: &[&str] = if v6 { &["0.0.0.0", "::"] } else { &["0.0.0.0"] };
    let config = PvaServerConfig {
        tcp_port: 0,
        udp_port: port,
        auto_beacon: false,
        beacon_destinations: Vec::new(),
        interfaces: intf
            .iter()
            .map(|t| epics_pva_rs::config::Endpoint::parse(t, 0).unwrap())
            .collect(),
        ..Default::default()
    };
    PvaServer::start(Arc::new(SharedSource::new()), config)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_port_held_on_v6_only_refuses_the_v6_start_as_softiocpvx() {
    let ioc = require_pvxs(SOFT_IOC_PVX);
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("t.db");
    std::fs::write(&db, "record(ai, \"X\") {}\n").expect("write db");
    // The number is the kernel's v6 pick, so another process may hold it on
    // v4; the v4-only start says so, and a fresh pick is taken.
    let mut picks = (0..4).map(|_| {
        let holder = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        holder.set_only_v6(true).unwrap();
        let any = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0);
        holder.bind(&any.into()).expect("hold a v6 number");
        let port = holder.local_addr().unwrap().as_socket().unwrap().port();
        let v4 = soft_ioc_pvx(&ioc, &db, port, "0.0.0.0");
        (holder, port, v4)
    });
    let (_holder, port, v4) = picks
        .find(|(_, _, v4)| !v4.contains("Address already in use"))
        .expect("no v6 pick was free on v4 in 4 tries");
    assert!(
        v4.contains(&format!("EPICS_PVAS_BROADCAST_PORT={port}")),
        "softIocPVX on 0.0.0.0 did not start on {port}:\n{v4}"
    );
    let both = soft_ioc_pvx(&ioc, &db, port, "0.0.0.0 ::");
    assert!(
        both.contains("Error in pvxsBaseRegistrar : Address already in use")
            && both.contains("No Instance"),
        "softIocPVX on 0.0.0.0 and :: started on {port}:\n{both}"
    );

    drop(rs_start(port, false).expect("v4-only start on a v6-held number"));
    match rs_start(port, true) {
        Err(PvaError::Io(e)) => assert!(epics_base_rs::net::port_held(&e), "{e}"),
        Err(e) => panic!("refused for another reason: {e}"),
        Ok(_) => panic!("the v6 start took a number held on v6"),
    }
}
