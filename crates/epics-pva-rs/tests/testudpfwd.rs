//! UDP forward integration tests for the `mshim-rs` binary,
//! mirroring pvxs `test/testudpfwd.cpp::testFwdVia`.
//!
//! Pattern: spawn `mshim-rs` as a subprocess with `-L 127.0.0.1:A -F
//! 127.0.0.1:B`, bind a sender socket and a receiver socket, send a
//! SEARCH to the listen port, verify a rebuilt SEARCH arrives at the
//! forward port (mshim decodes + rebuilds per destination — it does not
//! relay raw bytes), and that an unrecognized datagram is dropped.
//! `CARGO_BIN_EXE_mshim-rs` gives the test the path of the freshly-built
//! binary.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use epics_pva_rs::codec::PvaCodec;
use epics_pva_rs::server_native::udp::ForwardableDatagram;
use serial_test::serial;

/// Allocate two ephemeral UDP ports by binding+dropping. There's a
/// micro-window where another process could grab them, but for a
/// single-test loopback scenario the chance is negligible.
fn alloc_two_ports() -> (u16, u16) {
    let a = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let b = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let pa = a.local_addr().unwrap().port();
    let pb = b.local_addr().unwrap().port();
    drop(a);
    drop(b);
    (pa, pb)
}

struct ChildGuard(Option<Child>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[test]
#[serial]
fn mshim_forwards_search_and_drops_unrecognized() {
    // Skip on platforms where the binary path env var isn't set
    // (older cargo) — environment_var-based binary lookup is the
    // canonical way to find a sibling bin.
    let bin = match option_env!("CARGO_BIN_EXE_mshim-rs") {
        Some(p) => p,
        None => {
            eprintln!("CARGO_BIN_EXE_mshim-rs not set — skipping");
            return;
        }
    };

    let (listen_port, forward_port) = alloc_two_ports();

    // Start the receiver socket FIRST so we don't miss the forwarded
    // packet.
    let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, forward_port)).expect("bind receiver");
    receiver
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    // Spawn mshim-rs. Inherit stderr so a binding failure surfaces
    // in the test log instead of silently failing the wait loop.
    let child = Command::new(bin)
        .arg("-L")
        .arg(format!("127.0.0.1:{listen_port}"))
        .arg("-F")
        .arg(format!("127.0.0.1:{forward_port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn mshim-rs");
    let _guard = ChildGuard(Some(child));

    // Give mshim-rs ≥500ms to bind before the first probe so the
    // initial datagrams aren't tossed at a not-yet-listening port.
    std::thread::sleep(Duration::from_millis(500));

    let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind sender");
    let listen_addr: SocketAddr = format!("127.0.0.1:{listen_port}").parse().unwrap();

    // mshim-rs no longer relays raw bytes — it decodes each datagram as
    // SEARCH/BEACON and rebuilds a fresh wire body per destination (pvxs
    // `tools/mshim.cpp`). So the probe MUST be a real SEARCH; a garbage
    // datagram is dropped (asserted at the end). The reply addr is a
    // concrete loopback address (not `isAny`) so it is preserved verbatim
    // through the rebuild.
    let codec = PvaCodec::new();
    let search = codec.build_search(1, 7, "testpv1", [127, 0, 0, 1], 5566, true);

    // Try sending a few times — mshim-rs may not be listening yet. Verify
    // a recognizable SEARCH (not the raw bytes) arrives at the forward
    // port, carrying the original query.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut received = false;
    while Instant::now() < deadline {
        sender.send_to(&search, listen_addr).expect("send");
        let mut buf = [0u8; 256];
        if let Ok((n, _)) = receiver.recv_from(&mut buf) {
            let msgs = ForwardableDatagram::decode_all(&buf[..n]);
            assert_eq!(msgs.len(), 1, "forwarded datagram must hold one message");
            assert!(
                msgs[0].is_search(),
                "forwarded datagram must be a SEARCH, got {:?}",
                &buf[..n]
            );
            received = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(received, "mshim-rs did not forward the SEARCH datagram");

    // Negative contract: an unrecognized datagram is decoded as nothing
    // and therefore forwarded NOWHERE. mshim is proven up by the positive
    // path above, so a short window with no arrival is conclusive.
    receiver
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    sender
        .send_to(b"PVXS-TEST-FRAME", listen_addr)
        .expect("send garbage");
    let mut buf = [0u8; 256];
    match receiver.recv_from(&mut buf) {
        Ok((n, _)) => panic!(
            "mshim-rs forwarded an unrecognized datagram: {:?}",
            &buf[..n]
        ),
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {}
        Err(e) => panic!("unexpected recv error: {e}"),
    }
}

#[test]
#[serial]
fn mshim_rejects_invalid_listen_endpoint() {
    let bin = match option_env!("CARGO_BIN_EXE_mshim-rs") {
        Some(p) => p,
        None => return,
    };
    let out = Command::new(bin)
        .arg("-L")
        .arg("not-an-ip:5076")
        .arg("-F")
        .arg("127.0.0.1:5076")
        .output()
        .expect("spawn");
    // exit code 2 = parse error per our CLI contract.
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}
