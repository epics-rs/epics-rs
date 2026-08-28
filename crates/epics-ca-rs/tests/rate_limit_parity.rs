//! **Both CA receive loops enforce the rate policy, or neither does.**
//!
//! `EPICS_CAS_RATE_LIMIT_MSGS_PER_SEC` / `_BURST` / `_STRIKES` are documented
//! for the CA server with no host-only qualification, but the token draw lived
//! in `server::tcp::handle_client`, which no reactor-free build compiles at
//! all — so on RTEMS and VxWorks, where `server::blocking` is the only receive
//! loop, all three were inert. The gate now belongs to `RecvAccumulator`,
//! which both loops parse through; this file measures that on both rather than
//! reasoning from the shared type.
//!
//! The policy is read from the process environment per connection, which is
//! why these tests live in their own binary: a suite that sets a server-wide
//! rate cap must not be able to cap another suite's servers.
//!
//! Ports are always ephemeral (`:0`) — never the real 5064, per the
//! `build() ⟹ listening` port-ownership rule.

#![cfg(tokio_backend)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::protocol::{CA_MINOR_VERSION, CA_PROTO_ERROR, CA_PROTO_VERSION, CaHeader};
use epics_ca_rs::server::CaServer;
use epics_ca_rs::server::blocking::BlockingCaServer;
use serial_test::serial;

const PV: &str = "RATE:PARITY";
/// Long enough that a server which is *not* going to close has demonstrably
/// not closed, short enough to keep the suite quick.
const READ_TIMEOUT: Duration = Duration::from_millis(1500);

/// What a loop did with the script.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    /// Every frame the server sent back, by command.
    replies: Vec<u16>,
    /// Whether the server closed the circuit.
    closed: bool,
}

/// A bodiless `CA_PROTO_VERSION` — the smallest message that passes every C
/// gate, so what the rate gate does to it is all that is under test.
fn version_frame() -> Vec<u8> {
    let mut hdr = CaHeader::new(CA_PROTO_VERSION);
    hdr.count = CA_MINOR_VERSION;
    hdr.to_bytes().to_vec()
}

/// Walk a reply stream into commands. None of the replies a CA server sends
/// here uses the v4.9 extended header, so the 16-byte form is the whole
/// grammar this needs.
fn commands_of(buf: &[u8]) -> Vec<u16> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 16 <= buf.len() {
        out.push(u16::from_be_bytes([buf[i], buf[i + 1]]));
        let postsize = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        i += 16 + postsize;
    }
    out
}

/// Send every frame, then read until the server closes or goes quiet.
fn drive(addr: SocketAddr, script: &[Vec<u8>]) -> Outcome {
    let mut sock = TcpStream::connect(addr).expect("connect");
    sock.set_read_timeout(Some(READ_TIMEOUT)).expect("timeout");
    for frame in script {
        sock.write_all(frame).expect("send frame");
    }
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let closed = loop {
        match sock.read(&mut chunk) {
            Ok(0) => break true,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break false, // read timeout: still up
        }
    };
    Outcome {
        replies: commands_of(&buf),
        closed,
    }
}

fn seed_db() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    block_on_sync(db.add_pv(PV, EpicsValue::Double(1.5)))
        .expect("no async runtime on this thread")
        .expect("add_pv");
    db
}

/// The blocking driver — the one RTEMS and VxWorks run.
fn against_blocking(script: &[Vec<u8>]) -> Outcome {
    let server = Arc::new(
        BlockingCaServer::bind(
            "127.0.0.1:0",
            seed_db(),
            epics_base_rs::server::access_security::new_acf_cell(None),
        )
        .expect("bind ephemeral port"),
    );
    let addr = server.local_addr().expect("local_addr");
    let srv = server.clone();
    let accept = thread::spawn(move || srv.serve());

    let outcome = drive(addr, script);

    server.shutdown();
    let _ = accept.join();
    outcome
}

/// The async host driver.
fn against_async(script: &[Vec<u8>]) -> Outcome {
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let server_thread = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let server = CaServer::builder()
                .port(0)
                .tcp_port(0)
                .pv(PV, EpicsValue::Double(1.5))
                .build()
                .await
                .expect("build CA server");
            port_tx.send(server.tcp_port()).expect("report tcp port");
            tokio::select! {
                _ = server.run() => {}
                _ = tokio::task::spawn_blocking(move || { let _ = stop_rx.recv(); }) => {}
            }
        });
    });

    let port = port_rx.recv().expect("async server reports its port");
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let outcome = drive(addr, script);

    let _ = stop_tx.send(());
    let _ = server_thread.join();
    outcome
}

/// SAFETY: every test in this file is `#[serial]` and this is the only
/// binary that touches these three variables, so nothing reads them
/// concurrently. The policy is read per connection, so setting them before
/// the client connects is what takes effect.
fn set_policy(msgs_per_sec: Option<&str>, burst: &str, strikes: &str) {
    unsafe {
        match msgs_per_sec {
            Some(v) => std::env::set_var("EPICS_CAS_RATE_LIMIT_MSGS_PER_SEC", v),
            None => std::env::remove_var("EPICS_CAS_RATE_LIMIT_MSGS_PER_SEC"),
        }
        std::env::set_var("EPICS_CAS_RATE_LIMIT_BURST", burst);
        std::env::set_var("EPICS_CAS_RATE_LIMIT_STRIKES", strikes);
    }
}

/// One token, then two strikes: the third message ends the circuit. The peer
/// is told nothing — C has no reply for "too fast" and libca has no status to
/// carry one — so the only observable is the close.
#[test]
#[serial]
fn a_peer_over_its_rate_is_disconnected_by_both_loops() {
    set_policy(Some("1"), "1", "2");
    let script = vec![version_frame(), version_frame(), version_frame()];

    for (which, got) in [
        ("blocking", against_blocking(&script)),
        ("async", against_async(&script)),
    ] {
        assert!(
            got.closed,
            "{which} kept a circuit that crossed the strike threshold: {got:?}"
        );
        assert!(
            !got.replies.contains(&CA_PROTO_ERROR),
            "{which} answered a rate-limit disconnect with an error frame: {got:?}"
        );
    }
}

/// The boundary under it: the same script with the policy disabled — the
/// default — is three ordinary messages and the circuit stays up. Without
/// this, a server that closed for any other reason would pass the test above.
#[test]
#[serial]
fn the_same_script_keeps_the_circuit_when_the_policy_is_off() {
    set_policy(None, "0", "2");
    let script = vec![version_frame(), version_frame(), version_frame()];

    for (which, got) in [
        ("blocking", against_blocking(&script)),
        ("async", against_async(&script)),
    ] {
        assert!(
            !got.closed,
            "{which} closed a circuit with no rate policy configured: {got:?}"
        );
    }
}
