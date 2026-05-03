use std::net::SocketAddr;

use crate::channel::AccessRights;

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
    ReadResponse {
        ioid: u32,
        data_type: u16,
        count: u32,
        data: Vec<u8>,
    },
    ReadError {
        ioid: u32,
        eca_status: u32,
    },
    WriteResponse {
        ioid: u32,
        status: u32,
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
