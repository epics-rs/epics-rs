use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::sync::oneshot;
use epics_base_rs::types::DbFieldType;

use crate::channel::AccessRights;
use crate::client::state::ChannelState;

// --- Per-server last-RX timestamp sidecar (Option C, Phase D) ---

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
pub(crate) type ServerLastRxAt = Arc<DashMap<SocketAddr, Instant>>;

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

// --- Direct in-flight op registries (Option C) ---

/// Reply channel type for one-shot reads. Carries (data_type, count, payload).
pub(crate) type ReadReplyTx = oneshot::Sender<CaResult<(u16, u32, Vec<u8>)>>;
/// Reply channel type for one-shot writes (write-notify completion).
pub(crate) type WriteReplyTx = oneshot::Sender<CaResult<()>>;

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
#[derive(Clone, Default)]
pub(crate) struct InFlightOps {
    pub(crate) reads: Arc<DashMap<u32, (u32, ReadReplyTx)>>,
    pub(crate) writes: Arc<DashMap<u32, (u32, WriteReplyTx)>>,
}

impl InFlightOps {
    pub(crate) fn new() -> Self {
        Self::default()
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
    /// Replace the entire working address list. Mirrors libca
    /// `configureChannelAccessAddressList` (iocinf.cpp:166). Use
    /// when the application has authoritative knowledge of the
    /// IOC topology and wants to override env-derived state at
    /// runtime.
    SetAddressList(Vec<SocketAddr>),
}

pub(crate) enum SearchResponse {
    Found { cid: u32, server_addr: SocketAddr },
}

// --- Transport Manager messages ---

pub(crate) enum TransportCommand {
    CreateChannel {
        cid: u32,
        pv_name: String,
        server_addr: SocketAddr,
    },
    ReadNotify {
        sid: u32,
        data_type: u16,
        count: u32,
        ioid: u32,
        server_addr: SocketAddr,
    },
    Write {
        sid: u32,
        data_type: u16,
        count: u32,
        payload: Vec<u8>,
        server_addr: SocketAddr,
    },
    WriteNotify {
        sid: u32,
        data_type: u16,
        count: u32,
        ioid: u32,
        payload: Vec<u8>,
        server_addr: SocketAddr,
    },
    Subscribe {
        sid: u32,
        data_type: u16,
        count: u32,
        subid: u32,
        mask: u16,
        server_addr: SocketAddr,
    },
    Unsubscribe {
        sid: u32,
        subid: u32,
        data_type: u16,
        server_addr: SocketAddr,
    },
    ClearChannel {
        cid: u32,
        sid: u32,
        server_addr: SocketAddr,
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
    BeaconArrivalNotify {
        server_addr: SocketAddr,
        anomaly: bool,
    },
    EventsOff {
        server_addr: SocketAddr,
    },
    EventsOn {
        server_addr: SocketAddr,
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
    },
    MonitorData {
        subid: u32,
        data_type: u16,
        count: u32,
        data: Vec<u8>,
    },
    AccessRightsChanged {
        cid: u32,
        access: AccessRights,
    },
    ChannelCreateFailed {
        cid: u32,
    },
    ServerError {
        _original_request: Option<u16>,
        _message: String,
    },
    TcpClosed {
        server_addr: SocketAddr,
    },
    ServerDisconnect {
        cid: u32,
        server_addr: SocketAddr,
    },
    /// Echo timed out once — circuit may be unresponsive but TCP is still up.
    CircuitUnresponsive {
        server_addr: SocketAddr,
    },
    /// Data received after unresponsive state — circuit recovered.
    CircuitResponsive {
        server_addr: SocketAddr,
    },
    /// Server's CA minor protocol version, parsed from CA_PROTO_VERSION
    /// during TCP handshake. Mirrors libca `tcpiiu::minorProtocolVersion`
    /// (BUG_ARCHAEOLOGY d763541 / `ca_host_minor_protocol`).
    ServerVersion {
        server_addr: SocketAddr,
        minor_version: u16,
    },
}
