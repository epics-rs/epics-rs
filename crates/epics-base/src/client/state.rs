use std::fmt;
use std::net::SocketAddr;

use crate::channel::AccessRights;
use crate::types::DbFieldType;

/// Channel lifecycle states
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    /// UDP search in progress
    Searching,
    /// Server found, TCP handshake + CREATE_CHAN in progress
    Connecting,
    /// Channel established, ready for read/write/subscribe
    Connected,
    /// Connection lost, automatic re-search triggered
    Disconnected,
    /// User dropped channel, no more reconnection
    Shutdown,
}

impl fmt::Display for ChannelState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Searching => write!(f, "Searching"),
            Self::Connecting => write!(f, "Connecting"),
            Self::Connected => write!(f, "Connected"),
            Self::Disconnected => write!(f, "Disconnected"),
            Self::Shutdown => write!(f, "Shutdown"),
        }
    }
}

/// Shared inner state of a channel, owned by coordinator
pub(crate) struct ChannelInner {
    pub cid: u32,
    pub pv_name: String,
    pub state: ChannelState,
    /// Server-assigned SID (valid only when Connected)
    pub sid: u32,
    /// Native DBR type (valid only when Connected)
    pub native_type: Option<DbFieldType>,
    /// Element count (valid only when Connected)
    pub element_count: u32,
    /// Server address (known after search)
    pub server_addr: Option<SocketAddr>,
    /// Access rights
    pub access_rights: AccessRights,
    /// Waiters for connection (oneshot senders)
    pub connect_waiters: Vec<crate::runtime::sync::oneshot::Sender<()>>,
    /// Connection event broadcaster
    pub conn_tx: crate::runtime::sync::broadcast::Sender<ConnectionEvent>,
}

/// Connection state change events
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionEvent {
    Connected,
    Disconnected,
}
