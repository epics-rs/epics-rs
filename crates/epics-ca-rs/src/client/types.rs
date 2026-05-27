use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Instant;

use dashmap::DashMap;
use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::sync::{mpsc, oneshot};
use epics_base_rs::types::{DbFieldType, EpicsValue};

use crate::channel::AccessRights;
use crate::client::state::ChannelState;

// --- Virtual-circuit identity ---

/// Identity of one CA virtual circuit: the server address paired with
/// the CA priority the channel was created at. libca keys its circuit
/// table (`caServerID`, `caServerID.h:28-38`) on exactly this pair —
/// `(sockaddr_in, ca_uint8_t pri)` — so two channels to the same IOC at
/// different priorities open independent TCP circuits. The Rust client
/// mirrors that: every map that used to be keyed by `SocketAddr`
/// (`connections`, `DirectServerWriters`, `ServerLastRxAt`, the
/// coordinator's `server_channels`, the per-circuit flow-control state)
/// is keyed by `CircuitKey`, and every `TransportCommand` carries the
/// `priority` so the transport manager can route to the right circuit.
///
/// `priority` is the libca CA priority (`0..=99`, `cacChannel::priorityMax`);
/// `0` is `priorityDefault` and reproduces the historical single-circuit
/// behaviour.
pub(crate) type CircuitKey = (SocketAddr, u8);

// --- Per-circuit last-RX timestamp sidecar (Option C, Phase D) ---

/// Last instant a frame was received from each server. Bumped by the
/// per-server transport `read_loop` whenever any TCP frame arrives —
/// covers `READ_NOTIFY`, `WRITE_NOTIFY`, `EVENT_ADD`, `ACCESS_RIGHTS`,
/// `CREATE_CH_RESP`, version negotiation, echoes, etc.
///
/// Phase A turned read/write responses into a transport-direct path
/// that never reaches the coordinator, so the coordinator can no
/// longer maintain this stamp from `TransportEvent`s alone — a
/// read-heavy client without monitors would look idle even though
/// frames are arriving every millisecond. The sidecar lets the
/// transport keep the stamp current and the coordinator (which is
/// the one answering `ca_receive_watchdog_delay`) read it directly.
///
/// keyed by [`CircuitKey`] so each priority circuit to one
/// server keeps an independent receive timestamp — libca's
/// `tcpRecvWatchdog` is per-circuit.
pub(crate) type ServerLastRxAt = Arc<DashMap<CircuitKey, Instant>>;

// --- Direct per-server writer sidecar (Option C, Phase E) ---

/// Send buffer backpressure threshold (matches C EPICS flushBlockThreshold).
/// If more than this many frames are pending, the connection is stalled.
pub(crate) const SEND_BACKPRESSURE_FRAMES: usize = 4096;

/// Cloneable write handle for an established virtual circuit.
///
/// Hot one-shot operations (`CaChannel::get` / `put`) use this sidecar
/// to enqueue frames straight to the per-server writer task, bypassing
/// the transport manager actor after the channel has already reached
/// `Operational`. Lifecycle operations still go through the transport
/// manager so connection setup/teardown remains centralized.
#[derive(Clone)]
pub(crate) struct DirectServerWriter {
    pub(crate) write_tx: mpsc::UnboundedSender<Vec<u8>>,
    pub(crate) pending_frames: Arc<AtomicUsize>,
}

impl DirectServerWriter {
    pub(crate) fn send_frame(&self, frame: Vec<u8>) -> CaResult<()> {
        let pending = self.pending_frames.load(Ordering::Relaxed);
        if pending >= SEND_BACKPRESSURE_FRAMES {
            return Err(CaError::Disconnected);
        }

        self.pending_frames.fetch_add(1, Ordering::Relaxed);
        if self.write_tx.send(frame).is_err() {
            // same accounting fix as write_loop — use atomic
            // CAS instead of load + store so a concurrent
            // `send_frame` increment cannot be silently overwritten.
            // The send-failure rollback decrements exactly one frame
            // and saturates at zero (a concurrent write_loop drain
            // may already have driven the counter below 1).
            let mut current = self.pending_frames.load(Ordering::Relaxed);
            loop {
                let next = current.saturating_sub(1);
                match self.pending_frames.compare_exchange_weak(
                    current,
                    next,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(observed) => current = observed,
                }
            }
            return Err(CaError::Disconnected);
        }
        Ok(())
    }
}

/// Shared server-writer registry. Transport manager publishes; channel hot
/// paths read.
///
/// keyed by [`CircuitKey`]. A channel's hot path looks up its
/// writer by `(server_addr, priority)`, so two priorities to one server
/// write to their own circuits.
pub(crate) type DirectServerWriters = Arc<DashMap<CircuitKey, DirectServerWriter>>;

// --- Channel snapshot sidecar (Option C, Phase B) ---

/// Immutable, per-channel snapshot published by the coordinator
/// whenever lifecycle state changes. CaChannel hot paths
/// (`ch.get` / `ch.put` / `ch.subscribe`) read from this map
/// directly instead of round-tripping `CoordRequest::GetChannelInfo`
/// for every operation.
///
/// The coordinator inserts/updates an entry on every relevant
/// `TransportEvent` (ChannelCreated, AccessRightsChanged, …) and
/// removes it on Drop. Stale-read window: a tiny race exists where
/// a CaChannel sees an old snapshot for one nanosecond after a
/// state change; that's acceptable because the request will either
/// fail at the server (which already knows the new state) or get
/// retried on disconnect drain.
#[derive(Clone)]
pub(crate) struct ChannelSnapshotPublic {
    pub sid: u32,
    pub native_type: DbFieldType,
    pub element_count: u32,
    pub server_addr: SocketAddr,
    pub access_rights: AccessRights,
    pub state: ChannelState,
}

/// Shared snapshot registry. Coordinator publishes; CaChannel reads.
pub(crate) type ChannelSnapshots = Arc<DashMap<u32, ChannelSnapshotPublic>>;

/// Per-channel SEARCH attempt counter (CA-035 `ca_search_attempts`).
/// SearchEngine bumps on every fanout call (immediate first SEARCH
/// after Schedule + each bucket-tick retransmit); one bump per
/// fanout regardless of how many UDP datagrams the addr_list /
/// nameserver duplication produces. CaChannel surfaces it via
/// [`super::CaChannel::search_attempts`]. Entry is removed when
/// the channel is cancelled or its connection succeeds (matching
/// libca, which resets attempts on circuit creation).
pub(crate) type SearchAttempts = Arc<DashMap<u32, std::sync::atomic::AtomicU32>>;

// --- CA-130 ca_add_exception_event ----------------------------------

/// Out-of-band / unrecoverable error categories surfaced via the
/// per-client exception handler. Mirrors the C `caEventHandlerArgs`
/// `op` field — but typed instead of a magic-number enum.
///
/// Variants are added when a real dispatch site exists. Adding a
/// variant without a live source would create a dead API; clients
/// would `match` on it and never receive the case.
///
/// `#[non_exhaustive]` so future variants (e.g. BeaconAnomaly when
/// that path gets a real source) can be added without breaking
/// downstream `match` blocks. Clients must include a `_ => …` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CaExceptionKind {
    /// Server emitted a `CA_PROTO_ERROR` (cmd=11) with a status code
    /// for an operation that wasn't otherwise routed to a callback.
    /// `status` carries the ECA code, `message` the optional payload.
    ServerError,
    /// Server-initiated channel close (`CA_PROTO_SERVER_DISCONN`).
    /// Per-op waiters tied to the channel are released with
    /// `Disconnected`; the handler additionally fires for callers
    /// who want a global notification stream.
    ServerDisconnect,
}

/// Single OOB-error notification delivered to a registered handler.
///
/// `#[non_exhaustive]` so additional context fields (e.g. timestamp,
/// retry-attempt count) can be added without breaking downstream
/// struct-literal construction. Construct via mutating an instance
/// from the public API or use functional update on a constructed
/// value; do not literal-init from the outside.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CaException {
    pub kind: CaExceptionKind,
    pub message: String,
    pub server_addr: Option<SocketAddr>,
    pub pv_name: Option<String>,
    /// ECA status code when applicable (server-error path).
    pub status: Option<u32>,
}

/// Boxed handler. Returns `()`; logs are emitted regardless so a
/// handler that panics or is slow can't suppress the existing
/// tracing diagnostics.
pub type CaExceptionHandler = Arc<dyn Fn(&CaException) + Send + Sync>;

/// Shared slot for the per-client handler. `parking_lot::RwLock`
/// keeps the read path lock-free in the common (no handler set)
/// case after the first install. One slot per CaClient instance —
/// not a process-global singleton.
pub(crate) type CaExceptionSlot = Arc<parking_lot::RwLock<Option<CaExceptionHandler>>>;

/// Best-effort dispatch — never panics, even if the handler does.
pub(crate) fn dispatch_exception(slot: &CaExceptionSlot, exc: CaException) {
    let handler = slot.read().clone();
    if let Some(h) = handler {
        // Catch panics so a buggy handler doesn't poison the
        // dispatching task. We can't recover the handler's bug but
        // we can keep the rest of the client functional.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h(&exc)));
    }
}

// --- Client identity (user / host advertised on circuit handshakes) ---

/// User and host names advertised to servers on every CA virtual-circuit
/// handshake (`CA_PROTO_CLIENT_NAME` / `CA_PROTO_HOST_NAME`).
///
/// libca resolves these once at context creation — user from `$USER`
/// (then `$USERNAME` on Windows), host from the local hostname — and the
/// `tcpiiu` constructor (`tcpiiu.cpp:755-762`) queues them on every new
/// circuit. This port keeps them in a shared slot instead so the names
/// can be overridden at runtime; the handshake builder reads the slot
/// rather than the environment.
#[derive(Clone, Debug)]
pub(crate) struct ClientIdentity {
    pub(crate) user: String,
    pub(crate) host: String,
}

impl ClientIdentity {
    /// Resolve the identity from the environment the way libca does:
    /// user from `$USER` falling back to `$USERNAME`, host from the
    /// local hostname.
    pub(crate) fn from_env() -> Self {
        let user = epics_base_rs::runtime::env::get("USER")
            .or_else(|| epics_base_rs::runtime::env::get("USERNAME"))
            .unwrap_or_else(|| "unknown".to_string());
        let host = epics_base_rs::runtime::env::hostname();
        Self { user, host }
    }
}

/// Shared, runtime-mutable [`ClientIdentity`]. Owned by the `CaClient`;
/// cloned into the transport manager so every new circuit handshake
/// reads the current value, and mutated by `CaClient::set_user_name` /
/// `set_host_name`.
pub(crate) type ClientIdentitySlot = Arc<parking_lot::RwLock<ClientIdentity>>;

// --- Direct in-flight op registries (Option C) ---

pub(crate) enum ReadReply {
    Plain {
        dbr_type: DbFieldType,
        value: EpicsValue,
    },
    Raw {
        data_type: u16,
        count: u32,
        data: Vec<u8>,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum ReadReplyMode {
    Plain,
    Raw,
}

/// Reply channel type for reads.
pub(crate) type ReadReplyTx = oneshot::Sender<CaResult<ReadReply>>;
/// Reply channel type for one-shot writes (write-notify completion).
pub(crate) type WriteReplyTx = oneshot::Sender<CaResult<()>>;

/// Reusable Sender slot used by `ReadWaiter::Warm` — the channel-side
/// caller refills it before each call, the dispatcher takes it on
/// response. Wrapped in a `parking_lot::Mutex` because both sides hold
/// the lock for nanoseconds (just `take`/`replace`).
pub(crate) type WarmReplySlot = Arc<parking_lot::Mutex<Option<ReadReplyTx>>>;

pub(crate) enum ReadWaiter {
    OneShot {
        cid: u32,
        mode: ReadReplyMode,
        reply_tx: ReadReplyTx,
    },
    /// Persistent waiter installed by `CachedRead`. Same ioid stays in
    /// the registry across calls so subsequent reads skip
    /// `alloc_ioid` + DashMap insert/remove. The dispatcher takes the
    /// `Sender` from `slot` on response without removing the entry; the
    /// channel-side caller refills `slot` before each frame send. See
    /// `transport::dispatch_read_reply_with` and
    /// `client::CaChannel::cached_read` for the full lifecycle.
    Warm {
        cid: u32,
        mode: ReadReplyMode,
        slot: WarmReplySlot,
    },
}

impl ReadWaiter {
    pub(crate) fn cid(&self) -> u32 {
        match self {
            Self::OneShot { cid, .. } => *cid,
            Self::Warm { cid, .. } => *cid,
        }
    }

    pub(crate) fn mode(&self) -> ReadReplyMode {
        match self {
            Self::OneShot { mode, .. } => *mode,
            Self::Warm { mode, .. } => *mode,
        }
    }

    /// Consume the waiter and signal `result`. Used by the disconnect
    /// drain path (`drain_waiters_for_cids`) where we want both
    /// `OneShot` and `Warm` waiters notified-and-evicted.
    pub(crate) fn send(self, result: CaResult<ReadReply>) {
        match self {
            Self::OneShot { reply_tx, .. } => {
                let _ = reply_tx.send(result);
            }
            Self::Warm { slot, .. } => {
                if let Some(tx) = slot.lock().take() {
                    let _ = tx.send(result);
                }
            }
        }
    }
}

/// Shared in-flight op registry. Channel handles insert reply oneshots
/// here keyed by `ioid`; the per-server transport read loop removes
/// and fulfils them on `ReadResponse` / `WriteResponse` arrival.
///
/// This replaces the previous design where every read/write went
/// through the coordinator's `tokio::select!` loop twice (once on
/// op submission to register the waiter, once on response to dispatch
/// it). With ~25 µs of coordinator-iteration overhead on each touch,
/// `bulk_caget(20)` showed ~1.8 ms wall time in benchmarks against a
/// localhost IOC despite the 20 spawned tasks all "running in
/// parallel". Routing reads/writes directly here removes both
/// touches; the coordinator only sees the lifecycle path
/// (`RegisterChannel`, search-found, TCP close, beacon anomaly).
///
/// The `cid` field stored alongside each reply lets the disconnect-
/// cleanup path filter pending ops by channel when a server's
/// virtual circuit dies (Phase D).
#[derive(Clone)]
pub(crate) struct InFlightOps {
    pub(crate) reads: Arc<DashMap<u32, ReadWaiter>>,
    pub(crate) writes: Arc<DashMap<u32, (u32, WriteReplyTx)>>,
    /// monotonic `ioid` source owned by the same registry
    /// that holds the live ids. Keeping the counter here (rather than
    /// a process-global static) lets [`Self::alloc_ioid`] probe
    /// `reads`/`writes` so a counter that wraps through 2^32 cannot
    /// reissue an id whose read/write is still pending. Shared across
    /// `InFlightOps` clones (it is `Arc`), so the coordinator and all
    /// channels of one client draw from a single id space.
    next_ioid: Arc<AtomicU32>,
}

impl InFlightOps {
    pub(crate) fn new() -> Self {
        Self {
            reads: Arc::new(DashMap::new()),
            writes: Arc::new(DashMap::new()),
            next_ioid: Arc::new(AtomicU32::new(1)),
        }
    }

    /// Allocate an `ioid` that is not currently live in either
    /// in-flight table. the monotonic counter alone can wrap
    /// onto an id whose operation is still pending (≈11.9 h at 100k
    /// ops/s); a late response for the stale op would then wake the
    /// wrong waiter. Probing `reads`/`writes` skips any live id. Two
    /// concurrent allocations never collide because the counter is
    /// monotonic, so the probe only guards against prior-wrap
    /// survivors.
    pub(crate) fn alloc_ioid(&self) -> u32 {
        crate::channel::alloc_nonzero_probe(&self.next_ioid, |v| {
            self.reads.contains_key(&v) || self.writes.contains_key(&v)
        })
    }

    /// Test-only: seed the next-ioid counter to drive the wrap path
    /// deterministically.
    #[cfg(test)]
    pub(crate) fn seed_next_ioid(&self, v: u32) {
        self.next_ioid.store(v, Ordering::Relaxed);
    }
}

// --- cid allocator ---

/// `cid` allocator with a live-set. Unlike `ioid`/`subid` — whose
/// owning tables are reachable where they are allocated — the cid is
/// minted in [`super::CaClient::create_channel`], which is synchronous
/// and uses the cid immediately (search schedule + lifecycle handle),
/// so it cannot be allocated inside the coordinator. This shared
/// allocator therefore owns the live-set directly: [`Self::allocate`]
/// reserves an id, skipping any cid still live after a 2^32 wrap, and
/// the coordinator calls [`Self::release`] at its single channel-
/// removal site (`CoordRequest::DropChannel`).
///
/// Invariant: the live-set mirrors the coordinator's `channels` map
/// keyed by cid — reserved at `create_channel`, released at
/// `DropChannel`. Disconnect keeps a channel's entry (it will
/// reconnect), and `DropChannel` is the one and only place a cid leaves
/// `channels`, so the two views never disagree on which cids are live.
#[derive(Clone)]
pub(crate) struct CidAllocator {
    next: Arc<AtomicU32>,
    live: Arc<DashMap<u32, ()>>,
}

impl CidAllocator {
    pub(crate) fn new() -> Self {
        Self {
            next: Arc::new(AtomicU32::new(1)),
            live: Arc::new(DashMap::new()),
        }
    }

    /// Reserve a fresh non-zero cid not currently live. The monotonic
    /// counter guarantees two concurrent allocations never receive the
    /// same value, so the probe only has to guard against a prior-wrap
    /// survivor; the subsequent insert records the reservation for
    /// future probes and for [`Self::release`].
    pub(crate) fn allocate(&self) -> u32 {
        let cid = crate::channel::alloc_nonzero_probe(&self.next, |v| self.live.contains_key(&v));
        self.live.insert(cid, ());
        cid
    }

    /// Release a cid at channel teardown (`DropChannel`). Idempotent —
    /// a missing entry (already released) is a no-op.
    pub(crate) fn release(&self, cid: u32) {
        self.live.remove(&cid);
    }

    /// Test-only view of the current live-set size.
    #[cfg(test)]
    pub(crate) fn live_len(&self) -> usize {
        self.live.len()
    }

    /// Test-only: seed the next-cid counter to drive the wrap path
    /// deterministically.
    #[cfg(test)]
    pub(crate) fn seed_next(&self, v: u32) {
        self.next.store(v, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod id_alloc_tests {
    use super::*;

    #[test]
    fn ioid_alloc_is_monotonic_and_distinct() {
        let f = InFlightOps::new();
        let a = f.alloc_ioid();
        let b = f.alloc_ioid();
        assert_ne!(a, b);
        assert_ne!(a, 0);
        assert_ne!(b, 0);
    }

    #[test]
    fn ioid_alloc_skips_live_id_on_wrap() {
        let f = InFlightOps::new();
        // Park a live read waiter at ioid 1.
        f.reads.insert(
            1,
            ReadWaiter::Warm {
                cid: 1,
                mode: ReadReplyMode::Plain,
                slot: Arc::new(parking_lot::Mutex::new(None)),
            },
        );
        // Force the counter to wrap back onto the live id.
        f.seed_next_ioid(1);
        let id = f.alloc_ioid();
        assert_ne!(
            id, 1,
            "must not reissue an ioid whose read is still in flight"
        );
        assert!(!f.reads.contains_key(&id) && !f.writes.contains_key(&id));
    }

    #[test]
    fn ioid_alloc_skips_live_write_on_wrap() {
        let f = InFlightOps::new();
        let (tx, _rx) = oneshot::channel();
        f.writes.insert(2, (7, tx));
        f.seed_next_ioid(2);
        let id = f.alloc_ioid();
        assert_ne!(
            id, 2,
            "must not reissue an ioid whose write is still in flight"
        );
    }

    #[test]
    fn cid_allocator_reserves_distinct_and_releases() {
        let a = CidAllocator::new();
        let c1 = a.allocate();
        let c2 = a.allocate();
        assert_ne!(c1, c2);
        assert_ne!(c1, 0);
        assert_eq!(a.live_len(), 2);
        a.release(c1);
        assert_eq!(a.live_len(), 1);
        // Idempotent: releasing an absent cid is a no-op.
        a.release(c1);
        assert_eq!(a.live_len(), 1);
    }

    #[test]
    fn cid_allocator_skips_live_cid_on_wrap() {
        let a = CidAllocator::new();
        let live = a.allocate();
        // Force the counter to wrap back onto the still-live cid.
        a.seed_next(live);
        let again = a.allocate();
        assert_ne!(
            again, live,
            "must not reissue a cid whose channel is still live"
        );
        assert_eq!(a.live_len(), 2);
    }

    #[test]
    fn cid_allocator_reuses_released_cid_only_after_release() {
        let a = CidAllocator::new();
        let c1 = a.allocate();
        a.release(c1);
        // After release the id is free; a wrap onto it may reuse it.
        a.seed_next(c1);
        let c2 = a.allocate();
        assert_eq!(c2, c1, "a released cid is reusable on wrap");
    }
}

// --- Search Engine messages ---

/// Why a search is being initiated — affects bucket assignment.
///
/// pvxs-style bucket scheduler dispatches new searches into a 30-bucket
/// ring. `Initial` and `BeaconAnomaly` searches go into the immediately
/// next bucket (fire within 1 tick); `Reconnect` searches are hashed by
/// cid across all buckets so a server-side event disconnecting N channels
/// doesn't materialize as one burst of N searches per tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchReason {
    /// Fresh channel creation.
    Initial,
    /// Re-search after TCP disconnect / server disconnect.
    Reconnect,
    /// Beacon anomaly detected for the server this channel was on.
    BeaconAnomaly,
}

pub(crate) enum SearchRequest {
    /// Schedule a PV for searching.
    Schedule {
        cid: u32,
        pv_name: String,
        reason: SearchReason,
    },
    /// Cancel searching for a PV (channel dropped or connected).
    Cancel { cid: u32 },
    /// Feedback from coordinator about TCP connection outcome.
    ConnectResult {
        cid: u32,
        success: bool,
        server_addr: SocketAddr,
    },
    /// Append a unicast address to the search engine's working
    /// address list. Mirrors libca
    /// `addAddrToChannelAccessAddressList` (iocinf.cpp:45). The
    /// new entry is consulted on the next scheduled search round;
    /// already-pending searches do NOT auto-restart against the
    /// new address — call [`super::CaClient::hurry_up`] (or wait
    /// for the natural retry) for that.
    AddAddress(SocketAddr),
    /// Remove a unicast address from the search engine's working
    /// address list. Used when a discovery backend reports an IOC
    /// went away (`DiscoveryEvent::Removed`). No-op if the address
    /// isn't present. Already-pending searches against the removed
    /// address run to their natural retry; only future search rounds
    /// stop targeting it.
    RemoveAddress(SocketAddr),
    /// Replace the entire working address list. Mirrors libca
    /// `configureChannelAccessAddressList` (iocinf.cpp:166). Use
    /// when the application has authoritative knowledge of the
    /// IOC topology and wants to override env-derived state at
    /// runtime.
    SetAddressList(Vec<SocketAddr>),
}

pub(crate) enum SearchResponse {
    Found {
        cid: u32,
        server_addr: SocketAddr,
    },
    /// dispatched when a second SEARCH reply for the same cid
    /// names a different server (the libca
    /// `cac.cpp::msgForMultiplyDefinedPV` condition). The coordinator
    /// fans this out to the exception handler as `ECA_DBLCHNL`,
    /// matching libca's `pvMultiplyDefinedNotify` → `this->exception`
    /// path.
    MultiplyDefined {
        pv_name: String,
        prev_addr: SocketAddr,
        new_addr: SocketAddr,
    },
}

// --- Transport Manager messages ---

pub(crate) enum TransportCommand {
    CreateChannel {
        cid: u32,
        pv_name: String,
        server_addr: SocketAddr,
        priority: u8,
    },
    ReadNotify {
        sid: u32,
        data_type: u16,
        count: u32,
        ioid: u32,
        server_addr: SocketAddr,
        priority: u8,
    },
    Write {
        sid: u32,
        data_type: u16,
        count: u32,
        payload: Vec<u8>,
        server_addr: SocketAddr,
        priority: u8,
    },
    WriteNotify {
        sid: u32,
        data_type: u16,
        count: u32,
        ioid: u32,
        payload: Vec<u8>,
        server_addr: SocketAddr,
        priority: u8,
    },
    Subscribe {
        sid: u32,
        data_type: u16,
        count: u32,
        subid: u32,
        mask: u16,
        server_addr: SocketAddr,
        priority: u8,
    },
    Unsubscribe {
        sid: u32,
        subid: u32,
        data_type: u16,
        /// Original requested element count from the EVENT_ADD that
        /// installed this subscription. C `libca/tcpiiu.cpp::
        /// subscriptionCancelRequest()` includes the subscription's
        /// stored count in the CANCEL request; we echo the same
        /// shape so strict CA dissectors / replay tooling see the
        /// libca-equivalent frame.
        count: u32,
        server_addr: SocketAddr,
        priority: u8,
    },
    ClearChannel {
        cid: u32,
        sid: u32,
        server_addr: SocketAddr,
        priority: u8,
    },
    /// Beacon arrival routed from the beacon monitor to the per-circuit
    /// receive watchdog. `anomaly = false` for healthy beacons (mirrors
    /// libca `tcpRecvWatchdog::beaconArrivalNotify` — pet the watchdog
    /// so a quiet circuit isn't probed unnecessarily). `anomaly = true`
    /// when the monitor classified the beacon as a real restart signal
    /// (`IdMismatch` / `PeriodCollapse`); the read loop only sets a
    /// flag (mirrors libca `beaconAnomalyNotify`) and lets the existing
    /// idle watchdog expire on its own schedule rather than firing an
    /// immediate echo probe — under load that immediate probe was the
    /// trigger for spurious 5-s echo timeouts and reconnect storms.
    ///
    /// a beacon is a per-server UDP signal, but the watchdog it
    /// pets lives on each circuit, so the notify fans out to every
    /// priority circuit for `server_addr` (see `process_command`).
    BeaconArrivalNotify {
        server_addr: SocketAddr,
        anomaly: bool,
    },
    EventsOff {
        server_addr: SocketAddr,
        priority: u8,
    },
    EventsOn {
        server_addr: SocketAddr,
        priority: u8,
    },
}

pub(crate) enum TransportEvent {
    ChannelCreated {
        cid: u32,
        sid: u32,
        data_type: u16,
        element_count: u32,
        access: AccessRights,
        server_addr: SocketAddr,
        /// priority of the circuit the CREATE_CH_RESP arrived
        /// on. Lets the coordinator clear a late response for an
        /// already-dropped cid on the right circuit.
        priority: u8,
    },
    MonitorData {
        subid: u32,
        data_type: u16,
        count: u32,
        data: Vec<u8>,
    },
    /// Server emitted a monitor frame with a non-NORMAL `m_cid` (ECA
    /// status), e.g. `no_read_access_event` after an ACF reload
    /// revoked read access on an active subscription. libca
    /// `cac::eventAddRespAction` (`cac.cpp:973-977`) routes this to
    /// the per-subscription `pmiu->exception` callback with the
    /// reported status — the user's monitor callback receives an
    /// Err result. Pre-fix Rust warn+dropped the frame, so an
    /// `ECA_NORDACCESS` from a C IOC was silently invisible to the
    /// subscriber.
    MonitorStatusError {
        subid: u32,
        eca_status: u32,
    },
    AccessRightsChanged {
        cid: u32,
        access: AccessRights,
    },
    ChannelCreateFailed {
        cid: u32,
    },
    ServerError {
        /// ECA status code (caerr.h) — the server's resp.cid carries
        /// this in CA_PROTO_ERROR. This is what `ca_extract_msg_no(stat)`
        /// would parse on the C side.
        eca_status: u32,
        /// Original request command that triggered the error
        /// (from the first u16 of the error payload's copy of the
        /// original header). Diagnostic only — distinct from `eca_status`.
        original_request: Option<u16>,
        message: String,
        server_addr: SocketAddr,
    },
    TcpClosed {
        server_addr: SocketAddr,
        /// which priority circuit closed. Only channels on
        /// `(server_addr, priority)` are torn down; sibling circuits to
        /// the same server at other priorities are untouched.
        priority: u8,
    },
    ServerDisconnect {
        cid: u32,
        server_addr: SocketAddr,
    },
    /// Echo timed out once — circuit may be unresponsive but TCP is still up.
    CircuitUnresponsive {
        server_addr: SocketAddr,
        priority: u8,
    },
    /// Data received after unresponsive state — circuit recovered.
    CircuitResponsive {
        server_addr: SocketAddr,
        priority: u8,
    },
    /// Server's CA minor protocol version, parsed from CA_PROTO_VERSION
    /// during TCP handshake. Mirrors libca `tcpiiu::minorProtocolVersion`
    /// (BUG_ARCHAEOLOGY d763541 / `ca_host_minor_protocol`).
    ServerVersion {
        server_addr: SocketAddr,
        priority: u8,
        minor_version: u16,
    },
    /// A fresh TCP circuit was just inserted into the connections map.
    /// Used by the coordinator to issue a `BeaconControl::ResetServer`
    /// to the beacon monitor (libca `bhe.cpp` "new client connect"
    /// EMA reset) so a stale steady-state period estimate doesn't
    /// misclassify the server's `online_notify_task` ramp-up as a
    /// `PeriodCollapse` cascade after reconnect. Emitted exactly once
    /// per circuit, before any other event for that circuit.
    ServerConnected {
        server_addr: SocketAddr,
    },
}
