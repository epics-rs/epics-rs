//! Regression test (R9-21): the pre-put `Old :` read can NEVER abort `caput`.
//!
//! C `caput.c:532-535` calls `caget()` for the `Old :` display and DISCARDS
//! its return value — `result` is overwritten by the put that follows
//! (`caput.c:539-548`). Inside that `caget()`, a channel whose GET fails
//! (`ca_array_get` / the server's error reply) takes the `*** ...` marker path
//! (`caput.c:200-206`) and `caget()` still returns 0 (`caput.c:239`). So a PV
//! that answers a write but not a read still gets its put, prints the marker,
//! and exits 0.
//!
//! Pre-fix `caput-rs` treated the `Old :` read's failure as fatal: it printed
//! `error: <e>` on stderr and `exit(1)` BEFORE issuing the put, so the write
//! never reached the server.
//!
//! The scenario needs a PV that connects and accepts a write but fails every
//! read. A framing proxy in front of a real `CaServer` provides exactly that:
//! it relays every frame untouched except the `CA_PROTO_READ_NOTIFY` REPLY,
//! whose status word (`m_cid`) it stamps with `ECA_NORDACCESS` — the wire
//! shape of rsrv's `no_read_access_event` (`camessage.c:450-480`), which the
//! CA client surfaces as a failed get.

#![cfg(tokio_backend)]

use std::net::SocketAddr;
use std::time::Duration;

use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::protocol::{CA_PROTO_READ_NOTIFY, CA_PROTO_SEARCH, CaHeader, ECA_NORDACCESS};
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

/// Relay one direction of a CA circuit frame by frame. In the server→client
/// direction, stamp every `CA_PROTO_READ_NOTIFY` reply's status word
/// (`m_cid`, header bytes 8..12) with `ECA_NORDACCESS` so every get fails
/// while the write path stays untouched.
async fn relay(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    deny_reads: bool,
) {
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
            let mut frame: Vec<u8> = buf.drain(..total).collect();
            if deny_reads && cmmd == CA_PROTO_READ_NOTIFY {
                frame[8..12].copy_from_slice(&ECA_NORDACCESS.to_be_bytes());
            }
            if to.write_all(&frame).await.is_err() {
                return;
            }
        }
    }
}

/// Search-and-relay proxy: UDP searches are forwarded to `server_udp` and
/// their replies re-pointed at the proxy's TCP port; the circuit to
/// `server_tcp` is relayed with every read reply denied.
///
/// The two server ports are distinct: the server bound them itself from
/// `.port(0)`, so the UDP search port and the TCP data port are separate
/// ephemerals.
async fn read_denying_proxy(server_udp: u16, server_tcp: u16) -> u16 {
    let (udp, tcp, proxy_port) = bind_proxy_pair().await;
    let search: SocketAddr = ([127, 0, 0, 1], server_udp).into();
    let circuit: SocketAddr = ([127, 0, 0, 1], server_tcp).into();

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
            tokio::spawn(relay(cr, sw, false));
            tokio::spawn(relay(sr, cw, true));
        }
    });

    proxy_port
}

/// `caput` against a PV whose every get fails: C prints the `*** no read
/// access` marker on BOTH readback lines, still writes, and exits 0.
#[tokio::test(flavor = "multi_thread")]
async fn caput_writes_a_read_denied_pv_and_exits_zero() {
    let server = CaServer::builder()
        .port(0)
        .record("R921:WRITEONLY", AiRecord::new(1.0))
        .build()
        .await
        .expect("build CA server");
    let (server_udp, server_tcp) = (server.udp_port(), server.tcp_port());
    let db = server.database().clone();
    tokio::spawn(async move { server.run().await });

    let proxy_port = read_denying_proxy(server_udp, server_tcp).await;

    let out = Command::new(env!("CARGO_BIN_EXE_caput-rs"))
        .args(["-w", "2", "R921:WRITEONLY", "42"])
        .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{proxy_port}"))
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", proxy_port.to_string())
        .output()
        .await
        .expect("run caput-rs");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(
        out.status.code(),
        Some(0),
        "the Old: read's status is discarded (caput.c:535) and a marker-path \
         caget() returns 0 (caput.c:239); stdout: {stdout:?} stderr: {stderr:?}"
    );
    let old_line = stdout
        .lines()
        .find(|l| l.starts_with("Old : "))
        .unwrap_or_else(|| panic!("caput must still print an Old line; got: {stdout:?}"));
    assert!(
        old_line.contains("*** no read access"),
        "C prints ECA_NORDACCESS as `*** no read access` (caput.c:203-204); got: {old_line:?}"
    );

    // The put itself must have reached the record — that is what the pre-fix
    // `exit(1)` skipped.
    let stored = db
        .get_pv("R921:WRITEONLY")
        .expect("read the record back through the database");
    assert_eq!(
        stored,
        EpicsValue::Double(42.0),
        "the put must run even though the Old: read failed (caput.c:539-548)"
    );
}

/// Regression test (R9-23): a failed `New :` read prints C's marker, NOT the
/// value that was submitted.
///
/// The post-put `caget()` (`caput.c:583`) is the same print loop: a non-NORMAL
/// per-PV status takes the `*** no read access` / `*** CA error <msg>` branch
/// (`caput.c:201-206`) and `caget()` returns 0 (`caput.c:239`). caput-rs echoed
/// the submitted value on that line instead, so a PV whose readback failed
/// reported the write as if it had been read back and confirmed.
#[tokio::test(flavor = "multi_thread")]
async fn caput_new_read_error_prints_the_marker_not_the_submitted_value() {
    let server = CaServer::builder()
        .port(0)
        .record("R923:NOREAD", AiRecord::new(1.0))
        .build()
        .await
        .expect("build CA server");
    let (server_udp, server_tcp) = (server.udp_port(), server.tcp_port());
    tokio::spawn(async move { server.run().await });

    let proxy_port = read_denying_proxy(server_udp, server_tcp).await;

    let out = Command::new(env!("CARGO_BIN_EXE_caput-rs"))
        .args(["-w", "2", "R923:NOREAD", "42"])
        .env("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{proxy_port}"))
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_SERVER_PORT", proxy_port.to_string())
        .output()
        .await
        .expect("run caput-rs");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let new_line = stdout
        .lines()
        .find(|l| l.starts_with("New : "))
        .unwrap_or_else(|| panic!("caput must still print a New line; got: {stdout:?}"));

    assert!(
        new_line.contains("*** no read access"),
        "a failed post-put read prints C's marker (caput.c:203-204); got: {new_line:?}"
    );
    assert!(
        !new_line.contains("42"),
        "the submitted value must NOT be echoed as if it had been read back; \
         got: {new_line:?}"
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "the marker path still returns 0 (caput.c:239); stdout: {stdout:?}"
    );
}
