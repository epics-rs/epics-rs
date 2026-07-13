//! R18-58: an IP server port accepts connections.
//!
//! C `drvAsynIPServerPortConfigure` binds the socket, pre-creates one child port
//! per client slot (`<parent>:<N>`, drvAsynIPServerPort.c:681-708) and starts
//! `connectionListener` (:711-714), which loops on `epicsSocketAccept` (:326).
//! Before the fix, nothing in the port ever accepted — `accept_one` had no
//! caller outside `mod tests` — and `drvAsynIPServerPortConfigure` was not an
//! iocsh command at all. A TCP client sat in the backlog until it timed out
//! while the port reported Connected.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use asyn_rs::iocsh::build_asyn_commands;
use asyn_rs::manager::PortManager;
use asyn_rs::services::PortServices;
use asyn_rs::sync_io::SyncIOHandle;
use asyn_rs::trace::TraceManager;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandContext, CommandDef};

fn make_ctx() -> CommandContext {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let handle = rt.handle().clone();
    let ctx = CommandContext::new(Arc::new(PvDatabase::new()), handle);
    std::mem::forget(rt);
    ctx
}

fn find<'a>(cmds: &'a [CommandDef], name: &str) -> &'a CommandDef {
    cmds.iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name} not registered as an iocsh command"))
}

/// Take a free TCP port by binding one and letting it go — the server binds the
/// same number a moment later. (`bind_port = 0` would be cleaner, but the whole
/// point of this test is the st.cmd path, which names a port.)
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// ```text
/// drvAsynIPServerPortConfigure("R1858", "127.0.0.1:<port> TCP", 2, 0, 0, 0)
/// ```
///
/// After that line an st.cmd IOC is serving: a client connects and its bytes
/// reach the child port that owns the slot.
#[test]
fn an_iocsh_configured_server_port_accepts_a_client() {
    let services = PortServices::new(Arc::new(TraceManager::new()));
    let mgr = Arc::new(PortManager::with_services(services));
    let cmds = build_asyn_commands(mgr);
    let ctx = make_ctx();

    let port = free_port();
    find(&cmds, "drvAsynIPServerPortConfigure")
        .handler
        .call(
            &[
                ArgValue::String("R1858".into()),
                ArgValue::String(format!("127.0.0.1:{port} TCP")),
                ArgValue::Int(2),
            ],
            &ctx,
        )
        .expect("drvAsynIPServerPortConfigure failed");

    // C pre-creates the child ports at configure — device support binds to them
    // before any client has connected.
    for name in ["R1858:0", "R1858:1"] {
        assert!(
            asyn_rs::asyn_record::get_port(name).is_some(),
            "child port {name} must exist after configure \
             (drvAsynIPServerPort.c:681-708)"
        );
    }

    asyn_rs::asyn_record::get_port("R1858").expect("server port not registered");
    // Auto-connect binds the listener; wait for it rather than assuming a
    // scheduling order.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut client = loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(c) => break c,
            Err(e) => {
                assert!(Instant::now() < deadline, "server never bound {port}: {e}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };

    client.write_all(b"hello-server").unwrap();
    client.flush().unwrap();

    // The listener thread must have accepted and filled slot 0, so the child
    // port that owns the slot can read the bytes. Before the fix this read
    // timed out: the connection was never accepted.
    let child = asyn_rs::asyn_record::get_port("R1858:0").unwrap();
    let io = SyncIOHandle::from_handle(child.handle.clone(), 0, Duration::from_millis(500));
    let mut got: Vec<u8> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while got.is_empty() {
        assert!(
            Instant::now() < deadline,
            "no bytes reached the child port — the server never accepted the client \
             (C: connectionListener, drvAsynIPServerPort.c:326)"
        );
        match io.read_octet(0, 32) {
            Ok(data) => got.extend_from_slice(&data),
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    assert_eq!(got, b"hello-server");

    let mut buf = [0u8; 16];
    io.write_octet(0, b"hello-client")
        .expect("child port write failed");
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let n = client.read(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"hello-client");
}
