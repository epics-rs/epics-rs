//! Server-side TCP "nameserver publish" mode.
//!
//! Mirrors the pvxs feature where a server periodically pushes
//! `CMD_BEACON` frames over TCP to one or more configured nameserver
//! daemons (`pvxsrnssd`-style). Clients then resolve PVs by sending
//! TCP `CMD_SEARCH` to the nameserver instead of UDP-broadcasting.
//!
//! What this module does:
//!
//! 1. Accepts a list of TCP nameserver `SocketAddr`s.
//! 2. Spawns one supervisor task per address.
//! 3. Each task opens a TCP connection, immediately sends a
//!    `build_beacon` frame, then re-sends on every `period` interval
//!    (matching the existing UDP beacon cadence).
//! 4. On TCP error or peer close, reconnects with exponential
//!    backoff (250 ms → 30 s cap) — same shape as the gateway
//!    upstream-monitor reconnect loop.
//!
//! What this module does NOT do (intentional scope cut):
//!
//! - It does not implement the *receiving* side of the TCP
//!   nameserver protocol — that's a separate daemon (pvxs's
//!   `pvxsrnssd`) that any pvxs-compatible nameserver runs.
//! - It does not register channels by name; the BEACON frame
//!   carries the server identity (GUID + addr + change_count) and
//!   the nameserver consults its own server registry.
//!
//! Usage from the runtime: pass a list of TCP nameserver addresses
//! into [`spawn_nameserver_publishers`] alongside the existing
//! UDP beacon emitter; the two run independently.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::proto::ByteOrder;

use super::udp::build_beacon;

/// Per-publisher state. Returned by [`spawn_nameserver_publishers`]
/// so callers can later cancel / inspect.
pub struct NameserverPublisher {
    pub addr: SocketAddr,
    pub handle: JoinHandle<()>,
}

/// Spawn one supervisor task per configured TCP nameserver address.
/// Each task connects, sends a BEACON, then loops sending periodic
/// BEACONs separated by `period`. On error the task waits with
/// exponential backoff (250 ms → 30 s) and reconnects.
///
/// `change_count` is shared with the rest of the server; the
/// nameserver-publish path reads it on every emission so a topology
/// change visible to the UDP beacon path is also reflected in the
/// next TCP beacon. The runtime increments the AtomicU16 on
/// addPV / removePV.
pub fn spawn_nameserver_publishers(
    addrs: Vec<SocketAddr>,
    guid: [u8; 12],
    tcp_port: u16,
    order: ByteOrder,
    period: Duration,
    change_count: Arc<AtomicU16>,
) -> Vec<NameserverPublisher> {
    addrs
        .into_iter()
        .map(|addr| {
            let cc = change_count.clone();
            let handle = tokio::spawn(run_publisher_loop(addr, guid, tcp_port, order, period, cc));
            NameserverPublisher { addr, handle }
        })
        .collect()
}

async fn run_publisher_loop(
    addr: SocketAddr,
    guid: [u8; 12],
    tcp_port: u16,
    order: ByteOrder,
    period: Duration,
    change_count: Arc<AtomicU16>,
) {
    let mut backoff = Duration::from_millis(250);
    let backoff_cap = Duration::from_secs(30);
    let mut sequence: u8 = 0;

    loop {
        match TcpStream::connect(addr).await {
            Ok(mut stream) => {
                tracing::info!(
                    nameserver = %addr,
                    "nameserver publish: connected, beginning BEACON push"
                );
                // Reset backoff on successful connect.
                backoff = Duration::from_millis(250);

                // Drain a BEACON onto the connection now, then every
                // `period` until the connection drops. This advertises
                // a plain-TCP server endpoint, so the beacon protocol
                // tag is "tcp".
                loop {
                    let cc = change_count.load(Ordering::Relaxed);
                    let frame = build_beacon(guid, tcp_port, order, sequence, cc, "tcp");
                    sequence = sequence.wrapping_add(1);
                    if let Err(e) = stream.write_all(&frame).await {
                        tracing::warn!(
                            nameserver = %addr,
                            error = %e,
                            "nameserver publish: write failed; reconnecting"
                        );
                        break;
                    }
                    if let Err(e) = stream.flush().await {
                        tracing::warn!(
                            nameserver = %addr,
                            error = %e,
                            "nameserver publish: flush failed; reconnecting"
                        );
                        break;
                    }
                    sleep(period).await;
                }
            }
            Err(e) => {
                tracing::debug!(
                    nameserver = %addr,
                    error = %e,
                    backoff_ms = backoff.as_millis() as u64,
                    "nameserver publish: connect failed"
                );
            }
        }

        sleep(backoff).await;
        backoff = (backoff * 2).min(backoff_cap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    /// Spin up a fake nameserver: accept TCP connections, read the
    /// first 8 bytes (PVA header), and verify the publisher emitted
    /// a CMD_BEACON frame.
    #[tokio::test]
    async fn publisher_connects_and_sends_beacon() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let guid = [0xABu8; 12];
        let change_count = Arc::new(AtomicU16::new(7));

        let _publishers = spawn_nameserver_publishers(
            vec![addr],
            guid,
            5075,
            ByteOrder::Little,
            Duration::from_millis(100),
            change_count,
        );

        let (mut sock, _peer) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("publisher connected within 2s")
            .unwrap();

        // PVA header: 0xCA, version, flags, command, payload_len(u32)
        // = 8 bytes. Beacon command = 0x00.
        let mut header = [0u8; 8];
        tokio::time::timeout(Duration::from_secs(2), sock.read_exact(&mut header))
            .await
            .expect("header arrived in 2s")
            .expect("read_exact ok");
        assert_eq!(header[0], 0xCA, "PVA magic byte");
        assert_eq!(header[3], 0x00, "CMD_BEACON code");
    }

    /// When the listener closes, the publisher should reconnect and
    /// not panic. We just verify the supervisor task survives.
    #[tokio::test]
    async fn publisher_survives_connection_drop() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let guid = [0u8; 12];
        let change_count = Arc::new(AtomicU16::new(0));

        let publishers = spawn_nameserver_publishers(
            vec![addr],
            guid,
            5075,
            ByteOrder::Little,
            Duration::from_millis(50),
            change_count,
        );

        // Accept and drop — publisher should reconnect.
        let (sock, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("first connect")
            .unwrap();
        drop(sock);

        // Re-accept proves the publisher reconnected.
        let _second = tokio::time::timeout(Duration::from_secs(3), listener.accept())
            .await
            .expect("publisher reconnected within 3s")
            .unwrap();

        // Still running.
        for p in &publishers {
            assert!(!p.handle.is_finished());
        }
    }

    #[tokio::test]
    async fn empty_addrs_yields_no_publishers() {
        let pubs = spawn_nameserver_publishers(
            Vec::new(),
            [0u8; 12],
            5075,
            ByteOrder::Little,
            Duration::from_secs(15),
            Arc::new(AtomicU16::new(0)),
        );
        assert!(pubs.is_empty());
    }
}
