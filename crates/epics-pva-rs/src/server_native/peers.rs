//! Per-peer book-keeping for [`crate::server_native::PvaServer::report`].
//!
//! Mirrors pvxs `Server::report()` at the "live peers + per-peer
//! channel/op counts" granularity. The accept loop registers an entry
//! when it accepts a connection; the per-connection task updates the
//! mutable counters as it processes commands; the entry is removed on
//! disconnect.
//!
//! Lock granularity: the registry is a [`parking_lot::RwLock`] over a
//! [`std::collections::HashMap`]. Mutations (insert / remove / update)
//! take the write lock briefly; the [`PvaServer::report`] read takes
//! the read lock for the snapshot. Concurrent connection handlers
//! never block each other on this lock — each holds its own
//! [`Arc<PeerEntry>`] and updates its own atomic counters without
//! re-entering the registry.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// Per-connection counters held in [`PeerRegistry`].
///
/// Counters are [`AtomicU64`] so the connection handler can update them
/// without locking the registry. `connected_at` is set once at
/// registration; the rest grow over the connection's lifetime.
#[derive(Debug)]
pub struct PeerEntry {
    /// When the connection was accepted (server clock).
    pub connected_at: SystemTime,
    /// Last time the read loop bumped its rx watermark (Unix nanos).
    pub last_rx_nanos: AtomicU64,
    /// Live channels currently open on this connection.
    pub channels: AtomicU64,
    /// Total CREATE_CHANNEL successes since connect (resets to 0
    /// across reconnects since the entry is replaced).
    pub channels_created: AtomicU64,
    /// Total operation INITs (GET / PUT / MONITOR / RPC) seen.
    pub ops_init: AtomicU64,
    /// Total bytes read off the socket.
    pub bytes_in: AtomicU64,
    /// Total bytes pushed into the writer mpsc.
    pub bytes_out: AtomicU64,
    /// Whether TLS is in effect for this connection (recorded at
    /// accept). pvxs surfaces `secure` similarly.
    pub tls: bool,
    /// PVA-FR-2: peer credentials `(account, method)` once the
    /// connection-validation handshake establishes them. pvxs
    /// `Server::report` includes `ReportInfo`/credentials per peer.
    pub(crate) credentials: parking_lot::Mutex<Option<(String, String)>>,
    /// PVA-FR-2: live PV names of the channels currently open on this
    /// connection, mirrored from the per-connection channel table on
    /// every create/destroy so the report carries per-channel detail.
    pub(crate) channel_names: parking_lot::Mutex<Vec<String>>,
}

impl PeerEntry {
    pub(crate) fn new(tls: bool) -> Arc<Self> {
        Arc::new(Self {
            connected_at: SystemTime::now(),
            last_rx_nanos: AtomicU64::new(now_nanos()),
            channels: AtomicU64::new(0),
            channels_created: AtomicU64::new(0),
            ops_init: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            tls,
            credentials: parking_lot::Mutex::new(None),
            channel_names: parking_lot::Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn touch_rx(&self, n: usize) {
        self.last_rx_nanos.store(now_nanos(), Ordering::Relaxed);
        self.bytes_in.fetch_add(n as u64, Ordering::Relaxed);
    }

    pub(crate) fn touch_tx(&self, n: usize) {
        self.bytes_out.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// PVA-FR-2: record the validated peer credentials (set once at
    /// connection validation).
    pub(crate) fn set_credentials(&self, account: &str, method: &str) {
        *self.credentials.lock() = Some((account.to_string(), method.to_string()));
    }

    /// PVA-FR-2: mirror the connection's current open-channel PV names
    /// (the per-connection channel table is the source of truth; this
    /// snapshot is read by the report).
    pub(crate) fn set_channel_names(&self, names: Vec<String>) {
        *self.channel_names.lock() = names;
    }

    pub(crate) fn channel_added(&self) {
        self.channels.fetch_add(1, Ordering::Relaxed);
        self.channels_created.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn channel_removed(&self) {
        self.channels.fetch_sub(1, Ordering::Relaxed);
    }

    pub(crate) fn op_init(&self) {
        self.ops_init.fetch_add(1, Ordering::Relaxed);
    }
}

/// Concurrent map of `SocketAddr → Arc<PeerEntry>`. The accept loop
/// inserts on connect and removes on disconnect; the
/// `PvaServer::report()` reader snapshots without blocking writers.
#[derive(Debug, Default)]
pub struct PeerRegistry {
    inner: parking_lot::RwLock<HashMap<SocketAddr, Arc<PeerEntry>>>,
}

impl PeerRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn insert(&self, peer: SocketAddr, entry: Arc<PeerEntry>) {
        self.inner.write().insert(peer, entry);
    }

    pub(crate) fn remove(&self, peer: SocketAddr) {
        self.inner.write().remove(&peer);
    }

    /// Snapshot the registry into a Vec of (peer, snapshot) pairs.
    /// Cloned out so the caller doesn't hold the read lock across
    /// further work.
    pub fn snapshot(&self) -> Vec<(SocketAddr, PeerSnapshot)> {
        self.snapshot_zeroed(false)
    }

    /// PVA-FR-2: snapshot, then optionally zero each peer's byte
    /// counters (pvxs `Server::report(bool zero)` — the next report
    /// returns deltas since this one). `connected_at`, channel counts,
    /// and credentials are NOT reset; only the byte counters.
    pub fn snapshot_zeroed(&self, zero: bool) -> Vec<(SocketAddr, PeerSnapshot)> {
        let g = self.inner.read();
        g.iter()
            .map(|(addr, e)| {
                let snap = PeerSnapshot::from(e.as_ref());
                if zero {
                    e.bytes_in.store(0, Ordering::Relaxed);
                    e.bytes_out.store(0, Ordering::Relaxed);
                }
                (*addr, snap)
            })
            .collect()
    }

    /// Total number of currently-active connections.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Lock-free snapshot returned by [`PeerRegistry::snapshot`].
#[derive(Debug, Clone)]
pub struct PeerSnapshot {
    pub connected_at: SystemTime,
    pub last_rx_nanos: u64,
    pub channels: u64,
    pub channels_created: u64,
    pub ops_init: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub tls: bool,
    /// PVA-FR-2: validated peer credentials `(account, method)`, or
    /// `None` before the connection-validation handshake completes.
    pub credentials: Option<(String, String)>,
    /// PVA-FR-2: PV names of the channels currently open on this peer.
    pub channel_names: Vec<String>,
}

impl From<&PeerEntry> for PeerSnapshot {
    fn from(e: &PeerEntry) -> Self {
        Self {
            connected_at: e.connected_at,
            last_rx_nanos: e.last_rx_nanos.load(Ordering::Relaxed),
            channels: e.channels.load(Ordering::Relaxed),
            channels_created: e.channels_created.load(Ordering::Relaxed),
            ops_init: e.ops_init.load(Ordering::Relaxed),
            bytes_in: e.bytes_in.load(Ordering::Relaxed),
            bytes_out: e.bytes_out.load(Ordering::Relaxed),
            tls: e.tls,
            credentials: e.credentials.lock().clone(),
            channel_names: e.channel_names.lock().clone(),
        }
    }
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_remove_snapshot_roundtrip() {
        let reg = PeerRegistry::new();
        let addr: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        assert!(reg.is_empty());
        let entry = PeerEntry::new(false);
        entry.channel_added();
        entry.touch_rx(64);
        reg.insert(addr, entry.clone());
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        let (a, s) = &snap[0];
        assert_eq!(*a, addr);
        assert_eq!(s.channels, 1);
        assert_eq!(s.bytes_in, 64);
        reg.remove(addr);
        assert!(reg.is_empty());
    }

    /// PVA-FR-2: the snapshot carries per-peer credentials and live
    /// channel names, and `snapshot_zeroed(true)` resets only the byte
    /// counters (not channels/credentials) so the next report is a delta.
    #[test]
    fn snapshot_carries_credentials_channels_and_zeroes_bytes() {
        let reg = PeerRegistry::new();
        let addr: SocketAddr = "127.0.0.1:5076".parse().unwrap();
        let e = PeerEntry::new(true);
        e.set_credentials("op", "ca");
        e.channel_added();
        e.set_channel_names(vec!["X:PV".into(), "Y:PV".into()]);
        e.touch_rx(100);
        e.touch_tx(40);
        reg.insert(addr, e);

        // report(false): credentials + channel names + non-zero bytes.
        let s = &reg.snapshot()[0].1;
        assert_eq!(s.credentials, Some(("op".into(), "ca".into())));
        assert_eq!(
            s.channel_names,
            vec!["X:PV".to_string(), "Y:PV".to_string()]
        );
        assert_eq!((s.bytes_in, s.bytes_out), (100, 40));
        assert_eq!(s.channels, 1);

        // report(true): byte counters reset; channels/credentials kept.
        let s = &reg.snapshot_zeroed(true)[0].1;
        assert_eq!((s.bytes_in, s.bytes_out), (100, 40), "snapshot is pre-zero");
        let s = &reg.snapshot()[0].1;
        assert_eq!(
            (s.bytes_in, s.bytes_out),
            (0, 0),
            "next report sees zeroed bytes"
        );
        assert_eq!(s.channels, 1, "channel count not reset");
        assert_eq!(
            s.credentials,
            Some(("op".into(), "ca".into())),
            "creds not reset"
        );
    }
}
