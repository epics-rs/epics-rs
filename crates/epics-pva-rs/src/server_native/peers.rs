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
//! take the write lock briefly; the [`crate::server_native::PvaServer::report`] read takes
//! the read lock for the snapshot. Concurrent connection handlers
//! never block each other on this lock — each holds its own
//! [`Arc<PeerEntry>`] and updates its own atomic counters without
//! re-entering the registry.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// Per-channel report entry — mirrors pvxs `Report::Channel`
/// (`netcommon.h:43-52`): the served PV name plus per-channel transmit /
/// receive byte counters and source-supplied contextual info. One
/// `ChannelStat` is shared (via `Arc`) between the connection task's
/// channel table and the [`PeerEntry`] so the report can read the live
/// counters the handlers mutate without copying names every create/destroy.
///
/// pvxs accumulates `chan->statTx`/`chan->statRx` at each op's send/recv
/// (`serverget.cpp:124/386`, `servermon.cpp:186/513`, `serverchan.cpp:151`,
/// `serverintrospect.cpp:45/164`) and copies them into the report at
/// `server.cpp:260-268`, zeroing per-channel when `report(true)`.
#[derive(Debug)]
pub struct ChannelStat {
    /// Served PV name (aka channel name).
    pub name: String,
    /// Bytes transmitted to the peer for this channel.
    pub tx: AtomicU64,
    /// Bytes received from the peer for this channel.
    pub rx: AtomicU64,
    /// Source-supplied contextual info (pvxs `ReportInfo`,
    /// netcommon.h:70). Populated at CREATE_CHANNEL from the bound owner's
    /// [`crate::server_native::ChannelSource::channel_report_info`] hook
    /// via [`Self::set_report_info`] — the single writer — and `None` when
    /// the source attaches nothing (the default).
    pub(crate) report_info: parking_lot::Mutex<Option<String>>,
}

impl ChannelStat {
    pub(crate) fn new(name: String) -> Arc<Self> {
        Arc::new(Self {
            name,
            tx: AtomicU64::new(0),
            rx: AtomicU64::new(0),
            report_info: parking_lot::Mutex::new(None),
        })
    }

    /// Record source-supplied contextual info for this channel — the
    /// single writer of [`Self::report_info`]. Mirrors pvxs
    /// `ServerChannelControl::updateInfo` (`serverchan.cpp`), which stashes
    /// the `ReportInfo` a Source hands it so `Server::report()` can surface
    /// it (`schan.info = chan->reportInfo`). Stored behind the existing
    /// mutex so a later source update can overwrite the value captured at
    /// channel open; `None` clears it.
    pub(crate) fn set_report_info(&self, info: Option<String>) {
        *self.report_info.lock() = info;
    }

    /// Attribute `n` transmitted bytes to this channel (pvxs
    /// `chan->statTx += enqueueTxBody(cmd)`).
    pub(crate) fn add_tx(&self, n: usize) {
        self.tx.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// Attribute `n` received bytes to this channel (pvxs
    /// `chan->statRx += rxlen`). Low-level primitive; production op
    /// handlers must use [`Self::add_op_rx`] so the 8-byte header is
    /// never dropped from the count.
    pub(crate) fn add_rx(&self, n: usize) {
        self.rx.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// Attribute one inbound op [`Frame`] to this channel, charging the
    /// full framed PVA byte count (`PvaHeader::SIZE + body`) — pvxs
    /// `rxlen = 8u + evbuffer_get_length(segBuf)` accumulated into
    /// `chan->statRx` (serverget.cpp:349/386, servermon.cpp:478/513,
    /// serverintrospect.cpp:145/164). This is the single per-channel
    /// op-RX owner: taking the received [`Frame`] rather than a raw
    /// `usize` is the structural guard against the under-count — every op handler
    /// previously called `add_rx(frame.payload.len())` and under-counted
    /// the header by 8 bytes per request, so no caller can pass the body
    /// length alone here.
    ///
    /// [`Frame`]: crate::decode::Frame
    pub(crate) fn add_op_rx(&self, frame: &crate::decode::Frame) {
        self.add_rx(crate::proto::PvaHeader::SIZE + frame.payload.len());
    }
}

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
    /// peer credentials `(account, method)` once the
    /// connection-validation handshake establishes them. pvxs
    /// `Server::report` includes `ReportInfo`/credentials per peer.
    pub(crate) credentials: parking_lot::Mutex<Option<(String, String)>>,
    /// Live channels currently open on this connection, keyed by server
    /// channel id (SID). Each value is the SAME `Arc<ChannelStat>` the
    /// connection task holds in its channel table, so the report reads the
    /// live per-channel tx/rx counters the handlers mutate. Inserted on
    /// CREATE_CHANNEL success, removed on DESTROY_CHANNEL / teardown
    /// (pvxs iterates `conn->chanBySID`, server.cpp:260).
    pub(crate) channels_by_sid: parking_lot::Mutex<HashMap<u32, Arc<ChannelStat>>>,
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
            channels_by_sid: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn touch_rx(&self, n: usize) {
        self.last_rx_nanos.store(now_nanos(), Ordering::Relaxed);
        self.bytes_in.fetch_add(n as u64, Ordering::Relaxed);
    }

    pub(crate) fn touch_tx(&self, n: usize) {
        self.bytes_out.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// record the validated peer credentials (set once at
    /// connection validation).
    pub(crate) fn set_credentials(&self, account: &str, method: &str) {
        *self.credentials.lock() = Some((account.to_string(), method.to_string()));
    }

    /// Register a newly-opened channel: bump the live + lifetime counts
    /// and store its shared `ChannelStat` keyed by SID so the report can
    /// read its per-channel tx/rx counters. The same `Arc` is held by the
    /// connection task's channel table, so handler-side `add_tx`/`add_rx`
    /// are visible to the report without re-mirroring.
    pub(crate) fn channel_opened(&self, sid: u32, stat: Arc<ChannelStat>) {
        self.channels.fetch_add(1, Ordering::Relaxed);
        self.channels_created.fetch_add(1, Ordering::Relaxed);
        self.channels_by_sid.lock().insert(sid, stat);
    }

    /// Deregister a channel on DESTROY / teardown: drop its report entry
    /// and decrement the live count. Mirrors pvxs dropping the channel
    /// from `conn->chanBySID`.
    pub(crate) fn channel_closed(&self, sid: u32) {
        if self.channels_by_sid.lock().remove(&sid).is_some() {
            self.channels.fetch_sub(1, Ordering::Relaxed);
        }
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

    /// snapshot, then optionally zero each peer's byte
    /// counters (pvxs `Server::report(bool zero)` — the next report
    /// returns deltas since this one). `connected_at`, channel counts,
    /// and credentials are NOT reset; only the byte counters.
    pub fn snapshot_zeroed(&self, zero: bool) -> Vec<(SocketAddr, PeerSnapshot)> {
        let g = self.inner.read();
        g.iter()
            .map(|(addr, e)| {
                let mut snap = PeerSnapshot::from(e.as_ref());
                if zero {
                    // `swap(0)` captures the exact pre-reset byte counts
                    // and clears them atomically; the snapshot reports the
                    // swapped values so an increment that `touch_rx` /
                    // `touch_tx` lands between the snapshot read and the
                    // reset is neither lost nor double-counted. A `store(0)`
                    // after the `From` load would drop it.
                    snap.bytes_in = e.bytes_in.swap(0, Ordering::Relaxed);
                    snap.bytes_out = e.bytes_out.swap(0, Ordering::Relaxed);
                    // Per-channel counters reset under the SAME report path
                    // as the connection counters (pvxs `server.cpp:270-271`
                    // zeroes `chan->statTx`/`statRx` when `zero`). Rebuild the
                    // per-channel snapshot from the swapped (pre-reset) values
                    // so per-PV deltas are neither lost nor double-counted.
                    let chans = e.channels_by_sid.lock();
                    snap.channels_detail = chans
                        .values()
                        .map(|c| ChannelReport {
                            name: c.name.clone(),
                            tx: c.tx.swap(0, Ordering::Relaxed),
                            rx: c.rx.swap(0, Ordering::Relaxed),
                            report_info: c.report_info.lock().clone(),
                        })
                        .collect();
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

/// Per-channel entry in a [`PeerSnapshot`] — mirrors pvxs
/// `Report::Channel` (`netcommon.h:43-52`): name + per-channel tx/rx byte
/// counters + optional source-supplied `ReportInfo`.
#[derive(Debug, Clone)]
pub struct ChannelReport {
    /// Served PV name (aka channel name).
    pub name: String,
    /// Bytes transmitted to the peer for this channel.
    pub tx: u64,
    /// Bytes received from the peer for this channel.
    pub rx: u64,
    /// Source-supplied contextual info (pvxs `ReportInfo`); `None` when the
    /// owning source's `channel_report_info` hook returned nothing.
    pub report_info: Option<String>,
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
    /// validated peer credentials `(account, method)`, or
    /// `None` before the connection-validation handshake completes.
    pub credentials: Option<(String, String)>,
    /// Per-channel report entries for the channels currently open on this
    /// peer — name + per-channel tx/rx counters + optional `ReportInfo`
    /// (pvxs `Report::Connection::channels`, server.cpp:260-268).
    pub channels_detail: Vec<ChannelReport>,
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
            channels_detail: e
                .channels_by_sid
                .lock()
                .values()
                .map(|c| ChannelReport {
                    name: c.name.clone(),
                    tx: c.tx.load(Ordering::Relaxed),
                    rx: c.rx.load(Ordering::Relaxed),
                    report_info: c.report_info.lock().clone(),
                })
                .collect(),
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
        entry.channel_opened(1, ChannelStat::new("X:PV".into()));
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

    /// the snapshot carries per-peer credentials and structured
    /// per-channel entries with their own tx/rx counters, and
    /// `snapshot_zeroed(true)` resets BOTH the connection AND the
    /// per-channel byte counters (not channels/credentials) so the next
    /// report is a delta — pvxs `server.cpp:256-271`.
    #[test]
    fn snapshot_carries_credentials_per_channel_counters_and_zeroes_bytes() {
        let reg = PeerRegistry::new();
        let addr: SocketAddr = "127.0.0.1:5076".parse().unwrap();
        let e = PeerEntry::new(true);
        e.set_credentials("op", "ca");
        let x = ChannelStat::new("X:PV".into());
        let y = ChannelStat::new("Y:PV".into());
        e.channel_opened(1, x.clone());
        e.channel_opened(2, y.clone());
        // per-channel attribution (what the handlers do on send/recv).
        x.add_rx(70);
        x.add_tx(30);
        y.add_rx(30);
        y.add_tx(10);
        e.touch_rx(100);
        e.touch_tx(40);
        reg.insert(addr, e);

        // report(false): credentials + per-channel detail + non-zero bytes.
        let s = &reg.snapshot()[0].1;
        assert_eq!(s.credentials, Some(("op".into(), "ca".into())));
        let mut names: Vec<&str> = s.channels_detail.iter().map(|c| c.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["X:PV", "Y:PV"]);
        let xr = s.channels_detail.iter().find(|c| c.name == "X:PV").unwrap();
        assert_eq!((xr.tx, xr.rx), (30, 70), "per-channel X counters");
        let yr = s.channels_detail.iter().find(|c| c.name == "Y:PV").unwrap();
        assert_eq!((yr.tx, yr.rx), (10, 30), "per-channel Y counters");
        assert_eq!((s.bytes_in, s.bytes_out), (100, 40));
        assert_eq!(s.channels, 2);

        // report(true): connection AND per-channel byte counters reset;
        // channels/credentials kept.
        let s = &reg.snapshot_zeroed(true)[0].1;
        assert_eq!((s.bytes_in, s.bytes_out), (100, 40), "snapshot is pre-zero");
        let xr = s.channels_detail.iter().find(|c| c.name == "X:PV").unwrap();
        assert_eq!((xr.tx, xr.rx), (30, 70), "per-channel snapshot is pre-zero");
        let s = &reg.snapshot()[0].1;
        assert_eq!(
            (s.bytes_in, s.bytes_out),
            (0, 0),
            "next report sees zeroed connection bytes"
        );
        let xr = s.channels_detail.iter().find(|c| c.name == "X:PV").unwrap();
        assert_eq!(
            (xr.tx, xr.rx),
            (0, 0),
            "next report sees zeroed per-channel bytes"
        );
        assert_eq!(s.channels, 2, "channel count not reset");
        assert_eq!(
            s.credentials,
            Some(("op".into(), "ca".into())),
            "creds not reset"
        );

        // channel_closed drops the per-channel report entry + live count.
        reg.snapshot(); // no-op read
        e_close(&reg, addr, 1);
        let s = &reg.snapshot()[0].1;
        assert_eq!(s.channels, 1, "closing SID 1 drops the live count");
        let names: Vec<&str> = s.channels_detail.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Y:PV"],
            "closed channel drops out of the report"
        );
    }

    /// Helper: close a channel via the registry's live entry.
    fn e_close(reg: &PeerRegistry, addr: SocketAddr, sid: u32) {
        reg.inner.read().get(&addr).unwrap().channel_closed(sid);
    }

    /// `snapshot_zeroed(true)` must not lose a byte increment
    /// that lands concurrently with the reset. Summing every drained
    /// delta against a writer thread must equal the total written —
    /// `swap(0)` guarantees this; a `load` then `store(0)` would drop the
    /// increments arriving between the read and the store.
    #[test]
    fn snapshot_zeroed_loses_no_concurrent_bytes() {
        use std::thread;
        let reg = PeerRegistry::new();
        let addr: SocketAddr = "127.0.0.1:5099".parse().unwrap();
        let e = PeerEntry::new(false);
        reg.insert(addr, e.clone());

        const N: u64 = 200_000;
        let writer = {
            let e = e.clone();
            thread::spawn(move || {
                for _ in 0..N {
                    e.touch_tx(1);
                }
            })
        };

        let mut drained = 0u64;
        loop {
            for (_, snap) in reg.snapshot_zeroed(true) {
                drained += snap.bytes_out;
            }
            if writer.is_finished() {
                break;
            }
        }
        writer.join().unwrap();
        // Final drain for bytes written after the last in-loop snapshot.
        for (_, snap) in reg.snapshot_zeroed(true) {
            drained += snap.bytes_out;
        }
        assert_eq!(drained, N, "no concurrent byte increment may be lost");
    }
}
