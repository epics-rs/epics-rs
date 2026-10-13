use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use epics_base_rs::runtime::sync::broadcast;

use crate::channel::AccessRights;
use epics_base_rs::types::{DbFieldType, PvString};

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
    pub pv_name: PvString,
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
    pub conn_tx: ConnEvents,
    /// Consecutive short-lived disconnects (for reconnection backoff)
    pub reconnect_count: u32,
    /// When the last successful connection was established
    pub last_connected_at: Option<Instant>,
}

/// A channel's connection-event sender, owned by the coordinator (its only
/// writer). Every event is folded into [`ConnFold`] before it is sent, so a
/// receiver that lagged can take the state it would hold had it lost
/// nothing — an outage it never saw included — rather than a guess from the
/// channel's current state, which cannot show an outage that is over.
pub(crate) struct ConnEvents {
    tx: broadcast::Sender<ConnectionEvent>,
    fold: Arc<AtomicU64>,
}

/// The receiving side of [`ConnEvents`], held by every `CaChannel`.
#[derive(Clone)]
pub(crate) struct ConnEventsView {
    tx: broadcast::Sender<ConnectionEvent>,
    fold: Arc<AtomicU64>,
}

/// The fold of every event a [`ConnEvents`] has sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConnFold {
    pub connected: bool,
    /// The rights of the last `AccessRightsChanged` sent, `true`/`true`
    /// before the first. Every connect sends its rights ahead of
    /// `Connected`, so a connected fold holds that connection's rights.
    pub read: bool,
    pub write: bool,
    /// Connected → `Disconnected` edges sent.
    pub outages: u64,
}

impl ConnFold {
    const CONNECTED: u64 = 1;
    const READ: u64 = 2;
    const WRITE: u64 = 4;
    const OUTAGE: u64 = 8;

    fn unpack(v: u64) -> Self {
        Self {
            connected: v & Self::CONNECTED != 0,
            read: v & Self::READ != 0,
            write: v & Self::WRITE != 0,
            outages: v / Self::OUTAGE,
        }
    }

    fn pack(self) -> u64 {
        (self.outages * Self::OUTAGE)
            | if self.connected { Self::CONNECTED } else { 0 }
            | if self.read { Self::READ } else { 0 }
            | if self.write { Self::WRITE } else { 0 }
    }

    fn after(mut self, evt: &ConnectionEvent) -> Self {
        match *evt {
            ConnectionEvent::Connected => self.connected = true,
            ConnectionEvent::Disconnected => {
                if self.connected {
                    self.outages += 1;
                }
                self.connected = false;
            }
            ConnectionEvent::AccessRightsChanged { read, write } => {
                self.read = read;
                self.write = write;
            }
            ConnectionEvent::NativeTypeChanged { .. } => {}
        }
        self
    }
}

impl ConnEvents {
    pub fn new(capacity: usize) -> (Self, broadcast::Receiver<ConnectionEvent>) {
        let (tx, rx) = broadcast::channel(capacity);
        let fold = ConnFold {
            connected: false,
            read: true,
            write: true,
            outages: 0,
        };
        let fold = Arc::new(AtomicU64::new(fold.pack()));
        (Self { tx, fold }, rx)
    }

    pub fn view(&self) -> ConnEventsView {
        ConnEventsView {
            tx: self.tx.clone(),
            fold: self.fold.clone(),
        }
    }

    /// Fold `evt`, then send it: a receiver woken by it reads a fold that
    /// already includes it.
    pub fn send(
        &self,
        evt: ConnectionEvent,
    ) -> Result<usize, broadcast::error::SendError<ConnectionEvent>> {
        let now = ConnFold::unpack(self.fold.load(Ordering::Relaxed)).after(&evt);
        self.fold.store(now.pack(), Ordering::Release);
        self.tx.send(evt)
    }
}

impl ConnEventsView {
    pub fn subscribe(&self) -> broadcast::Receiver<ConnectionEvent> {
        self.tx.subscribe()
    }

    pub fn fold(&self) -> ConnFold {
        ConnFold::unpack(self.fold.load(Ordering::Acquire))
    }
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

#[cfg(test)]
mod conn_fold_tests {
    use super::*;

    /// One case per boundary of the fold: a disconnect counts only from
    /// connected, and the rights are the last ones sent, which for a
    /// connected fold are the ones its connect sent ahead of `Connected`.
    #[test]
    fn the_fold_counts_edges_and_connected_rights() {
        let (tx, _rx) = ConnEvents::new(4);
        let view = tx.view();
        let at = |connected, read, write, outages| ConnFold {
            connected,
            read,
            write,
            outages,
        };
        assert_eq!(view.fold(), at(false, true, true, 0));

        let _ = tx.send(ConnectionEvent::Disconnected);
        assert_eq!(view.fold().outages, 0, "never connected: no edge");

        let _ = tx.send(ConnectionEvent::AccessRightsChanged {
            read: true,
            write: false,
        });
        let _ = tx.send(ConnectionEvent::Connected);
        assert_eq!(view.fold(), at(true, true, false, 0));

        let _ = tx.send(ConnectionEvent::Disconnected);
        let _ = tx.send(ConnectionEvent::AccessRightsChanged {
            read: false,
            write: false,
        });
        let _ = tx.send(ConnectionEvent::Disconnected);
        assert_eq!(view.fold(), at(false, false, false, 1), "one edge");

        let _ = tx.send(ConnectionEvent::AccessRightsChanged {
            read: true,
            write: true,
        });
        let _ = tx.send(ConnectionEvent::Connected);
        assert_eq!(view.fold(), at(true, true, true, 1));
    }
}
