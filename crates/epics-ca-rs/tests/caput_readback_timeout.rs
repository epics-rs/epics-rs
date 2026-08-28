//! Regression test (R9-18): a post-put readback timeout is NOT fatal in
//! `caput`.
//!
//! C `caput.c:583` calls `caget()` after the put and returns its status as
//! the process exit code (`caput.c:589`). Inside that `caget()` only
//! `if (!nConn) return 1` (`caput.c:181`) yields a non-zero return — a
//! `ca_pend_io` TIMEOUT merely prints "Read operation timed out: PV data was
//! not read." on stderr (`caput.c:186-188`, no `return`) and falls through to
//! the print loop, which renders the `calloc`'d, never-filled buffer
//! (`caput.c:167,201-209`) and returns 0 (`caput.c:239`). So C emits a
//! `New : <name> <zeroed value>` line and exits 0.
//!
//! Pre-fix `caput-rs` classified the readback timeout as fatal: it printed
//! the same stderr warning and then `exit(1)` with NO `New :` line — which
//! breaks scripts keying on caput's exit status.
//!
//! The scenario needs a PV that CONNECTS and accepts the write but never
//! answers a get. A framing proxy in front of a real `CaServer` provides
//! exactly that: it relays everything except `CA_PROTO_READ_NOTIFY`, which
//! it swallows, so every `ca_array_get` runs out its `-w` window.

#![cfg(tokio_backend)]

use std::net::SocketAddr;
use std::time::Duration;

use epics_base_rs::server::records::ai::AiRecord;
use epics_ca_rs::protocol::{CA_PROTO_READ_NOTIFY, CA_PROTO_SEARCH, CaHeader};
use epics_ca_rs::server::CaServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::process::Command;

/// Bind the proxy's UDP socket and TCP listener on the SAME port number,
/// holding both — a port is TAKEN by binding it, never probed and handed on.
///
/// The proxy needs one number on both protocols, so it cannot use the plain
/// `.port(0)` + read-back pattern of a single socket: bind TCP on `:0` to
/// take a number, then bind UDP on that number; retry the pair with a fresh
/// number if the UDP side is taken. There is no drop→rebind window at any
/// point — under a parallel test run the old probe-then-drop pattern lost
/// the number to a neighbour and the bind `expect` panicked the test.
///
/// TCP anchors the pair, not UDP: Windows CI runners carry Hyper-V
/// administered port exclusions on the TCP side, and the UDP ephemeral
/// allocator is sequential — a UDP-first anchor that wanders into a
/// TCP-excluded block stays inside it for every retry (observed on GitHub
/// runners: 10 straight anchors in one block, every TCP bind refused). The
/// TCP allocator never hands out a number from its own excluded ranges.
async fn bind_proxy_pair() -> (UdpSocket, TcpListener, u16) {
    const ATTEMPTS: usize = 10;
    for _ in 0..ATTEMPTS {
        let tcp = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind proxy TCP");
        let port = tcp.local_addr().expect("proxy TCP addr").port();
        if let Ok(udp) = UdpSocket::bind(("127.0.0.1", port)).await {
            return (udp, tcp, port);
        }
    }
    panic!("no same-numbered UDP+TCP port pair in {ATTEMPTS} attempts");
}

/// Relay one direction of a CA circuit, frame by frame, dropping every
/// `CA_PROTO_READ_NOTIFY`. Frames are 16-byte header + payload; the PV under
/// test is a scalar, so no extended (0xffff) header can appear.
async fn relay(mut from: tokio::net::tcp::OwnedReadHalf, mut to: tokio::net::tcp::OwnedWriteHalf) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = match from.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);

        while buf.len() >= CaHeader::SIZE {
            let cmmd = u16::from_be_bytes([buf[0], buf[1]]);
            let postsize = u16::from_be_bytes([buf[2], buf[3]]) as usize;
            let total = CaHeader::SIZE + postsize;
            if buf.len() < total {
                break;
            }
            let frame: Vec<u8> = buf.drain(..total).collect();
            // Swallow the get — request or response, whichever direction this
            // relay carries. The peer never sees it, so the client's
            // `ca_array_get` times out on a fully connected channel.
            if cmmd == CA_PROTO_READ_NOTIFY {
                continue;
            }
            if to.write_all(&frame).await.is_err() {
                return;
            }
        }
    }
}

/// Stand a UDP+TCP proxy in front of the server. The UDP half relays the
/// name search to `server_udp` and rewrites the TCP port the server
/// advertises in its SEARCH reply (header `data_type`) to the proxy's own,
/// so the client's data circuit lands here and is relayed to `server_tcp`.
/// Returns the proxy port.
///
/// The two server ports are distinct: the server bound them itself from
/// `.port(0)`, so the UDP search port and the TCP data port are separate
/// ephemerals.
async fn read_dropping_proxy(server_udp: u16, server_tcp: u16) -> u16 {
    let (udp, tcp, proxy_port) = bind_proxy_pair().await;
    let search: SocketAddr = format!("127.0.0.1:{server_udp}").parse().unwrap();
    let circuit: SocketAddr = format!("127.0.0.1:{server_tcp}").parse().unwrap();

    tokio::spawn(async move {
        let upstream = UdpSocket::bind(("127.0.0.1", 0))
            .await
            .expect("bind proxy upstream UDP");
        let mut buf = [0u8; 4096];
        loop {
            let Ok((n, client)) = udp.recv_from(&mut buf).await else {
                return;
            };
            if upstream.send_to(&buf[..n], search).await.is_err() {
                continue;
            }
            let mut reply = [0u8; 4096];
            let Ok(Ok((rn, _))) =
                tokio::time::timeout(Duration::from_secs(2), upstream.recv_from(&mut reply)).await
            else {
                continue;
            };
            // Patch every SEARCH reply's advertised TCP port (m_dataType) to
            // the proxy's, so the client opens its data circuit through us.
            let mut off = 0usize;
            while off + CaHeader::SIZE <= rn {
                let cmmd = u16::from_be_bytes([reply[off], reply[off + 1]]);
                let postsize = u16::from_be_bytes([reply[off + 2], reply[off + 3]]) as usize;
                if cmmd == CA_PROTO_SEARCH {
                    reply[off + 4..off + 6].copy_from_slice(&proxy_port.to_be_bytes());
                }
                off += CaHeader::SIZE + postsize;
            }
            let _ = udp.send_to(&reply[..rn], client).await;
        }
    });

    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = tcp.accept().await else {
                return;
            };
            let Ok(upstream) = TcpStream::connect(circuit).await else {
                continue;
            };
            let (cr, cw) = client.into_split();
            let (sr, sw) = upstream.into_split();
            tokio::spawn(relay(cr, sw));
            tokio::spawn(relay(sr, cw));
        }
    });

    proxy_port
}

/// `caput` against a PV whose get never answers: C warns on stderr, prints
/// the zeroed readback as `New :`, and exits 0.
#[tokio::test(flavor = "multi_thread")]
async fn caput_readback_timeout_prints_the_new_line_and_exits_zero() {
    let server = CaServer::builder()
        .port(0)
        .record("R918:PUT", AiRecord::new(1.0))
        .build()
        .await
        .expect("build CA server");
    let (server_udp, server_tcp) = (server.udp_port(), server.tcp_port());
    tokio::spawn(async move { server.run().await });

    let proxy_port = read_dropping_proxy(server_udp, server_tcp).await;

    // `-w` is the single pend_io window covering BOTH the connect (search +
    // data circuit, relayed through this test's UDP+TCP proxy) and the readback
    // get. Only the readback is under test, and it is DETERMINISTIC: the proxy
    // swallows every CA_PROTO_READ_NOTIFY, so the get times out no matter how
    // long the window is. A too-tight window instead makes the load-sensitive
    // path — the proxy-relayed connect — race the clock, so under load caput
    // aborts with "Channel connect timed out" and never reaches the readback
    // this test asserts on. 2 s matches the sibling `caput_old_read_never_aborts`
    // (same proxy shape, same deterministically-denied read).
    let out = Command::new(env!("CARGO_BIN_EXE_caput-rs"))
        .args(["-w", "2", "R918:PUT", "42"])
        .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{proxy_port}"))
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", proxy_port.to_string())
        .output()
        .await
        .expect("run caput-rs");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stderr.contains("Read operation timed out: PV data was not read."),
        "C warns on the readback timeout (caput.c:188), got stderr: {stderr:?}"
    );
    assert!(
        stdout.contains("New : R918:PUT"),
        "C still prints the New line after a readback timeout (caput.c:581-583); got: {stdout:?}"
    );
    // C prints the calloc'd, never-filled DBR_TIME_DOUBLE buffer: a zeroed
    // double renders as `0` (caput.c:201-209 + val2str).
    let new_line = stdout
        .lines()
        .find(|l| l.starts_with("New : "))
        .expect("a New line");
    assert!(
        new_line.trim_end().ends_with(" 0"),
        "the timed-out readback renders C's zeroed buffer; got: {new_line:?}"
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "a readback timeout does NOT change caput's exit status (caput.c:239); \
         stderr: {stderr:?}"
    );
}
