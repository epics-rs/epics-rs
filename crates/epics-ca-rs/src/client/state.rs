use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use epics_base_rs::runtime::sync::broadcast;

use crate::channel::AccessRights;
use crate::client::types::ChannelSnapshots;
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
    /// Connection event broadcaster
    pub conn_tx: ConnEvents,
    /// Consecutive short-lived disconnects (for reconnection backoff)
    pub reconnect_count: u32,
    /// When the last successful connection was established
    pub last_connected_at: Option<Instant>,
}

impl ChannelInner {
    /// The only way a connection event leaves the coordinator. The snapshot
    /// `CaChannel` reads takes this channel's state and rights first, so a
    /// receiver woken by `evt`, on any thread, reads what it announces.
    pub fn announce(&mut self, snapshots: &ChannelSnapshots, evt: ConnectionEvent) {
        if self.state.is_operational() {
            if let Some(mut snap) = snapshots.get_mut(&self.cid) {
                snap.state = self.state;
                snap.access_rights = self.access_rights;
            }
        } else {
            snapshots.remove(&self.cid);
        }
        self.conn_tx.send(evt);
    }

    /// The one place a channel becomes connected to its callers — first
    /// connect and unresponsive-circuit recovery alike: rights ahead of
    /// `Connected` (`nciu.cpp:145-158`), which also wakes every
    /// `wait_connected`.
    pub fn announce_connected(&mut self, snapshots: &ChannelSnapshots) {
        let rights = ConnectionEvent::AccessRightsChanged {
            read: self.access_rights.read,
            write: self.access_rights.write,
        };
        self.announce(snapshots, rights);
        self.announce(snapshots, ConnectionEvent::Connected);
    }
}

/// A channel's connection-event sender, owned by the coordinator (its only
/// writer). Every event is folded into [`ConnFold`] before it is sent, so a
/// receiver that lagged can take the state it would hold had it lost
/// nothing — an outage it never saw included — rather than a guess from the
/// channel's current state, which cannot show an outage that is over.
pub(crate) struct ConnEvents {
    tx: broadcast::Sender<ConnectionEvent>,
    fold: Arc<AtomicU64>,
    /// `wait_connected` callers, woken by the next `Connected` sent — so,
    /// like a receiver, only after [`ChannelInner::announce`] published it.
    waiters: Vec<epics_base_rs::runtime::sync::oneshot::Sender<()>>,
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
        (
            Self {
                tx,
                fold,
                waiters: Vec::new(),
            },
            rx,
        )
    }

    pub fn view(&self) -> ConnEventsView {
        ConnEventsView {
            tx: self.tx.clone(),
            fold: self.fold.clone(),
        }
    }

    /// Wake `reply` at the next `Connected`.
    pub fn add_waiter(&mut self, reply: epics_base_rs::runtime::sync::oneshot::Sender<()>) {
        self.waiters.push(reply);
    }

    /// Fold `evt`, then send it: a receiver woken by it reads a fold that
    /// already includes it. Private: the coordinator sends through
    /// [`ChannelInner::announce`].
    fn send(&mut self, evt: ConnectionEvent) {
        let now = ConnFold::unpack(self.fold.load(Ordering::Relaxed)).after(&evt);
        self.fold.store(now.pack(), Ordering::Release);
        let connected = evt == ConnectionEvent::Connected;
        let _ = self.tx.send(evt);
        if connected {
            for waiter in self.waiters.drain(..) {
                let _ = waiter.send(());
            }
        }
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
        let (mut tx, _rx) = ConnEvents::new(4);
        let view = tx.view();
        let at = |connected, read, write, outages| ConnFold {
            connected,
            read,
            write,
            outages,
        };
        assert_eq!(view.fold(), at(false, true, true, 0));

        tx.send(ConnectionEvent::Disconnected);
        assert_eq!(view.fold().outages, 0, "never connected: no edge");

        tx.send(ConnectionEvent::AccessRightsChanged {
            read: true,
            write: false,
        });
        tx.send(ConnectionEvent::Connected);
        assert_eq!(view.fold(), at(true, true, false, 0));

        tx.send(ConnectionEvent::Disconnected);
        tx.send(ConnectionEvent::AccessRightsChanged {
            read: false,
            write: false,
        });
        tx.send(ConnectionEvent::Disconnected);
        assert_eq!(view.fold(), at(false, false, false, 1), "one edge");

        tx.send(ConnectionEvent::AccessRightsChanged {
            read: true,
            write: true,
        });
        tx.send(ConnectionEvent::Connected);
        assert_eq!(view.fold(), at(true, true, true, 1));
    }
}

#[cfg(test)]
mod announce_tests {
    use super::*;
    use crate::client::types::ChannelSnapshotPublic;
    use std::time::Duration;

    const GRANTED: AccessRights = AccessRights {
        read: true,
        write: true,
    };

    fn channel(state: ChannelState, access_rights: AccessRights) -> ChannelInner {
        ChannelInner {
            cid: 1,
            pv_name: "TEST:PV".into(),
            priority: 0,
            state,
            sid: 7,
            native_type: Some(DbFieldType::Double),
            element_count: 1,
            server_addr: Some("127.0.0.1:5064".parse().unwrap()),
            access_rights,
            conn_tx: ConnEvents::new(4).0,
            reconnect_count: 0,
            last_connected_at: None,
        }
    }

    /// What a caller on another thread could see before the snapshot was
    /// let go, and the `(state, read, write)` the snapshot then holds.
    struct Seen {
        event: Option<ConnectionEvent>,
        waiter_woke: bool,
        after: Option<(ChannelState, bool, bool)>,
    }

    /// Run `announce` on another thread while this one holds the channel's
    /// snapshot entry, which reads `held_state` with `GRANTED`, and a
    /// `wait_connected` is pending. Published first, nothing is seen early.
    fn announce_under_a_held_snapshot(
        mut ch: ChannelInner,
        held_state: ChannelState,
        announce: impl FnOnce(&mut ChannelInner, &ChannelSnapshots) + Send + 'static,
    ) -> Seen {
        let snapshots: ChannelSnapshots = Arc::new(dashmap::DashMap::new());
        snapshots.insert(
            ch.cid,
            ChannelSnapshotPublic {
                sid: ch.sid,
                native_type: DbFieldType::Double,
                element_count: 1,
                server_addr: ch.server_addr.unwrap(),
                access_rights: GRANTED,
                state: held_state,
                server_minor: 13,
            },
        );
        let cid = ch.cid;
        let mut rx = ch.conn_tx.view().subscribe();
        let (reply, mut woke) = epics_base_rs::runtime::sync::oneshot::channel();
        ch.conn_tx.add_waiter(reply);
        let held = snapshots.get(&cid).unwrap();
        let announcer = std::thread::spawn({
            let snapshots = snapshots.clone();
            move || announce(&mut ch, &snapshots)
        });
        let deadline = Instant::now() + Duration::from_millis(200);
        let (mut event, mut waiter_woke) = (None, false);
        while event.is_none() && !waiter_woke && Instant::now() < deadline {
            event = rx.try_recv().ok();
            waiter_woke = woke.try_recv().is_ok();
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(held);
        announcer.join().unwrap();
        let after = snapshots
            .get(&cid)
            .map(|s| (s.state, s.access_rights.read, s.access_rights.write));
        Seen {
            event,
            waiter_woke,
            after,
        }
    }

    /// A rights change on a connected channel: the case the gateway's
    /// watcher reads back through `CaChannel::access_rights`.
    #[test]
    fn a_rights_change_is_published_before_it_is_announced() {
        let revoked = AccessRights {
            read: true,
            write: false,
        };
        let seen = announce_under_a_held_snapshot(
            channel(ChannelState::Connected, revoked),
            ChannelState::Connected,
            |ch, snapshots| {
                ch.announce(
                    snapshots,
                    ConnectionEvent::AccessRightsChanged {
                        read: true,
                        write: false,
                    },
                )
            },
        );
        assert_eq!(
            seen.event, None,
            "announced while the old rights were readable"
        );
        assert_eq!(seen.after, Some((ChannelState::Connected, true, false)));
    }

    /// An unresponsive circuit keeps the channel's snapshot and permits.
    #[test]
    fn an_unresponsive_edge_is_published_before_it_is_announced() {
        let seen = announce_under_a_held_snapshot(
            channel(ChannelState::Unresponsive, GRANTED),
            ChannelState::Connected,
            |ch, snapshots| ch.announce(snapshots, ConnectionEvent::Disconnected),
        );
        assert_eq!(
            seen.event, None,
            "announced while the channel read connected"
        );
        assert_eq!(seen.after, Some((ChannelState::Unresponsive, true, true)));
    }

    /// A circuit that is gone takes the snapshot with it.
    #[test]
    fn a_lost_circuit_is_published_before_it_is_announced() {
        let none = AccessRights {
            read: false,
            write: false,
        };
        let seen = announce_under_a_held_snapshot(
            channel(ChannelState::Disconnected, none),
            ChannelState::Connected,
            |ch, snapshots| ch.announce(snapshots, ConnectionEvent::Disconnected),
        );
        assert_eq!(
            seen.event, None,
            "announced while the channel read connected"
        );
        assert_eq!(seen.after, None);
    }

    /// Recovery from an unresponsive circuit: a `wait_connected` caller is
    /// woken like any receiver, so it must not return while the snapshot
    /// still reads Unresponsive (and `access_rights()` none).
    #[test]
    fn recovery_is_published_before_a_waiter_wakes() {
        let seen = announce_under_a_held_snapshot(
            channel(ChannelState::Connected, GRANTED),
            ChannelState::Unresponsive,
            |ch, snapshots| ch.announce_connected(snapshots),
        );
        assert!(
            !seen.waiter_woke,
            "waiter woke while the snapshot read Unresponsive"
        );
        assert_eq!(
            seen.event, None,
            "announced while the snapshot read Unresponsive"
        );
        assert_eq!(seen.after, Some((ChannelState::Connected, true, true)));
    }
}
