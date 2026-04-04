use std::net::SocketAddr;

use crate::channel::AccessRights;

// --- Search Engine messages ---

/// Why a search is being initiated — affects initial lane assignment.
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
    /// Schedule a PV for searching (replaces old Search variant).
    Schedule {
        cid: u32,
        pv_name: String,
        #[allow(dead_code)]
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
}
