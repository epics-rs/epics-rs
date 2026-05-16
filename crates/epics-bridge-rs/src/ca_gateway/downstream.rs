//! Downstream CA server adapter for the gateway.
//!
//! Hosts a [`CaServer`] backed by a shadow [`PvDatabase`]. The shadow
//! database is populated by the [`UpstreamManager`] as upstream
//! subscriptions establish. Downstream clients see PVs as if they
//! were normal in-process PVs — the gateway is transparent on the wire.
//!
//! ## Lazy resolution and optional preloading
//!
//! Resolution is lazy on-demand, matching C++ ca-gateway: a downstream
//! search for an unknown name triggers an upstream subscription, and the
//! PV is added to the shadow database only after the upstream IOC
//! responds. This is implemented by `GatewayServer::install_search_resolver`
//! (see `server.rs`), which installs a `PvDatabase::set_search_resolver`
//! hook — the epics-rs equivalent of C++ ca-gateway's
//! `gateServer::pvExistTest()`.
//!
//! Eager pre-subscription is still available as an *opt-in* convenience:
//! when `GatewayConfig::preload_path` is set, `GatewayServer::preload_pvs`
//! subscribes to a known set of upstream PVs at startup. It is not
//! required for lazy resolution to work.

use std::collections::VecDeque;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_ca_rs::server::{CaServer, ServerConnectionEvent};
use tokio::sync::Mutex;
use tokio::sync::broadcast;

use crate::error::BridgeResult;

/// B11: a connection event tagged with a monotonic sequence number.
///
/// The sequence number lets a lagged consumer ask the replay log for
/// "everything after the last event I saw" so per-PV subscriber
/// refcounts in the cache stay correct even when the consumer
/// transiently falls behind the broadcast.
pub type SeqConnEvent = (u64, ServerConnectionEvent);

/// Capacity of the connection-event replay ring buffer (B11).
///
/// Sized well above the broadcast channel capacity so a consumer can
/// lag by the full channel depth and still find every missed event in
/// the log. Connection events are small (`SocketAddr` + short `String`
/// + `u32`), so 4096 entries is a few hundred KiB at most.
const REPLAY_LOG_CAPACITY: usize = 4096;

/// Capacity of the re-broadcast channel feeding [`ReplayingReceiver`]s
/// (B11). When a consumer falls more than this many events behind, the
/// channel reports `Lagged` and the consumer recovers the gap from the
/// [`ConnEventReplay`] ring buffer instead of silently dropping events.
const REPLAY_CHANNEL_CAPACITY: usize = 1024;

/// B11: bounded replay log for downstream connection events.
///
/// The C++ ca-gateway drives its virtual-channel refcounts straight
/// off the libca event callbacks, which are never dropped. The Rust
/// gateway fans connection events through a `tokio::broadcast`
/// channel, which *can* drop events for a slow consumer (`Lagged`).
/// Previously the gateway's consumer only logged the lag, leaving
/// per-PV subscriber refcounts in [`super::cache::GwPvEntry`]
/// permanently skewed by the dropped CREATE/CLEAR pairs.
///
/// This log keeps the most recent [`REPLAY_LOG_CAPACITY`] events,
/// each tagged with a monotonic sequence number. On lag, a
/// [`ReplayingReceiver`] consults [`Self::events_since`] to recover
/// exactly the events it missed, in order, so the refcount math is
/// made whole.
pub struct ConnEventReplay {
    /// Recent events, oldest first. Bounded — the oldest entry is
    /// dropped once `REPLAY_LOG_CAPACITY` is exceeded.
    log: parking_lot::Mutex<VecDeque<SeqConnEvent>>,
}

impl ConnEventReplay {
    fn new() -> Self {
        Self {
            log: parking_lot::Mutex::new(VecDeque::with_capacity(REPLAY_LOG_CAPACITY)),
        }
    }

    /// Append an event to the ring buffer, evicting the oldest when
    /// at capacity. Called only by the single forwarder task, so the
    /// `seq` values are strictly increasing.
    fn record(&self, ev: SeqConnEvent) {
        let mut log = self.log.lock();
        if log.len() == REPLAY_LOG_CAPACITY {
            log.pop_front();
        }
        log.push_back(ev);
    }

    /// Return every logged event whose sequence number is strictly
    /// greater than `after`, oldest first.
    ///
    /// The second element of the tuple is `true` when the requested
    /// `after` is older than everything still in the log — i.e. the
    /// gap is larger than the ring buffer and some events are
    /// genuinely unrecoverable. The caller surfaces that as a warning;
    /// it is the only residual lossy case and is far less likely than
    /// the channel-depth lag this log is sized to absorb.
    pub fn events_since(&self, after: u64) -> (Vec<SeqConnEvent>, bool) {
        let log = self.log.lock();
        let oldest = log.front().map(|(s, _)| *s);
        let truncated = match oldest {
            // `after + 1` is the first seq the caller still needs; if
            // the log's oldest entry is newer than that, the gap
            // overflowed the ring buffer.
            Some(o) => o > after.saturating_add(1),
            None => false,
        };
        let missed = log
            .iter()
            .filter(|(seq, _)| *seq > after)
            .cloned()
            .collect();
        (missed, truncated)
    }
}

/// B11: a connection-event receiver that transparently replays events
/// missed during a broadcast `Lagged` instead of dropping them.
///
/// Wraps the re-broadcast `broadcast::Receiver<SeqConnEvent>` and the
/// shared [`ConnEventReplay`] log. [`Self::recv`] hands the caller one
/// event at a time, exactly like a plain `broadcast::Receiver`, but on
/// an internal `Lagged` it pulls the missed events out of the replay
/// log and feeds them out before resuming the live stream — so the
/// caller never observes a gap.
pub struct ReplayingReceiver {
    rx: broadcast::Receiver<SeqConnEvent>,
    replay: Arc<ConnEventReplay>,
    /// Sequence number of the last event handed to the caller. Used
    /// as the lower bound for `events_since` on lag recovery.
    last_seq: u64,
    /// Events recovered from the replay log on the most recent lag,
    /// not yet handed to the caller. Drained front-first before the
    /// next `rx.recv()`.
    pending: VecDeque<ServerConnectionEvent>,
}

/// Outcome of [`ReplayingReceiver::recv`].
#[derive(Debug)]
pub enum ConnEventRecv {
    /// A connection event — either live or replayed from the log.
    Event(ServerConnectionEvent),
    /// The broadcast sender was dropped and the log is drained; the
    /// consumer loop should terminate.
    Closed,
    /// A lag overflowed the replay log: `missed` events before the
    /// next delivered event are genuinely unrecoverable. The next
    /// `recv` resumes the live stream. The consumer should treat the
    /// affected PVs' refcounts as approximate until the next
    /// CREATE/CLEAR cycle and log the gap.
    GapTruncated { missed: u64 },
}

impl ReplayingReceiver {
    /// Receive the next connection event, replaying any events missed
    /// during a broadcast lag.
    ///
    /// Returns [`ConnEventRecv::Event`] for each event (live or
    /// replayed), [`ConnEventRecv::Closed`] once the stream ends, and
    /// [`ConnEventRecv::GapTruncated`] only in the residual case where
    /// a lag exceeded the replay log capacity.
    pub async fn recv(&mut self) -> ConnEventRecv {
        loop {
            // Drain any events recovered from the replay log first.
            if let Some(ev) = self.pending.pop_front() {
                return ConnEventRecv::Event(ev);
            }
            match self.rx.recv().await {
                Ok((seq, ev)) => {
                    // Skip events already handed to the caller via the
                    // replay log. After a lag recovery the broadcast
                    // channel still has its buffered tail (the events
                    // it did NOT drop); those overlap the replayed
                    // span, so without this guard the caller would see
                    // them twice and double-count the refcount.
                    if seq <= self.last_seq {
                        continue;
                    }
                    self.last_seq = seq;
                    return ConnEventRecv::Event(ev);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // The channel dropped events for this slow
                    // consumer. Recover the exact gap from the replay
                    // log: every event with seq > last_seq.
                    let (missed, truncated) = self.replay.events_since(self.last_seq);
                    if truncated {
                        // The gap overflowed the ring buffer. Report
                        // how many events are unrecoverable, advance
                        // `last_seq` past them so the next recovery
                        // does not double-count, then continue with
                        // whatever the log still holds.
                        let recovered_lo =
                            missed.first().map(|(s, _)| *s).unwrap_or(self.last_seq + 1);
                        let lost = recovered_lo.saturating_sub(self.last_seq + 1);
                        for (seq, ev) in missed {
                            self.last_seq = seq;
                            self.pending.push_back(ev);
                        }
                        return ConnEventRecv::GapTruncated { missed: lost };
                    }
                    for (seq, ev) in missed {
                        self.last_seq = seq;
                        self.pending.push_back(ev);
                    }
                    // Loop: drain `pending`, then resume live recv.
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return ConnEventRecv::Closed;
                }
            }
        }
    }
}

/// Downstream CA server adapter.
///
/// Wraps a [`CaServer`] that serves the gateway's shadow [`PvDatabase`].
/// All actual CA protocol handling (search, connect, get, put, monitor)
/// is delegated to `epics-ca-rs`.
///
/// The CaServer is held inside a `Mutex` because [`CaServer::connection_events`]
/// requires `&mut self` to install the broadcast sender on first call.
/// After installation, the server is moved out and run.
pub struct DownstreamServer {
    server: Mutex<Option<CaServer>>,
    /// Cached shadow DB pointer for `database()` accessor.
    shadow_db: Arc<PvDatabase>,
    /// B11: connection-event replay infrastructure. Lazily set up by
    /// the first [`Self::connection_events`] call: the re-broadcast
    /// sender, the shared replay log, and the forwarder task handle.
    /// `None` until then (and after [`Self::run`] has consumed the
    /// inner server). Held in a `Mutex` so `connection_events` can
    /// take `&self`.
    replay_state: Mutex<Option<ReplayState>>,
}

/// B11: lazily-constructed state backing [`ReplayingReceiver`]s.
struct ReplayState {
    /// Re-broadcast channel. Every [`ReplayingReceiver`] subscribes
    /// here; the forwarder task is the sole sender.
    tx: broadcast::Sender<SeqConnEvent>,
    /// Shared bounded replay log consulted on consumer lag.
    replay: Arc<ConnEventReplay>,
    /// Forwarder task: drains the CaServer broadcast, sequence-numbers
    /// each event, appends to the log, and re-broadcasts on `tx`.
    forwarder: tokio::task::JoinHandle<()>,
}

impl DownstreamServer {
    /// Create a new downstream server bound to `port`, serving from
    /// the given shadow database.
    pub fn new(shadow_db: Arc<PvDatabase>, port: u16) -> Self {
        let server = CaServer::from_parts(shadow_db.clone(), port, None, None, None, None);
        Self {
            server: Mutex::new(Some(server)),
            shadow_db,
            replay_state: Mutex::new(None),
        }
    }

    /// Variant of [`Self::new`] that also wraps every accepted
    /// connection in TLS. The gateway terminates TLS from clients;
    /// upstream traffic is encrypted independently when
    /// `GatewayConfig::upstream_tls` is set (B10) — see `upstream.rs`
    /// `UpstreamManager::new`. Available with the `ca-gateway-tls`
    /// feature.
    #[cfg(feature = "ca-gateway-tls")]
    pub fn new_with_tls(
        shadow_db: Arc<PvDatabase>,
        port: u16,
        tls: std::sync::Arc<epics_ca_rs::tls::ServerConfig>,
    ) -> Self {
        let mut server = CaServer::from_parts(shadow_db.clone(), port, None, None, None, None);
        server.set_tls(tls);
        Self {
            server: Mutex::new(Some(server)),
            shadow_db,
            replay_state: Mutex::new(None),
        }
    }

    /// Get the underlying shadow database.
    pub fn database(&self) -> &Arc<PvDatabase> {
        &self.shadow_db
    }

    /// Subscribe to connection lifecycle events with replay-on-lag
    /// (B11). Must be called BEFORE [`run`] (which moves the server
    /// out of the Mutex).
    ///
    /// The first call wires up the replay infrastructure: it takes the
    /// raw `CaServer` broadcast receiver and spawns a forwarder task
    /// that sequence-numbers every event, appends it to a bounded
    /// [`ConnEventReplay`] log, and re-broadcasts it. Each returned
    /// [`ReplayingReceiver`] subscribes to that re-broadcast and, on a
    /// `Lagged`, recovers the missed events from the log instead of
    /// dropping them — keeping the gateway's per-PV subscriber
    /// refcounts correct.
    ///
    /// Subsequent calls return additional independent receivers backed
    /// by the same forwarder + log.
    ///
    /// Returns `None` once [`run`] has consumed the inner server.
    pub async fn connection_events(&self) -> Option<ReplayingReceiver> {
        let mut replay_guard = self.replay_state.lock().await;
        if replay_guard.is_none() {
            // First call: install the forwarder. Take the raw
            // CaServer broadcast receiver while the server is still
            // present.
            let raw_rx = {
                let mut server_guard = self.server.lock().await;
                match server_guard.as_mut() {
                    Some(s) => s.connection_events(),
                    None => return None,
                }
            };
            let (tx, _) = broadcast::channel::<SeqConnEvent>(REPLAY_CHANNEL_CAPACITY);
            let replay = Arc::new(ConnEventReplay::new());
            let forwarder = spawn_conn_event_forwarder(raw_rx, tx.clone(), replay.clone());
            *replay_guard = Some(ReplayState {
                tx,
                replay,
                forwarder,
            });
        }
        let state = replay_guard.as_ref().expect("just initialised");
        Some(ReplayingReceiver {
            rx: state.tx.subscribe(),
            replay: state.replay.clone(),
            last_seq: 0,
            pending: VecDeque::new(),
        })
    }

    /// Abort the B11 connection-event forwarder task, if one was
    /// started. Called by the gateway's shutdown path so the
    /// forwarder does not outlive the server.
    pub async fn stop_connection_events(&self) {
        if let Some(state) = self.replay_state.lock().await.take() {
            state.forwarder.abort();
        }
    }

    /// Snapshot the beacon-anomaly Notify handle so the gateway can
    /// fire `generateBeaconAnomaly`-style pulses after [`run`] has
    /// consumed the inner CaServer. Must be called BEFORE `run`;
    /// returns None afterwards.
    pub async fn beacon_anomaly_handle(&self) -> Option<Arc<tokio::sync::Notify>> {
        let guard = self.server.lock().await;
        guard.as_ref().map(|s| s.beacon_anomaly_handle())
    }

    /// Run the CA server (blocks until shutdown).
    ///
    /// Spawn this in a tokio task — it accepts incoming TCP connections
    /// from downstream clients, handles UDP search broadcasts, and emits
    /// beacons. After this is called, [`connection_events`] returns None.
    pub async fn run(&self) -> BridgeResult<()> {
        let server = {
            let mut guard = self.server.lock().await;
            match guard.take() {
                Some(s) => s,
                None => {
                    return Err(crate::error::BridgeError::PutRejected(
                        "DownstreamServer already running or consumed".into(),
                    ));
                }
            }
        };
        server
            .run()
            .await
            .map_err(|e| crate::error::BridgeError::PutRejected(format!("CaServer run: {e}")))
    }

    /// Reinstall the inner [`CaServer`] after a previous [`run`] returned.
    /// Used by the supervisor when a CA server task crashes — the outer
    /// supervise loop reconstructs a server (with the same shadow DB)
    /// and re-attaches it here so the next [`run`] picks it up.
    /// Returns the previously installed server, if any.
    pub async fn reinstall(&self, server: CaServer) -> Option<CaServer> {
        let mut guard = self.server.lock().await;
        let prev = guard.take();
        *guard = Some(server);
        prev
    }
}

/// B11: spawn the connection-event forwarder.
///
/// Drains the raw `CaServer` broadcast, assigns a strictly-increasing
/// sequence number to every event, appends it to the shared replay
/// log, and re-broadcasts the sequenced event on `tx` for the
/// gateway's [`ReplayingReceiver`]s.
///
/// The forwarder does almost no work per event, so it keeps up with
/// the `CaServer` even under load — if the raw broadcast *itself*
/// lags (an extreme burst), the sequence numbers skip the dropped
/// span and the replay log simply never holds those events. That
/// residual loss surfaces to consumers as
/// [`ConnEventRecv::GapTruncated`], the same path as a ring-buffer
/// overflow. It is far less likely than a slow downstream consumer,
/// which is fully covered.
fn spawn_conn_event_forwarder(
    mut raw_rx: broadcast::Receiver<ServerConnectionEvent>,
    tx: broadcast::Sender<SeqConnEvent>,
    replay: Arc<ConnEventReplay>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Sequence numbers start at 1 so a consumer's initial
        // `last_seq = 0` means "I have seen nothing"; `events_since(0)`
        // then returns the whole log.
        let mut seq: u64 = 0;
        loop {
            match raw_rx.recv().await {
                Ok(ev) => {
                    seq += 1;
                    let sequenced = (seq, ev);
                    replay.record(sequenced.clone());
                    // A send error means there are no live receivers
                    // right now; the event is still in the replay log,
                    // so a receiver that subscribes later can recover
                    // it. Nothing to do here.
                    let _ = tx.send(sequenced);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // The raw CaServer broadcast dropped `n` events
                    // before the forwarder could read them. Advance
                    // the sequence counter past the gap so consumers
                    // can detect it as an unrecoverable truncation
                    // rather than silently renumbering.
                    seq += n;
                    tracing::warn!(
                        missed = n,
                        "ca-gateway-rs: raw connection-event broadcast lagged at \
                         the forwarder — these events cannot be replayed"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // CaServer dropped its sender — server stopped.
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn construct_downstream() {
        let db = Arc::new(PvDatabase::new());
        let downstream = DownstreamServer::new(db.clone(), 0);
        // Just verify it constructs without panicking
        assert!(Arc::ptr_eq(downstream.database(), &db));
    }

    #[tokio::test]
    async fn connection_events_subscribe() {
        let db = Arc::new(PvDatabase::new());
        let downstream = DownstreamServer::new(db, 0);
        let rx = downstream.connection_events().await;
        assert!(rx.is_some(), "expected receiver");
        downstream.stop_connection_events().await;
    }

    // --- B11: connection-event replay-on-lag ---

    fn ev(pv: &str, cid: u32) -> ServerConnectionEvent {
        ServerConnectionEvent::ChannelCreated {
            peer: "127.0.0.1:5064".parse().unwrap(),
            pv_name: pv.to_string(),
            cid,
        }
    }

    #[test]
    fn replay_log_records_and_queries() {
        let log = ConnEventReplay::new();
        for i in 1..=5u64 {
            log.record((i, ev("PV", i as u32)));
        }
        // events_since(0) returns everything.
        let (all, truncated) = log.events_since(0);
        assert_eq!(all.len(), 5);
        assert!(!truncated);
        // events_since(3) returns seq 4 and 5 only.
        let (tail, truncated) = log.events_since(3);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].0, 4);
        assert_eq!(tail[1].0, 5);
        assert!(!truncated);
        // events_since(5) returns nothing.
        let (none, _) = log.events_since(5);
        assert!(none.is_empty());
    }

    #[test]
    fn replay_log_is_bounded_and_reports_truncation() {
        let log = ConnEventReplay::new();
        // Overflow the ring buffer by one full capacity.
        let total = (REPLAY_LOG_CAPACITY + 100) as u64;
        for i in 1..=total {
            log.record((i, ev("PV", (i % 1000) as u32)));
        }
        // A consumer that last saw seq 0 cannot recover the dropped
        // prefix — the log only holds the most recent CAPACITY events.
        let (recovered, truncated) = log.events_since(0);
        assert_eq!(recovered.len(), REPLAY_LOG_CAPACITY);
        assert!(truncated, "gap past ring capacity must report truncation");
        // The oldest surviving event is seq (total - CAPACITY + 1).
        assert_eq!(recovered[0].0, total - REPLAY_LOG_CAPACITY as u64 + 1);
        // A consumer near the head recovers cleanly with no truncation.
        let (tail, truncated) = log.events_since(total - 10);
        assert_eq!(tail.len(), 10);
        assert!(!truncated);
    }

    #[tokio::test]
    async fn replaying_receiver_delivers_events_in_order() {
        let replay = Arc::new(ConnEventReplay::new());
        let (tx, rx) = broadcast::channel::<SeqConnEvent>(REPLAY_CHANNEL_CAPACITY);
        let mut recv = ReplayingReceiver {
            rx,
            replay: replay.clone(),
            last_seq: 0,
            pending: VecDeque::new(),
        };
        for i in 1..=3u64 {
            let sequenced = (i, ev("PV", i as u32));
            replay.record(sequenced.clone());
            tx.send(sequenced).unwrap();
        }
        for i in 1..=3u32 {
            match recv.recv().await {
                ConnEventRecv::Event(ServerConnectionEvent::ChannelCreated { cid, .. }) => {
                    assert_eq!(cid, i);
                }
                other => panic!("expected event {i}, got {other:?}"),
            }
        }
        drop(tx);
        assert!(matches!(recv.recv().await, ConnEventRecv::Closed));
    }

    /// The core B11 guarantee: when the broadcast channel drops events
    /// for a slow consumer (`Lagged`), `ReplayingReceiver::recv`
    /// recovers every missed event from the replay log so the consumer
    /// observes no gap.
    #[tokio::test]
    async fn replaying_receiver_recovers_lagged_events() {
        let replay = Arc::new(ConnEventReplay::new());
        // Tiny channel so a modest burst overflows it.
        let (tx, rx) = broadcast::channel::<SeqConnEvent>(4);
        let mut recv = ReplayingReceiver {
            rx,
            replay: replay.clone(),
            last_seq: 0,
            pending: VecDeque::new(),
        };
        // Push 20 events: the channel (cap 4) drops the older 16 for
        // this un-drained receiver, but every one is in the replay log.
        const N: u32 = 20;
        for i in 1..=N {
            let sequenced = (i as u64, ev("PV", i));
            replay.record(sequenced.clone());
            let _ = tx.send(sequenced);
        }
        // Despite the lag, recv must yield all 20 events, in order,
        // with no gap and no truncation.
        let mut seen = Vec::new();
        for _ in 0..N {
            match recv.recv().await {
                ConnEventRecv::Event(ServerConnectionEvent::ChannelCreated {
                    cid, ..
                }) => seen.push(cid),
                ConnEventRecv::GapTruncated { missed } => {
                    panic!("unexpected truncation, missed={missed}")
                }
                other => panic!("unexpected recv outcome: {other:?}"),
            }
        }
        assert_eq!(seen, (1..=N).collect::<Vec<_>>(), "lagged events not fully replayed");
        drop(tx);
        assert!(matches!(recv.recv().await, ConnEventRecv::Closed));
    }

    /// End-to-end through the forwarder: events published to the raw
    /// CaServer-style broadcast are sequenced, logged, and delivered
    /// via a `ReplayingReceiver` even when the *consumer* lags on the
    /// re-broadcast channel.
    ///
    /// The raw channel is sized to the burst so the forwarder never
    /// lags on it — this test isolates the consumer-lag replay path,
    /// which is the B11 guarantee. (Forwarder-lag is the separate
    /// residual `GapTruncated` case, exercised by
    /// `replay_log_is_bounded_and_reports_truncation`.)
    #[tokio::test]
    async fn forwarder_sequences_and_replays() {
        const N: u32 = 30;
        // Raw channel large enough that the forwarder never lags.
        let (raw_tx, raw_rx) = broadcast::channel::<ServerConnectionEvent>(N as usize);
        // Re-broadcast channel deliberately small so the consumer
        // lags and must recover from the replay log.
        let (tx, _keepalive) = broadcast::channel::<SeqConnEvent>(4);
        let replay = Arc::new(ConnEventReplay::new());
        let forwarder = spawn_conn_event_forwarder(raw_rx, tx.clone(), replay.clone());

        let mut recv = ReplayingReceiver {
            rx: tx.subscribe(),
            replay: replay.clone(),
            last_seq: 0,
            pending: VecDeque::new(),
        };

        for i in 1..=N {
            raw_tx.send(ev("PV", i)).unwrap();
        }
        // Give the forwarder time to drain the raw broadcast into the
        // replay log + re-broadcast channel.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut seen = Vec::new();
        for _ in 0..N {
            match recv.recv().await {
                ConnEventRecv::Event(ServerConnectionEvent::ChannelCreated {
                    cid, ..
                }) => seen.push(cid),
                other => panic!("unexpected recv outcome: {other:?}"),
            }
        }
        assert_eq!(seen, (1..=N).collect::<Vec<_>>());
        forwarder.abort();
    }
}
