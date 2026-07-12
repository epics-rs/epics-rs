//! Regression test (R9-21): the pre-put `Old :` read can NEVER abort `caput`.
//!
//! C `caput.c:531-535` calls `caget()` for the `Old :` display and DISCARDS
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

use std::net::SocketAddr;
use std::time::Duration;

use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::protocol::{CA_PROTO_READ_NOTIFY, CA_PROTO_SEARCH, CaHeader, ECA_NORDACCESS};
use epics_ca_rs::server::CaServer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::process::Command;

fn free_port() -> u16 {
    let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve free port");
    let p = probe.local_addr().expect("addr").port();
    drop(probe);
    p
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

/// Search-and-relay proxy: UDP searches are forwarded and their replies
/// re-pointed at the proxy's TCP port; the TCP circuit is relayed with every
/// read reply denied.
async fn read_denying_proxy(server_port: u16) -> u16 {
    let proxy_port = free_port();
    let server: SocketAddr = ([127, 0, 0, 1], server_port).into();

    let udp = UdpSocket::bind(("127.0.0.1", proxy_port))
        .await
        .expect("bind proxy UDP");
    tokio::spawn(async move {
        let upstream = UdpSocket::bind(("127.0.0.1", 0))
            .await
            .expect("bind proxy upstream UDP");
        let mut buf = [0u8; 4096];
        loop {
            let Ok((n, client)) = udp.recv_from(&mut buf).await else {
                return;
            };
            if upstream.send_to(&buf[..n], server).await.is_err() {
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

    let tcp = TcpListener::bind(("127.0.0.1", proxy_port))
        .await
        .expect("bind proxy TCP");
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = tcp.accept().await else {
                return;
            };
            let Ok(upstream) = TcpStream::connect(server).await else {
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
    let server_port = free_port();
    let server = CaServer::builder()
        .port(server_port)
        .record("R921:WRITEONLY", AiRecord::new(1.0))
        .build()
        .await
        .expect("build CA server");
    let db = server.database().clone();
    tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let proxy_port = read_denying_proxy(server_port).await;

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
        .await
        .expect("read the record back through the database");
    assert_eq!(
        stored,
        EpicsValue::Double(42.0),
        "the put must run even though the Old: read failed (caput.c:539-548)"
    );
}
