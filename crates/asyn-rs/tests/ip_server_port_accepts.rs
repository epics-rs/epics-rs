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
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    let ctx = CommandContext::new(Arc::new(PvDatabase::new()), bridge);
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
    let configure = find(&cmds, "drvAsynIPServerPortConfigure");

    // Probe-then-rebind race: `free_port()` drops its listener and the server
    // re-binds that number inside configure (`connect_blocking` -> `open_listener`
    // -> bind). Under parallel nextest a neighbour can steal the number in that
    // window. On a lost bind the handler reports "cannot listen" on the shell and
    // UNREGISTERS the server port (iocsh.rs:1602-1608) — it does NOT fail the
    // command — so a steal is exactly "the server port object is absent after the
    // call". Retry with a fresh number and a fresh, globally-unique port name
    // (asyn port names are a global registry) until our bind wins.
    let (name, port) = {
        let mut won = None;
        for attempt in 0..50 {
            let port = free_port();
            let name = format!("R1858_{attempt}");
            configure
                .handler
                .call(
                    &[
                        ArgValue::String(name.clone()),
                        ArgValue::String(format!("127.0.0.1:{port} TCP")),
                        ArgValue::Int(2),
                    ],
                    &ctx,
                )
                .expect("the command reports errors on the shell, it does not fail the command");
            // Registered ⟺ the synchronous `connect_blocking` bind won the number
            // (the handler unregisters on a bind failure). That is the steal test.
            if asyn_rs::asyn_record::get_port(&name).is_some() {
                won = Some((name, port));
                break;
            }
        }
        won.expect("could not win a free TCP port in 50 attempts")
    };

    // C pre-creates the child ports at configure — device support binds to them
    // before any client has connected.
    for slot in [0, 1] {
        let child = format!("{name}:{slot}");
        assert!(
            asyn_rs::asyn_record::get_port(&child).is_some(),
            "child port {child} must exist after configure \
             (drvAsynIPServerPort.c:681-708)"
        );
    }

    // The server owns the bind now (configure bound it synchronously before
    // registering), so the client connect is deterministic — but keep the wait
    // loop against listener-thread scheduling.
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
    let child = asyn_rs::asyn_record::get_port(&format!("{name}:0")).unwrap();
    let io = SyncIOHandle::from_handle(child.handle.clone(), 0, Duration::from_millis(500));
    let mut got: Vec<u8> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while got.is_empty() {
        assert!(
            Instant::now() < deadline,
            "no bytes reached the child port — the server never accepted the client \
             (C: connectionListener, drvAsynIPServerPort.c:326)"
        );
        // C parity: a child port is built by `drvAsynIPPortConfigure`
        // (drvAsynIPServerPort.c:688-694), so it carries the EOS interpose
        // (drvAsynIPPort.c:1065). `asynInterposeEos.c::readIt` keeps reading
        // until a terminator arrives; an un-terminated payload therefore comes
        // back as asynTimeout WITH the bytes already transferred attached
        // (readIt:242-253 runs the same tail on error as on success). The
        // bytes are delivered — the status is a timeout only because no
        // terminator followed them.
        match io.read_octet(0, 32) {
            Ok(data) => got.extend_from_slice(&data),
            Err(e) => match e.partial_read() {
                Some(partial) if !partial.data.is_empty() => got.extend_from_slice(&partial.data),
                _ => std::thread::sleep(Duration::from_millis(10)),
            },
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

/// R19-114: `maxClients = 0` is refused at the st.cmd boundary — no listening
/// port, no child ports.
///
/// C rejects it before it parses the protocol: *"No clients."*, return -1
/// (drvAsynIPServerPort.c:545-548). A server with zero slots would bind the
/// socket and destroy every connection it accepted.
#[test]
fn an_iocsh_server_port_with_zero_max_clients_is_not_created() {
    let services = PortServices::new(Arc::new(TraceManager::new()));
    let mgr = Arc::new(PortManager::with_services(services));
    let cmds = build_asyn_commands(mgr);
    let ctx = make_ctx();

    let port = free_port();
    find(&cmds, "drvAsynIPServerPortConfigure")
        .handler
        .call(
            &[
                ArgValue::String("R19114".into()),
                ArgValue::String(format!("127.0.0.1:{port} TCP")),
                ArgValue::Int(0),
            ],
            &ctx,
        )
        .expect("the command reports the error on the shell, it does not fail the shell");

    // The handler registers the port and only then binds inside `connect_blocking`
    // (iocsh.rs; a bind failure unregisters it, iocsh.rs:1602-1608). So a
    // successful bind ⟺ the port stays registered: `get_port` SOME is exactly
    // "a listener was bound". maxClients=0 is rejected at `with_config`, before
    // registration and before any bind (C drvAsynIPServerPort.c:545-548), so the
    // race-free registry check below is the whole proof — no listener, no child.
    //
    // The old test also re-bound the number to prove the socket was free. That
    // assert was both racy (a neighbour could grab the dropped probe number in the
    // window) AND redundant: a zero-slot server that regressed into binding would
    // bind the free number successfully and stay registered, so `get_port` SOME
    // already catches it. Holding the number instead would only mask that
    // regression (the server's bind would fail against the held socket and
    // unregister, flipping `get_port` back to none), so the socket assert is gone.
    assert!(
        asyn_rs::asyn_record::get_port("R19114").is_none(),
        "maxClients=0 must not produce a listening port"
    );
    assert!(
        asyn_rs::asyn_record::get_port("R19114:0").is_none(),
        "maxClients=0 must not produce a child port"
    );
}
