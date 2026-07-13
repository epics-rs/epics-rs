use std::fmt;
use std::net::SocketAddr;
use std::time::Instant;

use crate::channel::AccessRights;
use epics_base_rs::types::DbFieldType;

/// Channel lifecycle states
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    /// UDP search in progress
    Searching,
    /// Server found, TCP handshake + CREATE_CHAN in progress
    Connecting,
    /// Channel established, ready for read/write/subscribe
    Connected,
    /// Echo timeout, TCP still up but server may be hung (C EPICS ECA_UNRESPTMO)
    Unresponsive,
    /// Connection lost, automatic re-search triggered
    Disconnected,
    /// User dropped channel, no more reconnection
    Shutdown,
}

impl ChannelState {
    /// Whether the channel can accept read/write/subscribe operations.
    pub fn is_operational(self) -> bool {
        matches!(self, Self::Connected | Self::Unresponsive)
    }
}

impl fmt::Display for ChannelState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Searching => write!(f, "Searching"),
            Self::Connecting => write!(f, "Connecting"),
            Self::Connected => write!(f, "Connected"),
            Self::Unresponsive => write!(f, "Unresponsive"),
            Self::Disconnected => write!(f, "Disconnected"),
            Self::Shutdown => write!(f, "Shutdown"),
        }
    }
}

/// Shared inner state of a channel, owned by coordinator
pub(crate) struct ChannelInner {
    pub cid: u32,
    pub pv_name: String,
    /// CA priority (0..=99) this channel was created at. Fixed
    /// for the channel's lifetime; combined with `server_addr` it forms
    /// the [`crate::client::types::CircuitKey`] the channel rides on, so
    /// every `TransportCommand` the coordinator builds for this channel
    /// targets `(server_addr, priority)`.
    pub priority: u8,
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
    pub connect_waiters: Vec<epics_base_rs::runtime::sync::oneshot::Sender<()>>,
    /// Connection event broadcaster
    pub conn_tx: epics_base_rs::runtime::sync::broadcast::Sender<ConnectionEvent>,
    /// Consecutive short-lived disconnects (for reconnection backoff)
    pub reconnect_count: u32,
    /// When the last successful connection was established
    pub last_connected_at: Option<Instant>,
}

/// Connection state change events
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionEvent {
    Connected,
    /// The channel left the connected set. CA has exactly two connection
    /// states on the application side: `nciu::unresponsiveCircuitNotify`
    /// (`nciu.cpp:161-181`) — the echo-timeout path — calls the very same
    /// `disconnectNotify()` (`CA_OP_CONN_DOWN`) as a socket close, so an
    /// unresponsive circuit *is* a disconnect for every consumer. The
    /// distinction survives only internally, in [`ChannelState`], because
    /// it selects the recovery path (re-subscribe on the live socket vs.
    /// re-search).
    Disconnected,
    AccessRightsChanged {
        read: bool,
        write: bool,
    },
    /// epics-base `16877577` parity (PR #503-adjacent): the channel
    /// just (re)connected with a native DBR type different from the
    /// previously observed one. Records on the IOC side were
    /// redefined (e.g. `mbbi` replaced with `ai`), or the channel
    /// reconnected to a different IOC entirely. Consumers that cache
    /// per-type decoders (camonitor, archiver, archiver-appliance)
    /// should re-subscribe with the new type / re-build display
    /// metadata. `previous` is `None` for a first-time connection
    /// where the change check is vacuous.
    NativeTypeChanged {
        previous: Option<DbFieldType>,
        current: DbFieldType,
    },
}
