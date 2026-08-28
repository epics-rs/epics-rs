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

// All three cases drive the `mshim-rs` binary as a subprocess. Its collectors
// wait on `server_native::udp`, which is `tokio_backend`-only, so on
// `exec_backend` the binary refuses at startup. Nothing replaces it there:
// an embedded image does not run mshim.
#![cfg(tokio_backend)]

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
    // `env!`, not `option_env!`: `mshim-rs` is an unconditional `[[bin]]` of
    // this package, so cargo always builds it for this test and an absent
    // path is a broken build rather than an unmet prerequisite. The three
    // `option_env!` arms this replaces returned early instead, and nextest
    // scores an early return as a pass.
    let bin = env!("CARGO_BIN_EXE_mshim-rs");

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

/// A single received datagram that chains two SEARCHes (`A || B`) must be
/// forwarded as ONLY the first rebuilt message — pvxs
/// `UDPCollector::process_one` decodes one PVA header per datagram and
/// ignores the tail (udp_collector.cpp:329-352). Pre-fix `mshim-rs`
/// decoded every chained message and sent each as its own datagram,
/// amplifying one received datagram into two forwarded ones.
///
/// `parse_search_request` is crate-private, so messages are told apart by
/// their SEARCH sequence number, which `build_search` writes (and the
/// forward rebuilder preserves) as the first 4 payload bytes after the
/// 8-byte PVA header. The assertion is "the tail SEARCH(B)'s seq is never
/// forwarded", which holds no matter how many times the probe is resent.
#[test]
#[serial]
fn mshim_forwards_only_first_chained_message() {
    let bin = env!("CARGO_BIN_EXE_mshim-rs");

    const PVA_HEADER_SIZE: usize = 8;
    const SEQ_A: u32 = 0x0000_0011;
    const SEQ_B: u32 = 0x0000_0022;

    let (listen_port, forward_port) = alloc_two_ports();

    let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, forward_port)).expect("bind receiver");
    receiver
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();

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

    std::thread::sleep(Duration::from_millis(500));

    let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind sender");
    let listen_addr: SocketAddr = format!("127.0.0.1:{listen_port}").parse().unwrap();

    // SEARCH(A) || SEARCH(B) packed into one datagram. Distinct seqs so a
    // forwarded frame can be attributed to A or B by its payload prefix.
    let codec = PvaCodec::new();
    let search_a = codec.build_search(SEQ_A, 101, "chainA", [127, 0, 0, 1], 5566, true);
    let search_b = codec.build_search(SEQ_B, 102, "chainB", [127, 0, 0, 1], 5566, true);
    let mut chained = search_a.clone();
    chained.extend_from_slice(&search_b);

    // Resend until the first message (A) is observed forwarded, asserting
    // on every arrival that B's seq was NOT forwarded. With the fix only
    // A is ever forwarded; pre-fix a B-seq datagram arrives and trips the
    // assertion. The deadline tolerates the bind/scheduling window.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_a = false;
    while Instant::now() < deadline && !saw_a {
        sender.send_to(&chained, listen_addr).expect("send chained");
        let mut buf = [0u8; 256];
        while let Ok((n, _)) = receiver.recv_from(&mut buf) {
            if n >= PVA_HEADER_SIZE + 4 {
                let seq = u32::from_le_bytes(
                    buf[PVA_HEADER_SIZE..PVA_HEADER_SIZE + 4]
                        .try_into()
                        .unwrap(),
                );
                assert_ne!(
                    seq, SEQ_B,
                    "chained tail SEARCH(B) must NOT be forwarded as its own datagram"
                );
                if seq == SEQ_A {
                    saw_a = true;
                }
            }
        }
    }
    assert!(saw_a, "the first chained SEARCH(A) must be forwarded");

    // Final drain: after A is seen, give a B datagram a chance to surface
    // (pre-fix it trails A) and assert it never does.
    let drain_until = Instant::now() + Duration::from_millis(600);
    let mut buf = [0u8; 256];
    while Instant::now() < drain_until {
        match receiver.recv_from(&mut buf) {
            Ok((n, _)) if n >= PVA_HEADER_SIZE + 4 => {
                let seq = u32::from_le_bytes(
                    buf[PVA_HEADER_SIZE..PVA_HEADER_SIZE + 4]
                        .try_into()
                        .unwrap(),
                );
                assert_ne!(
                    seq, SEQ_B,
                    "chained tail SEARCH(B) must NOT be forwarded as its own datagram"
                );
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
}

#[test]
#[serial]
fn mshim_rejects_invalid_listen_endpoint() {
    let bin = env!("CARGO_BIN_EXE_mshim-rs");
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
