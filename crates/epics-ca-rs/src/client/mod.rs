// The beacon monitor is a UDP listener on the CA beacon port, fed by the
// repeater. `client-core` has no UDP client socket at all (the target's
// search reaches servers over `EPICS_CA_NAME_SERVERS`, §4.1), so there are
// no beacons to monitor and the module is not compiled. What it buys — a
// faster rescan after a server restart than the search retry cadence alone —
// is an optimisation over a transport that does not exist there.
//
// `ca_beacon_monitor` is `feature = "client"` AND `tokio_backend`, emitted as
// one name by `build.rs` because it is one fact — "this build has a UDP beacon
// listener" — and the fifteen sites below would otherwise each restate a
// two-term conjunction. The second term is not redundant: the monitor's socket
// (`AsyncUdpV4::bind_ephemeral_same_port`) and its repeater registration
// (`repeater::try_register`, a `tokio::net::UdpSocket`) both need a reactor,
// and `run_beacon_monitor` is started through `runtime::task::spawn`, which on
// `exec_backend` lands it on a callback-pool worker that has none. A hosted
// `--features rtems-exec-model` build had `feature = "client"` on and panicked
// on both of those sockets at `realtime-ca-ioc` boot, measured — two of the three
// panics in `doc/calink-rtems-design.md` §10.10 item 2.
// RTEMS-EXEC-MODEL-ALLOW(11): the flavored tests drive the full CA client over
// tokio::net or pin a specific tokio runtime flavor. These run and pass in the
// feature-ON suite on the tokio driver.
#[cfg(ca_beacon_monitor)]
mod beacon_monitor;
mod circuit_breaker;
mod search;
mod state;
mod subscription;
mod sync_group;
mod transport;
mod types;

pub use sync_group::{SyncGroup, SyncGroupResults, SyncGroupStat, SyncGroupStatus};

/// BRING-UP PROBE: the CA dial pool's `(workers, attempts)`, for
/// `realtime-ca-ioc`'s console reporter. `transport` is private, and a bin target
/// is a separate crate, so the rig's one accessor is re-exported here rather
/// than the module being opened up for it.
#[cfg(all(feature = "bringup-probes", any(exec_backend, ca_blocking_client)))]
pub use transport::dial_pool_probe;

pub use circuit_breaker::{BreakerConfig, BreakerState};

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use std::time::Duration;

use epics_base_rs::runtime::sync::{broadcast, mpsc, oneshot};
use parking_lot::Mutex;

// `channel` is `pub(crate)`, but `CaChannel::info` and `CaClient::cainfo`
// hand both of these back across the crate boundary. Re-exported here, beside
// the methods that return them, so a caller can name what they were given.
pub use crate::channel::{AccessRights, ChannelInfo};
use crate::protocol::*;
#[cfg(ca_beacon_monitor)]
use crate::repeater;
use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::snapshot::{DbrClass, Snapshot};
use epics_base_rs::types::{
    DBR_STRING, DBR_TIME_INT, DBR_TIME_STRING, DbFieldType, EpicsValue, PvString, decode_dbr,
};

pub use state::{ChannelState, ConnectionEvent};

/// DBR type code to request for an ENUM field's *default* readback,
/// mirroring the C CLI tools (`caget.c:177-182`, `camonitor.c:156-160`,
/// `caput.c:147-151`): with `enum_as_string` set (the tool default for
/// ENUM fields, off under `-n` and for the native library API), an ENUM
/// field is read back in its STRING form so the value renders as the
/// state label rather than the numeric index. `want_time` selects the
/// TIME-class string (`DBR_TIME_STRING`, carrying timestamp + alarm, for
/// `caget -a` / `camonitor` / `caput -l`) over the plain string
/// (`DBR_STRING`, value only).
///
/// Returns `None` for non-ENUM fields and when `enum_as_string` is unset
/// — the caller then keeps its native-type path (the plain library
/// subscribe, and any field that already renders meaningfully).
pub fn enum_string_readback_dbr(
    native: DbFieldType,
    want_time: bool,
    enum_as_string: bool,
) -> Option<u16> {
    if enum_as_string && matches!(native, DbFieldType::Enum) {
        Some(if want_time {
            DBR_TIME_STRING
        } else {
            DBR_STRING
        })
    } else {
        None
    }
}

/// Readback DBR type for a `caget -s` / `camonitor -s` (`floatAsString`)
/// request on a native FLOAT/DOUBLE field.
///
/// C `caget`/`camonitor` set `dbrType = DBR_TIME_STRING` when `floatAsString`
/// is set and the native type is FLOAT or DOUBLE (`caget.c:183-187`,
/// `camonitor.c:162-166`), so the SERVER performs the float→string
/// conversion — honoring record/server precision — instead of the client
/// formatting a numeric value locally. C always selects the TIME-class
/// string here (it starts from `dbf_type_to_DBR_TIME`), so this returns
/// `DBR_TIME_STRING` regardless of output mode; a plain readback simply
/// ignores the carried timestamp/alarm.
///
/// Returns `None` for every non-FLOAT/DOUBLE field (ENUM is handled by
/// [`enum_string_readback_dbr`], which takes precedence per C's `if/else if`).
pub fn float_as_string_readback_dbr(native: DbFieldType) -> Option<u16> {
    matches!(native, DbFieldType::Float | DbFieldType::Double).then_some(DBR_TIME_STRING)
}

/// ENUM readback DBR type for the `caget`/`camonitor` GET/monitor path.
///
/// C `caget.c:174-181` and `camonitor.c:155-162` start from
/// `dbf_type_to_DBR_TIME(dbfType)` and then, for an ENUM field, ALWAYS
/// replace the type — they never read an ENUM back as native
/// `DBR_TIME_ENUM`: `enumAsNr` (`-n`) selects `DBR_TIME_INT` (the numeric
/// index), otherwise `DBR_TIME_STRING` (the state label). Both branches
/// stay in the TIME class, so the request type is identical across plain,
/// terse, and wide output (the value-only modes simply ignore the carried
/// timestamp/alarm). Returns `None` for non-ENUM fields so the caller keeps
/// its native path.
///
/// Distinct from [`enum_string_readback_dbr`], which models only the
/// label-vs-native decision shared with `caput` and deliberately returns
/// `None` for the numeric case (caput keeps a native numeric readback
/// there). This owner is the caget/camonitor chain, where `-n` maps to an
/// integer DBR instead of falling through to native ENUM.
pub fn enum_cli_readback_dbr(native: DbFieldType, enum_as_number: bool) -> Option<u16> {
    matches!(native, DbFieldType::Enum).then(|| {
        if enum_as_number {
            DBR_TIME_INT
        } else {
            DBR_TIME_STRING
        }
    })
}

/// How a subscription requests an ENUM field's readback type.
///
/// Models the three mutually-exclusive ENUM readback modes as one sum type
/// instead of an `enum_as_string: bool` that meant different things on
/// different paths. The bool collapsed three intents into two values: the
/// plain library/gateway subscribe and `camonitor -n` BOTH passed `false`,
/// yet the former must keep the native ENUM index while the latter must
/// request `DBR_TIME_INT`. Disambiguating them by a runtime flag was the
/// dual-meaning defect; a sum type
/// makes the three modes explicit at every call site and the illegal
/// "native-vs-numeric on the same `false`" combination unrepresentable:
///
/// - `Native`  — keep the field's native TIME type. ENUM stays
///   `DBR_TIME_ENUM` (numeric index + label set); the plain
///   `subscribe_with_mask` and the CA gateway/pvalink rely on this (no
///   silent behaviour change for existing library consumers).
/// - `Label`   — `camonitor` default: an ENUM is requested as
///   `DBR_TIME_STRING` so values arrive as state labels
///   (C `camonitor.c:156-160`).
/// - `Numeric` — `camonitor -n`: an ENUM is requested as `DBR_TIME_INT`
///   so values arrive as the numeric index
///   (C `camonitor.c:158` `if (enumAsNr) ppv->dbrType = DBR_TIME_INT`).
///
/// `Label`/`Numeric` only ever substitute ENUM fields; for a non-ENUM field
/// all three modes fall through to the float/native chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnumReadback {
    /// Native TIME type (ENUM stays `DBR_TIME_ENUM`): the library/gateway
    /// subscribe default.
    Native,
    /// `camonitor` default — ENUM as `DBR_TIME_STRING` (state label).
    Label,
    /// `camonitor -n` — ENUM as `DBR_TIME_INT` (numeric index).
    Numeric,
}

impl EnumReadback {
    /// The ENUM substitution DBR type for this mode, or `None` when the
    /// caller should keep its native path (mode `Native`, or any non-ENUM
    /// field). `Label`/`Numeric` reuse [`enum_cli_readback_dbr`] — the same
    /// owner the `caget`/`camonitor` GET path uses — so the monitor and GET
    /// ENUM substitutions cannot drift.
    fn enum_substitution(self, native: DbFieldType) -> Option<u16> {
        match self {
            EnumReadback::Native => None,
            EnumReadback::Label => enum_cli_readback_dbr(native, false),
            EnumReadback::Numeric => enum_cli_readback_dbr(native, true),
        }
    }
}

/// TIME-class readback DBR type for a *subscription*, applying the C CLI
/// substitution chain in source order (`camonitor.c:155-166`): an ENUM
/// field is substituted per [`EnumReadback`] (`Label` → `DBR_TIME_STRING`,
/// `Numeric` → `DBR_TIME_INT`, `Native` → keep native); otherwise a
/// FLOAT/DOUBLE under `-s` becomes `DBR_TIME_STRING` via
/// [`float_as_string_readback_dbr`]; otherwise the field's native TIME-class
/// type. C tests the ENUM case first and the float case in the `else if`, so
/// the ENUM substitution takes precedence — preserved here by
/// `enum_substitution(..).or_else(..)`.
///
/// Single owner of the subscribe-time derivation so the initial
/// `Subscribe` and the `NativeTypeChanged` restore stay in agreement.
pub fn subscription_readback_dbr(
    native: DbFieldType,
    enum_readback: EnumReadback,
    float_as_string: bool,
) -> u16 {
    enum_readback
        .enum_substitution(native)
        .or_else(|| {
            float_as_string
                .then(|| float_as_string_readback_dbr(native))
                .flatten()
        })
        .unwrap_or_else(|| native.time_dbr_type())
}

use state::ChannelInner;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Type of diagnostic event recorded in the event history.
#[derive(Debug, Clone)]
pub enum DiagEvent {
    Connected {
        pv: String,
        server: SocketAddr,
    },
    Disconnected {
        server: SocketAddr,
        channels: usize,
    },
    Reconnected {
        pv: String,
        restored: u32,
        stale: u32,
    },
    Unresponsive {
        server: SocketAddr,
    },
    Responsive {
        server: SocketAddr,
    },
    BeaconAnomaly {
        server: SocketAddr,
    },
}

impl std::fmt::Display for DiagEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connected { pv, server } => write!(f, "Connected {pv} @ {server}"),
            Self::Disconnected { server, channels } => {
                write!(f, "Disconnected {server} ({channels} channels)")
            }
            Self::Reconnected {
                pv,
                restored,
                stale,
            } => write!(f, "Reconnected {pv} (restored={restored}, stale={stale})"),
            Self::Unresponsive { server } => write!(f, "Unresponsive {server}"),
            Self::Responsive { server } => write!(f, "Responsive {server}"),
            Self::BeaconAnomaly { server } => write!(f, "Beacon anomaly {server}"),
        }
    }
}

/// Timestamped diagnostic event.
#[derive(Debug, Clone)]
pub struct DiagRecord {
    pub time: std::time::Instant,
    pub event: DiagEvent,
}

const EVENT_HISTORY_CAPACITY: usize = 256;
const ONE_SHOT_CHANNEL_CACHE_CAPACITY: usize = 4096;

#[derive(Default)]
struct OneShotChannelCache {
    channels: HashMap<String, CaChannel>,
    order: VecDeque<String>,
}

impl OneShotChannelCache {
    fn get_or_create(&mut self, client: &CaClient, pv_name: String) -> CaChannel {
        if let Some(channel) = self.channels.get(&pv_name) {
            return channel.clone();
        }

        // Legacy one-shot API has no priority knob — default (0).
        let channel = client.create_channel_expanded(pv_name.clone(), 0);
        self.channels.insert(pv_name.clone(), channel.clone());
        self.order.push_back(pv_name);

        while self.channels.len() > ONE_SHOT_CHANNEL_CACHE_CAPACITY {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.channels.remove(&oldest);
        }

        channel
    }
}

/// Diagnostic counters for CA client health monitoring.
pub struct CaDiagnostics {
    pub connections: AtomicU64,
    pub disconnections: AtomicU64,
    pub reconnections: AtomicU64,
    pub unresponsive_events: AtomicU64,
    pub subscriptions_restored: AtomicU64,
    pub subscriptions_stale: AtomicU64,
    pub beacon_anomalies: AtomicU64,
    pub search_requests: AtomicU64,
    /// Monitor updates dropped because the application's queue was full.
    /// Slow consumers should bump up EPICS_CA_MONITOR_QUEUE or call
    /// recv() more often.
    pub dropped_monitors: AtomicU64,
    /// Ring buffer of recent events for post-mortem analysis.
    history: std::sync::Mutex<Vec<DiagRecord>>,
}

impl Default for CaDiagnostics {
    fn default() -> Self {
        Self {
            connections: AtomicU64::new(0),
            disconnections: AtomicU64::new(0),
            reconnections: AtomicU64::new(0),
            unresponsive_events: AtomicU64::new(0),
            subscriptions_restored: AtomicU64::new(0),
            subscriptions_stale: AtomicU64::new(0),
            beacon_anomalies: AtomicU64::new(0),
            search_requests: AtomicU64::new(0),
            dropped_monitors: AtomicU64::new(0),
            history: std::sync::Mutex::new(Vec::with_capacity(EVENT_HISTORY_CAPACITY)),
        }
    }
}

impl CaDiagnostics {
    /// Record a diagnostic event with the current timestamp.
    pub fn record(&self, event: DiagEvent) {
        let record = DiagRecord {
            time: std::time::Instant::now(),
            event,
        };
        if let Ok(mut history) = self.history.lock() {
            if history.len() >= EVENT_HISTORY_CAPACITY {
                history.remove(0);
            }
            history.push(record);
        }
    }

    /// Get a snapshot of counters + recent event history.
    pub fn snapshot(&self) -> DiagnosticsSnapshot {
        let history = self.history.lock().map(|h| h.clone()).unwrap_or_default();
        DiagnosticsSnapshot {
            connections: self.connections.load(Ordering::Relaxed),
            disconnections: self.disconnections.load(Ordering::Relaxed),
            reconnections: self.reconnections.load(Ordering::Relaxed),
            unresponsive_events: self.unresponsive_events.load(Ordering::Relaxed),
            subscriptions_restored: self.subscriptions_restored.load(Ordering::Relaxed),
            subscriptions_stale: self.subscriptions_stale.load(Ordering::Relaxed),
            beacon_anomalies: self.beacon_anomalies.load(Ordering::Relaxed),
            search_requests: self.search_requests.load(Ordering::Relaxed),
            dropped_monitors: self.dropped_monitors.load(Ordering::Relaxed),
            history,
        }
    }
}

/// Point-in-time snapshot of diagnostic counters + event history.
#[derive(Debug, Clone)]
pub struct DiagnosticsSnapshot {
    pub connections: u64,
    pub disconnections: u64,
    pub reconnections: u64,
    pub unresponsive_events: u64,
    pub subscriptions_restored: u64,
    pub subscriptions_stale: u64,
    pub beacon_anomalies: u64,
    pub search_requests: u64,
    pub dropped_monitors: u64,
    pub history: Vec<DiagRecord>,
}

impl std::fmt::Display for DiagnosticsSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Connections:            {}", self.connections)?;
        writeln!(f, "Disconnections:         {}", self.disconnections)?;
        writeln!(f, "Reconnections:          {}", self.reconnections)?;
        writeln!(f, "Unresponsive events:    {}", self.unresponsive_events)?;
        writeln!(f, "Subscriptions restored: {}", self.subscriptions_restored)?;
        writeln!(f, "Subscriptions stale:    {}", self.subscriptions_stale)?;
        writeln!(f, "Beacon anomalies:       {}", self.beacon_anomalies)?;
        writeln!(f, "Search requests:        {}", self.search_requests)?;
        writeln!(f, "Dropped monitors:       {}", self.dropped_monitors)?;
        if !self.history.is_empty() {
            writeln!(f, "Recent events ({}):", self.history.len())?;
            let start = self
                .history
                .first()
                .map(|r| r.time)
                .unwrap_or_else(std::time::Instant::now);
            for rec in &self.history {
                let elapsed = rec.time.duration_since(start);
                writeln!(f, "  +{:.1}s  {}", elapsed.as_secs_f64(), rec.event)?;
            }
        }
        Ok(())
    }
}
use subscription::SubscriptionRegistry;
use types::*;

// Public re-exports for the CA-130 exception-handler API. Mirror the
// pattern already used for DiagnosticsSnapshot.
pub use types::{CaException, CaExceptionHandler, CaExceptionKind, CaOp, ExceptionSite};

/// CA client with persistent channels and auto-reconnection.
pub struct CaClient {
    search_tx: mpsc::UnboundedSender<SearchRequest>,
    transport_tx: mpsc::UnboundedSender<TransportCommand>,
    coord_tx: mpsc::UnboundedSender<CoordRequest>,
    /// Bounded cache used by by-name one-shot bulk reads.
    ///
    /// `create_channel` remains uncached so persistent user channels
    /// keep independent lifecycle/subscription state. This cache only
    /// prevents repeated `caget_many([...])` calls from paying
    /// create/search/connect on every invocation.
    one_shot_channels: Mutex<OneShotChannelCache>,
    /// Shared registry of in-flight one-shot reads and writes
    /// (Option C, Phase A). Channel handles insert reply oneshots
    /// here directly; the per-server read loop removes and fulfils
    /// them on response arrival without a coordinator round-trip.
    in_flight: InFlightOps,
    /// cid allocator with a live-set. `create_channel`
    /// reserves; the coordinator releases at `DropChannel`. Shared
    /// (cloned) into the coordinator task so both sides draw from /
    /// retire to one id space.
    cid_alloc: CidAllocator,
    /// Per-channel snapshot sidecar (Option C, Phase B). Coordinator
    /// publishes channel state on every lifecycle change; CaChannel
    /// hot paths read directly without a `GetChannelInfo` round-trip.
    snapshots: ChannelSnapshots,
    /// Per-server writer sidecar (Option C, Phase E). Lets hot reads
    /// and writes enqueue frames directly to the connection writer
    /// once lifecycle has already established the virtual circuit.
    server_writers: DirectServerWriters,
    diagnostics: Arc<CaDiagnostics>,
    /// Per-channel SEARCH attempt counter — see CA-035
    /// `ca_search_attempts`. Counts every fanout call (immediate
    /// first SEARCH AND each bucket-tick retransmit). Search engine
    /// writes; CaChannel reads via [`CaChannel::search_attempts`].
    search_attempts: types::SearchAttempts,
    /// Per-client exception handler slot (CA-130
    /// `ca_add_exception_event`). Scope is this CaClient instance —
    /// each client owns its own slot. Internal sites call
    /// [`types::dispatch_exception`] for OOB / unrecoverable errors;
    /// callers register via [`CaClient::set_exception_handler`].
    exception_slot: types::CaExceptionSlot,
    /// Runtime-mutable client identity (user + host) advertised on every
    /// CA virtual-circuit handshake. Cloned into the transport manager so
    /// new circuits handshake with the current value;
    /// [`CaClient::set_user_name`] / [`CaClient::set_host_name`] mutate it
    /// and notify already-connected circuits out of band.
    client_identity: types::ClientIdentitySlot,
    // The spawned client background tasks. Typed as the `runtime::task`
    // seam handle (not a bare `tokio::JoinHandle`) because they are spawned
    // through `runtime::task::spawn`; under the hosted default this alias IS
    // `tokio::task::JoinHandle`, and under the `rtems-exec-model` backend it
    // is the executor's `JoinFuture`, so the field type follows the seam.
    _coordinator: epics_base_rs::runtime::task::TaskHandle<()>,
    _search_task: epics_base_rs::runtime::task::TaskHandle<()>,
    _transport_task: epics_base_rs::runtime::task::TaskHandle<()>,
    #[cfg(ca_beacon_monitor)]
    _beacon_task: epics_base_rs::runtime::task::TaskHandle<()>,
    /// Discovery backends retained for their lifetime. mDNS's
    /// `MdnsBackend` owns an `mdns-sd` `ServiceDaemon` whose browse
    /// runs only while the backend is alive — dropping it kills live
    /// discovery. Held here so post-startup IOCs keep being found.
    #[cfg(feature = "client")]
    _discovery_backends: Vec<Box<dyn crate::discovery::Backend>>,
    /// Per-backend forwarder tasks: drain each backend's
    /// `subscribe()` stream and feed `DiscoveryEvent`s into the
    /// search engine as `AddAddress` / `RemoveAddress`. Aborted on
    /// drop so they don't outlive the client.
    #[cfg(feature = "client")]
    _discovery_forwarders: Vec<epics_base_rs::runtime::task::TaskHandle<()>>,
}

/// Internal coordinator requests from CaChannel / public API
#[allow(dead_code)]
enum CoordRequest {
    RegisterChannel {
        cid: u32,
        pv_name: String,
        /// CA priority the channel was created at; stored on
        /// the coordinator's `ChannelInner` and threaded into every
        /// `TransportCommand` the coordinator builds for this channel.
        priority: u8,
        conn_tx: broadcast::Sender<ConnectionEvent>,
    },
    WaitConnected {
        cid: u32,
        reply: oneshot::Sender<()>,
    },
    Subscribe {
        cid: u32,
        mask: u16,
        deadband: f64,
        callback_tx: mpsc::Sender<CaResult<Snapshot>>,
        coalesce_slot: std::sync::Arc<subscription::CoalesceSlot>,
        /// Requested element-count cap (`camonitor -#`), or `None` for the
        /// full native count. Carried onto the subscription record and
        /// clamped to the channel's native element count at connect-time by
        /// the single owner [`subscription::resolve_subscription_count`]
        /// (C `camonitor.c:169`), so it works even though the subscribe is
        /// issued before the channel connects.
        req_count: Option<u32>,
        /// Which ENUM readback type the subscription requests: `Native`
        /// (keep `DBR_TIME_ENUM` — the plain library/gateway subscribe),
        /// `Label` (the `camonitor` default, `DBR_TIME_STRING`), or
        /// `Numeric` (`camonitor -n`, `DBR_TIME_INT`). See [`EnumReadback`].
        /// Applied by the coordinator at connect-time type derivation, so it
        /// works even though the subscribe is issued before the channel
        /// connects.
        enum_readback: EnumReadback,
        /// When set, a FLOAT/DOUBLE field is subscribed in its
        /// `DBR_TIME_STRING` form so the server renders the value to a
        /// string at record precision (`camonitor -s` / `floatAsString`;
        /// C `camonitor.c:162-166`). Folded into the readback derivation
        /// after the ENUM case, mirroring C's `else if`.
        float_as_string: bool,
        /// the coordinator owns the `subid` table, so it
        /// allocates the id (probing live subscriptions) and returns
        /// it here rather than accepting one minted at the call site.
        reply: oneshot::Sender<CaResult<u32>>,
    },
    Unsubscribe {
        subid: u32,
    },
    DropChannel {
        cid: u32,
    },
    /// Beacon anomaly classified by `beacon_monitor`. Coordinator
    /// rescans all disconnected/searching channels regardless of
    /// `kind`. The transport watchdog is updated through the
    /// separate `BeaconArrival` path — this variant intentionally
    /// no longer carries an EchoProbe-style side effect, mirroring
    /// libca's split between `udpiiu::beaconAnomalyNotify` (which
    /// only wakes searches) and `tcpRecvWatchdog::beaconAnomalyNotify`
    /// (which only flips a per-circuit flag).
    #[cfg(ca_beacon_monitor)]
    ForceRescanServer {
        server_addr: SocketAddr,
        kind: beacon_monitor::BeaconAnomalyKind,
    },
    /// Beacon arrival notification for the per-circuit receive
    /// watchdog. `anomaly = false` for healthy beacons (libca
    /// `beaconArrivalNotify` — refresh watchdog deadline);
    /// `anomaly = true` for `IdMismatch` and for the two libca period
    /// bands (`LongPeriod` / `ShortPeriod`, `bhe.cpp:226-262`) — libca
    /// `beaconAnomalyNotify`: set sticky flag, no immediate echo.
    /// `FirstSighting` deliberately does NOT generate this message:
    /// either we don't yet have a virtual circuit to the server, in
    /// which case the watchdog is irrelevant, or we do (we just
    /// pruned the BHE) and the next healthy beacon will refresh it
    /// naturally.
    #[cfg(ca_beacon_monitor)]
    BeaconArrival {
        server_addr: SocketAddr,
        anomaly: bool,
    },
    /// Time since this channel's circuit last received any frame.
    /// `None` if the channel isn't operational. Mirrors libca
    /// `ca_receive_watchdog_delay`.
    GetWatchdogDelay {
        cid: u32,
        reply: oneshot::Sender<Option<Duration>>,
    },
    /// Number of distinct operational virtual circuits — keyed by
    /// `(address, CA priority)` like libca's circuit table, so two
    /// priorities to one IOC count as two circuits. Mirrors libca
    /// `ca_get_ioc_connection_count`.
    GetIocConnectionCount {
        reply: oneshot::Sender<usize>,
    },
    /// Server's CA minor protocol version for the channel's circuit.
    /// `None` if disconnected or no VERSION reply yet. Mirrors libca
    /// `ca_host_minor_protocol` (BUG_ARCHAEOLOGY d763541).
    GetHostMinorProtocol {
        cid: u32,
        reply: oneshot::Sender<Option<u16>>,
    },
    /// Graceful shutdown: clear all channels on their servers before exiting.
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// Optional configuration knobs for `CaClient::new_with_config`.
/// All fields default to the same behaviour as the no-arg
/// `CaClient::new()`, so callers only need to set what they want
/// to override.
#[derive(Default)]
pub struct CaClientConfig {
    /// Enable CA-over-TLS. When `Some`, every TCP virtual circuit
    /// negotiated by the transport manager is wrapped in a
    /// `tokio_rustls::TlsStream` before the CA handshake. UDP search
    /// remains plaintext. Requires the `tls` cargo feature.
    #[cfg(feature = "experimental-rust-tls")]
    pub tls: Option<crate::tls::TlsConfig>,

    /// Override SNI / cert-hostname-verification name for TLS
    /// connections. When `None`, the client falls back to the server's
    /// IP address (which only works for IP-bound certs / wildcard
    /// fallbacks). Set this to the DNS name embedded in the server
    /// certificate when verifying hostname-bound certs. Picked up
    /// from `EPICS_CA_TLS_SERVER_NAME` by default.
    #[cfg(feature = "experimental-rust-tls")]
    pub tls_server_name: Option<String>,

    /// Service-discovery configuration. When `Some`, the client
    /// merges the addresses returned by every active backend into
    /// its `EPICS_CA_ADDR_LIST` at startup. Falls back to the
    /// `EPICS_CA_DISCOVERY` env var when `None`.
    #[cfg(feature = "client")]
    pub discovery: Option<crate::discovery::DiscoveryConfig>,

    /// Pre-built discovery backends to consult in addition to whatever
    /// `discovery` resolves to. Lets applications plug in custom
    /// `Backend` implementations (HTTP API, Consul, etcd, site CMDB,
    /// ...) without having to extend `DiscoveryConfig`. Each backend's
    /// `discover()` is called once at startup; addresses are deduped
    /// against `EPICS_CA_ADDR_LIST` and `discovery`-provided sources.
    #[cfg(feature = "client")]
    pub extra_backends: Vec<Box<dyn crate::discovery::Backend>>,
}

impl CaClient {
    pub async fn new() -> CaResult<Self> {
        // Default constructor: pick up TLS config from environment when
        // `EPICS_CA_TLS_ROOTS_FILE` etc. are set, otherwise plaintext.
        // `new_with_config(CaClientConfig::default())` skips this.
        #[cfg(feature = "experimental-rust-tls")]
        let cfg = {
            let mut c = CaClientConfig::default();
            match crate::tls::client_from_env() {
                Ok(Some(tls)) => c.tls = Some(tls),
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(error = %e,
                        "EPICS_CA_TLS_* configuration is invalid; using plaintext");
                }
            }
            // Pick up an explicit SNI / cert-hostname-verification name
            // for hostname-bound certs. Without this, the SNI falls back
            // to the server's IP literal — which only validates against
            // IP-bound certs.
            c.tls_server_name = epics_base_rs::runtime::env::get("EPICS_CA_TLS_SERVER_NAME");
            c
        };
        #[cfg(not(feature = "experimental-rust-tls"))]
        let cfg = CaClientConfig::default();
        Self::new_with_config(cfg).await
    }

    /// Create a client with explicit configuration. Currently the only
    /// knob is `tls`; future fields will follow the same pattern.
    pub async fn new_with_config(config: CaClientConfig) -> CaResult<Self> {
        // Every field of this struct belongs to a feature (`client`'s
        // discovery pair, `experimental-rust-tls`'s two), so in the
        // `client-core` build with TLS off it has none left. Destructured
        // rather than ignored: a field added unconditionally later fails to
        // compile here until someone states which configuration reads it.
        #[cfg(not(any(feature = "client", feature = "experimental-rust-tls")))]
        let CaClientConfig {} = config;
        #[cfg(feature = "experimental-rust-tls")]
        if config.tls.is_some() {
            tracing::warn!(
                "═══════════════════════════════════════════════════════════════════════\n  \
                 CA client TLS ENABLED — non-standard, Rust-only extension.\n  \
                 Cannot connect to C softIoc, EDM, MEDM, CSS, or pyepics-based tools.\n  \
                 See doc/11-tls-design.md for rationale.\n  \
                 ═══════════════════════════════════════════════════════════════════════"
            );
        }
        // C `cac::cac` resolves EPICS_CA_CONN_TMO while constructing the
        // context (`cac.cpp:188-194`), so a rejected value is diagnosed at
        // startup even when no circuit ever forms. Resolve it here for the
        // same reason — lazily resolving on the first circuit would swallow
        // the diagnostic for a client that never connects to anything.
        transport::prime_connection_timeout();

        // C `udpiiu` resolves EPICS_CA_REPEATER_PORT once, in its constructor
        // (`udpiiu.cpp:168`), and hands the stored `repeaterPort` to every
        // registration it later sends. One resolution per client here too:
        // the registration helper and the beacon monitor's 1 s retry loop both
        // take this value, so a misconfigured port is diagnosed once — not on
        // every attempt.
        //
        // Both are the beacon path, so both belong to the full `client`: the
        // repeater exists to fan beacons out to every client on the host, and
        // `client-core` binds no UDP socket to receive them on.
        #[cfg(ca_beacon_monitor)]
        let repeater_port = crate::protocol::repeater_port();

        // Run repeater registration in background — don't block client startup.
        #[cfg(ca_beacon_monitor)]
        epics_base_rs::runtime::task::spawn(async move {
            repeater::ensure_repeater(repeater_port).await
        });

        // Build the address list with hostname
        // info preserved so the search-engine refresh task can
        // re-resolve entries whose DNS name maps to a different IP
        // after IOC migration. Pre-fix `parse_addr_list()` returned
        // bare `Vec<SocketAddr>` resolved exactly once at startup,
        // permanently pinning the client to the first-resolved IPs.
        // `mut` only for the discovery merge below, which `client-core` does
        // not have — stated as two bindings rather than one binding and a
        // silenced lint.
        #[cfg(feature = "client")]
        let mut addr_list = parse_addr_list_with_hostnames()?;
        #[cfg(not(feature = "client"))]
        let addr_list = parse_addr_list_with_hostnames()?;

        // Service discovery: explicit config wins; otherwise honour
        // EPICS_CA_DISCOVERY env var. Custom `extra_backends` are then
        // appended. Results are merged with addr_list (deduped by
        // SocketAddr).
        #[cfg(feature = "client")]
        let discovery_cfg = config.discovery.clone().or_else(crate::discovery::from_env);
        #[cfg(feature = "client")]
        let mut backends: Vec<Box<dyn crate::discovery::Backend>> = match discovery_cfg {
            Some(cfg) => crate::discovery::build_backends(cfg),
            None => Vec::new(),
        };
        #[cfg(feature = "client")]
        backends.extend(config.extra_backends);
        // The one-shot scan merges into `addr_list`, the UDP SEARCH
        // destination list. The `subscribe()` wiring below is *not* gated with
        // it: its deltas go to the search engine, which absorbs them on either
        // transport (`SearchTransport::add_address` logs and drops them once,
        // through `log_dropped_udp_mutation`, when no UDP socket is bound).
        #[cfg(feature = "client")]
        if !backends.is_empty() {
            let mut discovered: Vec<SocketAddr> = Vec::new();
            for b in &backends {
                for addr in b.discover().await {
                    if !discovered.contains(&addr) {
                        discovered.push(addr);
                    }
                }
            }
            if !discovered.is_empty() {
                tracing::info!(
                    count = discovered.len(),
                    "discovered IOCs via service discovery: {:?}",
                    discovered
                );
            }
            for addr in discovered {
                if !addr_list.iter().any(|e| e.sock == addr) {
                    let port = match addr {
                        SocketAddr::V4(a) => a.port(),
                        SocketAddr::V6(a) => a.port(),
                    };
                    // Backend-discovered addresses have no DNS name
                    // attached — store them as IP literals (no
                    // refresh).
                    addr_list.push(AddrEntry::new(addr, None, port));
                }
            }
        }

        let nameserver_entries = parse_nameserver_list();
        // Build the per-address SNI registry. Two sources:
        // 1. EPICS_CA_NAME_SERVERS hostnames — live rows, re-keyed by
        //    each entry's own `refresh_dns` so a nameserver that moves
        //    behind a stable DNS name keeps its SNI override
        //    (epics-base#488 residual).
        // 2. EPICS_CA_TLS_SNI_MAP for IPs reached via UDP search
        //   . The CA SEARCH wire protocol carries no
        //    hostname, so a UDP-discovered TLS IOC otherwise has to
        //    fall back to the IP literal. Operators populate this map
        //    with `EPICS_CA_TLS_SNI_MAP="10.0.0.1=ioc1.lab.example.com 10.0.0.2:5064=ioc2.lab.example.com"`
        //    — the addr token may include or omit `:port`; the
        //    matching `connect_server(addr)` looks up the addr first,
        //    then the addr-with-port-zero (wildcard port) form.
        // Per-address overrides win over the global
        // EPICS_CA_TLS_SERVER_NAME (config.tls_server_name).
        #[cfg(feature = "experimental-rust-tls")]
        let sni_overrides = std::sync::Arc::new(SniOverrides::new(
            parse_tls_sni_map().into_iter().collect(),
            nameserver_entries
                .iter()
                .filter_map(|e| e.hostname.clone().map(|h| (h, e.sock))),
        ));
        #[cfg(feature = "experimental-rust-tls")]
        let nameserver_entries: Vec<AddrEntry> = nameserver_entries
            .into_iter()
            .map(|e| e.with_sni(sni_overrides.clone()))
            .collect();

        let (search_tx, search_rx) = mpsc::unbounded_channel();
        let (search_resp_tx, search_resp_rx) = mpsc::unbounded_channel();

        let (transport_tx, transport_rx) = mpsc::unbounded_channel();
        let (transport_evt_tx, transport_evt_rx) = mpsc::unbounded_channel();

        let (coord_tx, coord_rx) = mpsc::unbounded_channel();

        let search_attempts: types::SearchAttempts = Arc::new(dashmap::DashMap::new());
        // Which search engine this client gets is a property of its
        // *configuration*, and of nothing else. The ordinary engine binds the
        // UDP SEARCH socket and *additionally* uses any
        // `EPICS_CA_NAME_SERVERS` circuits; C's documented UDP-free mode
        // (`modules/ca/src/client/CAref.html:515-520`) stays an explicit entry
        // point — `search::name_servers_only_search_engine` — for the reason
        // stated there: deriving it from an empty `EPICS_CA_ADDR_LIST` would
        // silently drop UDP search for a client that starts with no
        // destinations and expects a later `AddAddress` or discovery event to
        // give it some.
        //
        // This used to be `#[cfg(tokio_backend)]` against `#[cfg(exec_backend)]`
        // — the embedded targets took the TCP-only engine whatever their
        // configuration said, because `AsyncUdpV4` did not compile for them. A
        // PV reachable only by broadcast then never resolved and said nothing:
        // an IOC that was itself discoverable but could not discover.
        // `epics_base_rs::net::search_udp` removed the reason, and with it the
        // gate — a target IOC's `ca://` links now resolve the way a C IOC's do.
        let search_task = epics_base_rs::runtime::task::spawn(search::run_search_engine(
            addr_list,
            nameserver_entries,
            search_rx,
            search_resp_tx,
            search_attempts.clone(),
        ));

        // Wire each discovery backend's live-update stream into the
        // search engine. `discover()` above was a one-shot scan;
        // `subscribe()` (mDNS, watch-style DNS) pushes IOCs that come
        // and go *after* startup. Without this, post-startup IOCs are
        // never discovered. The `backends` Vec — and the `ServiceDaemon`
        // an `MdnsBackend` owns — is retained on `CaClient` so the
        // browse keeps running for the client's lifetime.
        #[cfg(feature = "client")]
        let mut discovery_forwarders: Vec<epics_base_rs::runtime::task::TaskHandle<()>> =
            Vec::new();
        #[cfg(feature = "client")]
        for backend in &backends {
            if let Some(mut rx) = backend.subscribe() {
                let fwd_search_tx = search_tx.clone();
                discovery_forwarders.push(epics_base_rs::runtime::task::spawn(async move {
                    // Ref-count each address by how many discovery deltas
                    // currently back it. A backend emits one `Added` per
                    // `(instance, address)` pair and a matching `Removed`
                    // carrying that exact address — so a multi-homed IOC
                    // contributes one ref per interface, two IOCs sharing
                    // an address contribute two refs, and an instance
                    // re-bind is a `Removed` of the old plus an `Added` of
                    // the new. `AddAddress`/`RemoveAddress` are forwarded
                    // only on the 0↔1 edge, so a shared address is
                    // retracted from the search engine only once the last
                    // backer is gone.
                    let mut addr_refs: std::collections::HashMap<SocketAddr, usize> =
                        std::collections::HashMap::new();
                    while let Some(evt) = rx.recv().await {
                        match evt {
                            crate::discovery::DiscoveryEvent::Added { addr, .. } => {
                                let n = addr_refs.entry(addr).or_insert(0);
                                *n += 1;
                                if *n == 1
                                    && fwd_search_tx.send(SearchRequest::AddAddress(addr)).is_err()
                                {
                                    break;
                                }
                            }
                            crate::discovery::DiscoveryEvent::Removed { addr, .. } => {
                                if let Some(n) = addr_refs.get_mut(&addr) {
                                    *n -= 1;
                                    if *n == 0 {
                                        addr_refs.remove(&addr);
                                        if fwd_search_tx
                                            .send(SearchRequest::RemoveAddress(addr))
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                } else {
                                    tracing::debug!(
                                        %addr,
                                        "discovery Removed for untracked address — ignored"
                                    );
                                }
                            }
                        }
                    }
                }));
            }
        }

        #[cfg(feature = "experimental-rust-tls")]
        let tls_arc = config.tls.as_ref().and_then(|t| match t {
            crate::tls::TlsConfig::Client(arc) => Some(arc.clone()),
            crate::tls::TlsConfig::Server(_) => {
                tracing::warn!("server-side TlsConfig passed to CaClient; ignoring");
                None
            }
        });

        let in_flight = InFlightOps::new();
        let cid_alloc = CidAllocator::new();
        let snapshots: ChannelSnapshots = Arc::new(dashmap::DashMap::new());
        let server_writers: DirectServerWriters = Arc::new(dashmap::DashMap::new());
        let last_rx_at: ServerLastRxAt = Arc::new(dashmap::DashMap::new());
        let client_identity: types::ClientIdentitySlot =
            Arc::new(parking_lot::RwLock::new(types::ClientIdentity::from_env()));
        // Built before the transport, because the circuit's receive path — not
        // the coordinator — is what raises server exceptions (C ref:
        // `cac::exceptionRespAction`, `cac.cpp:1082-1120`, runs inside
        // `executeResponse` on the receive thread).
        let exception_slot: types::CaExceptionSlot = Arc::new(parking_lot::RwLock::new(None));

        let transport_task = {
            #[cfg(feature = "experimental-rust-tls")]
            {
                epics_base_rs::runtime::task::spawn(transport::run_transport_manager(
                    transport_rx,
                    transport_evt_tx,
                    in_flight.clone(),
                    exception_slot.clone(),
                    server_writers.clone(),
                    last_rx_at.clone(),
                    client_identity.clone(),
                    tls_arc,
                    config.tls_server_name.clone(),
                    sni_overrides,
                ))
            }
            #[cfg(not(feature = "experimental-rust-tls"))]
            {
                epics_base_rs::runtime::task::spawn(transport::run_transport_manager(
                    transport_rx,
                    transport_evt_tx,
                    in_flight.clone(),
                    exception_slot.clone(),
                    server_writers.clone(),
                    last_rx_at.clone(),
                    client_identity.clone(),
                ))
            }
        };

        let diagnostics = Arc::new(CaDiagnostics::default());

        #[cfg(ca_beacon_monitor)]
        let (beacon_ctrl_tx, beacon_ctrl_rx) =
            mpsc::unbounded_channel::<beacon_monitor::BeaconControl>();

        let coordinator = epics_base_rs::runtime::task::spawn(run_coordinator(
            coord_rx,
            search_resp_rx,
            transport_evt_rx,
            search_tx.clone(),
            transport_tx.clone(),
            in_flight.clone(),
            cid_alloc.clone(),
            snapshots.clone(),
            last_rx_at,
            diagnostics.clone(),
            exception_slot.clone(),
            search_attempts.clone(),
            #[cfg(ca_beacon_monitor)]
            beacon_ctrl_tx,
        ));

        #[cfg(ca_beacon_monitor)]
        let beacon_task = epics_base_rs::runtime::task::spawn(beacon_monitor::run_beacon_monitor(
            coord_tx.clone(),
            beacon_ctrl_rx,
            repeater_port,
        ));

        Ok(Self {
            search_tx,
            transport_tx,
            coord_tx,
            one_shot_channels: Mutex::new(OneShotChannelCache::default()),
            in_flight,
            cid_alloc,
            snapshots,
            server_writers,
            diagnostics,
            search_attempts,
            exception_slot,
            client_identity,
            _coordinator: coordinator,
            _search_task: search_task,
            _transport_task: transport_task,
            #[cfg(ca_beacon_monitor)]
            _beacon_task: beacon_task,
            #[cfg(feature = "client")]
            _discovery_backends: backends,
            #[cfg(feature = "client")]
            _discovery_forwarders: discovery_forwarders,
        })
    }

    /// Get a snapshot of diagnostic counters.
    pub fn diagnostics(&self) -> DiagnosticsSnapshot {
        self.diagnostics.snapshot()
    }

    /// Register a per-client handler for out-of-band errors —
    /// the Rust analog of libca `ca_add_exception_event` (cadef.h:617).
    /// Scope is the calling [`CaClient`] instance; each client owns
    /// its own slot.
    ///
    /// Currently dispatches on:
    /// - `CaExceptionKind::ServerError` — server emitted
    ///   `CA_PROTO_ERROR` (cmd=11) for an op not routed to a callback.
    /// - `CaExceptionKind::ServerDisconnect` — server emitted
    ///   `CA_PROTO_SERVER_DISCONN` for a known channel.
    ///
    /// Routine per-operation errors (timeouts, type mismatches) are
    /// returned through the operation's `Result` and do **not** fire
    /// the handler. Startup config / TLS errors surface through
    /// [`CaClient::new`]'s `Result` since the slot does not yet exist.
    ///
    /// At most one handler is registered at a time; calling again
    /// replaces. Returns the previous handler if present so callers
    /// can chain.
    pub fn set_exception_handler<F>(&self, f: F) -> Option<types::CaExceptionHandler>
    where
        F: Fn(&types::CaException) + Send + Sync + 'static,
    {
        let new = Arc::new(f);
        let mut slot = self.exception_slot.write();
        slot.replace(new)
    }

    /// Drop the registered handler. Subsequent OOB errors will only
    /// surface through `tracing::error!` (the default behaviour).
    pub fn clear_exception_handler(&self) -> Option<types::CaExceptionHandler> {
        self.exception_slot.write().take()
    }

    /// Override the user name this client advertises to servers
    /// (`CA_PROTO_CLIENT_NAME`). libca takes the user name from `$USER`
    /// at context creation; this lets it be changed at runtime.
    ///
    /// Applies to circuits opened **after** this call: the shared
    /// identity slot is read when each new circuit handshakes (before its
    /// first channel), so the server accepts the name. Circuits already
    /// established keep their original user name — the EPICS server
    /// rejects `CA_PROTO_CLIENT_NAME` once a circuit has created any
    /// channel (`rsrv/camessage.c` `client_name_action`: `chanCount != 0`
    /// → `ECA_INTERNAL`, name ignored), so there is no in-band way to
    /// rename a live circuit.
    pub fn set_user_name(&self, user: impl Into<String>) {
        self.client_identity.write().user = user.into();
    }

    /// Override the host name this client advertises to servers
    /// (`CA_PROTO_HOST_NAME`). Same scope as [`CaClient::set_user_name`]:
    /// new circuits read the updated slot at handshake time; existing
    /// circuits keep their host name (the EPICS server ignores
    /// `CA_PROTO_HOST_NAME` after the first channel — `host_name_action`,
    /// `chanCount != 0` → `ECA_INTERNAL`).
    pub fn set_host_name(&self, host: impl Into<String>) {
        self.client_identity.write().host = host.into();
    }

    /// Number of distinct operational CA virtual circuits this client
    /// currently holds. libca keys its circuit table on
    /// `(address, CA priority)` (`caServerID::operator==`,
    /// `caServerID.h:47-55`), so two channels to the same IOC at
    /// different priorities are two circuits. Mirrors libca
    /// `ca_get_ioc_connection_count()` (`cac::circuitCount` returns
    /// `circuitList.count`, `cac.cpp:403` / `access.cpp:671`) — a
    /// circuit-level (not channel-level) count, useful for sizing
    /// reconnect storms after an IOC restart.
    pub async fn ioc_connection_count(&self) -> usize {
        let (tx, rx) = oneshot::channel();
        if self
            .coord_tx
            .send(CoordRequest::GetIocConnectionCount { reply: tx })
            .is_err()
        {
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    /// Graceful shutdown: send ClearChannel for all connected channels
    /// so servers can release resources immediately.
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::Shutdown { reply: tx });
        // Wait briefly for the clear commands to be sent. Through the
        // `runtime::task` seam: on the RTEMS target there is no tokio timer
        // to drive `tokio::time::timeout`, and shutdown would panic instead
        // of draining.
        let _ = epics_base_rs::runtime::task::timeout(Duration::from_secs(2), rx).await;
    }

    /// Create a persistent channel. Returns immediately (starts searching in background).
    ///
    /// Uses CA `priorityDefault` (0). To request a specific virtual-circuit
    /// priority, use [`Self::create_channel_with_priority`].
    pub fn create_channel(&self, name: &str) -> CaChannel {
        self.create_channel_expanded(expand_pv_name(name), 0)
    }

    /// Create a persistent channel at a specific CA priority (`0..=99`,
    /// libca `cacChannel::priorityMax`).
    ///
    /// priority is part of the virtual-circuit identity. Two
    /// channels to the same IOC at different priorities open independent
    /// TCP circuits (libca `ca_create_channel` priority parameter,
    /// `cadef.h:498-508`), so bulk and latency-sensitive traffic can be
    /// isolated. The priority is sent to the server in the
    /// `CA_PROTO_VERSION` message's data-type field. Values above 99 are
    /// clamped to 99 to match libca's range check
    /// (`cac.cpp:512-520`).
    pub fn create_channel_with_priority(&self, name: &str, priority: u8) -> CaChannel {
        self.create_channel_expanded(expand_pv_name(name), priority.min(99))
    }

    fn create_channel_expanded(&self, pv_name: String, priority: u8) -> CaChannel {
        // reserve the cid against the live-set. Released by the
        // coordinator at `DropChannel` (the single channel-removal site).
        let cid = self.cid_alloc.allocate();
        let (conn_tx, _) = broadcast::channel(16);

        // C `cac::createChannel` rejects empty names with
        // ECA_BADSTR, and `tcpiiu::createChannelRequest` rejects
        // padded postCnt >= 0xffff with ECA_UNAVAILINSERV before
        // anything reaches the wire. Pre-fix Rust truncated postsize
        // via `as u16` and still emitted the full body, producing
        // malformed UDP SEARCH and TCP CREATE_CHAN frames that could
        // desynchronise the peer's CA parser. Validate up front; for
        // invalid inputs register the channel locally (so subscribe /
        // get / drop don't panic on a missing entry) but skip the
        // search schedule so nothing reaches the wire. The returned
        // CaChannel sits in Disconnected and times out naturally
        // when callers await connect, matching libca's client-side
        // reject semantic.
        let padded_len = crate::protocol::pad_string(&pv_name).len();
        let name_empty = pv_name.is_empty();
        let name_too_long = padded_len >= 0xFFFF;
        let valid = !name_empty && !name_too_long;
        if !valid {
            tracing::warn!(
                cid,
                pv_name = %pv_name,
                len = pv_name.len(),
                padded_len,
                reason = if name_empty { "empty" } else { "too long" },
                "create_channel: invalid PV name rejected (libca ECA_BADSTR / ECA_UNAVAILINSERV parity); channel will not connect"
            );
            metrics::counter!("ca_client_create_channel_rejects_total").increment(1);
        }

        let _ = self.coord_tx.send(CoordRequest::RegisterChannel {
            cid,
            pv_name: pv_name.clone(),
            priority,
            conn_tx: conn_tx.clone(),
        });

        if valid {
            let _ = self.search_tx.send(SearchRequest::Schedule {
                cid,
                pv_name: pv_name.clone(),
                reason: SearchReason::Initial,
            });
        }

        let lifecycle = Arc::new(ChannelLifecycle {
            cid,
            coord_tx: self.coord_tx.clone(),
        });
        let channel_pv_name: Arc<str> = Arc::from(pv_name.as_str());
        CaChannel {
            cid,
            pv_name: channel_pv_name,
            priority,
            coord_tx: self.coord_tx.clone(),
            transport_tx: self.transport_tx.clone(),
            in_flight: self.in_flight.clone(),
            snapshots: self.snapshots.clone(),
            server_writers: self.server_writers.clone(),
            conn_tx,
            cached_read: Arc::new(Mutex::new(None)),
            search_attempts: self.search_attempts.clone(),
            user_data: Arc::new(Mutex::new(None)),
            _lifecycle: lifecycle,
        }
    }

    fn cached_one_shot_channel(&self, name: &str) -> CaChannel {
        let pv_name = expand_pv_name(name);
        self.one_shot_channels.lock().get_or_create(self, pv_name)
    }

    // --- Legacy one-shot API (backwards-compatible) ---

    /// Append `addr` to the search engine's working address list at
    /// runtime. Mirrors libca `addAddrToChannelAccessAddressList`
    /// (iocinf.cpp:45) — the new entry is consulted on the next
    /// scheduled search round. Use when the application learns of
    /// a new IOC after `CaClient::new()` (e.g., service-discovery
    /// callback). Idempotent; duplicates are skipped.
    pub fn add_address(&self, addr: SocketAddr) {
        let _ = self.search_tx.send(SearchRequest::AddAddress(addr));
    }

    /// Replace the search engine's working address list. Mirrors
    /// libca `configureChannelAccessAddressList` (iocinf.cpp:166).
    /// Use when the application has authoritative knowledge of the
    /// IOC topology and wants to override env-derived state.
    pub fn set_address_list(&self, list: Vec<SocketAddr>) {
        let _ = self.search_tx.send(SearchRequest::SetAddressList(list));
    }

    pub async fn caget(&self, pv_name: &str) -> CaResult<(DbFieldType, EpicsValue)> {
        let ch = self.create_channel(pv_name);
        ch.wait_connected(Duration::from_secs(3)).await?;
        let result = ch.get().await;
        let _ = self
            .coord_tx
            .send(CoordRequest::DropChannel { cid: ch.cid });
        result
    }

    /// Bulk read for many PV names.
    ///
    /// Channels are cached by expanded PV name, so repeated calls avoid
    /// create/search/connect overhead and go straight through
    /// [`Self::get_many_with_timeout`], which coalesces same-server
    /// `READ_NOTIFY` requests into a single writer enqueue. Cold or
    /// disconnected cached channels are connected and retried once.
    /// Results are returned in input order.
    pub async fn caget_many<S>(&self, pv_names: &[S]) -> Vec<CaResult<(DbFieldType, EpicsValue)>>
    where
        S: AsRef<str> + Sync,
    {
        self.caget_many_with_timeout(pv_names, Duration::from_secs(30))
            .await
    }

    /// Bulk by-name read with a caller-supplied connect/read timeout.
    pub async fn caget_many_with_timeout<S>(
        &self,
        pv_names: &[S],
        timeout: Duration,
    ) -> Vec<CaResult<(DbFieldType, EpicsValue)>>
    where
        S: AsRef<str> + Sync,
    {
        let channels: Vec<CaChannel> = pv_names
            .iter()
            .map(|name| self.cached_one_shot_channel(name.as_ref()))
            .collect();

        let mut results = self.get_many_with_timeout(&channels, timeout).await;

        let retry_indices: Vec<usize> = results
            .iter()
            .enumerate()
            .filter_map(|(idx, result)| {
                if matches!(
                    result,
                    Err(CaError::Disconnected) | Err(CaError::ChannelNotFound(_))
                ) {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect();

        if retry_indices.is_empty() {
            return results;
        }

        let connected = futures_util::future::join_all(retry_indices.iter().map(|&idx| {
            let channel = channels[idx].clone();
            async move { (idx, channel.wait_connected(timeout).await) }
        }))
        .await;

        let mut ready_indices = Vec::new();
        let mut ready_channels = Vec::new();
        for (idx, result) in connected {
            match result {
                Ok(()) => {
                    ready_indices.push(idx);
                    ready_channels.push(channels[idx].clone());
                }
                Err(e) => results[idx] = Err(e),
            }
        }

        let read_results = self.get_many_with_timeout(&ready_channels, timeout).await;
        for (idx, result) in ready_indices.into_iter().zip(read_results) {
            results[idx] = result;
        }

        results
    }

    /// Bulk read for already-created channels.
    ///
    /// Unlike spawning `ch.get()` N times, this path constructs every
    /// request up front, groups requests by server, and enqueues one
    /// concatenated CA frame per server. That matches libca's bulk
    /// flush model more closely and avoids per-PV task scheduling.
    /// Results are returned in channel order.
    pub async fn get_many(
        &self,
        channels: &[CaChannel],
    ) -> Vec<CaResult<(DbFieldType, EpicsValue)>> {
        self.get_many_with_timeout(channels, Duration::from_secs(30))
            .await
    }

    /// Bulk read for already-created channels with a caller-supplied timeout.
    pub async fn get_many_with_timeout(
        &self,
        channels: &[CaChannel],
        timeout: Duration,
    ) -> Vec<CaResult<(DbFieldType, EpicsValue)>> {
        CaChannel::get_many_with_timeout(channels, timeout).await
    }

    /// Fire-and-forget write (CA_PROTO_WRITE). Matches C `caput` behavior.
    pub async fn caput(&self, pv_name: &str, value_str: &str) -> CaResult<()> {
        let ch = self.create_channel(pv_name);
        ch.wait_connected(Duration::from_secs(3)).await?;

        let snap = ch.snapshot()?;
        let value = EpicsValue::parse(snap.native_type, value_str)?;
        ch.put_nowait(&value).await?;
        let _ = self
            .coord_tx
            .send(CoordRequest::DropChannel { cid: ch.cid });
        Ok(())
    }

    /// Write with completion callback (CA_PROTO_WRITE_NOTIFY). Matches C `caput -c`.
    pub async fn caput_callback(
        &self,
        pv_name: &str,
        value_str: &str,
        timeout_secs: f64,
    ) -> CaResult<()> {
        let ch = self.create_channel(pv_name);
        // Caller-supplied seconds: a non-finite or negative value is a
        // panic for `Duration::from_secs_f64`, so go through the same
        // saturating conversion the env-derived timeouts use.
        let timeout = crate::estdlib::duration_from_secs(timeout_secs);
        ch.wait_connected(timeout).await?;

        let snap = ch.snapshot()?;
        let value = EpicsValue::parse(snap.native_type, value_str)?;
        ch.put_with_timeout(&value, timeout).await?;
        let _ = self
            .coord_tx
            .send(CoordRequest::DropChannel { cid: ch.cid });
        Ok(())
    }

    pub async fn cainfo(&self, pv_name: &str) -> CaResult<ChannelInfo> {
        let ch = self.create_channel(pv_name);
        ch.wait_connected(Duration::from_secs(3)).await?;

        let info = ch.info().await;
        let _ = self
            .coord_tx
            .send(CoordRequest::DropChannel { cid: ch.cid });
        info
    }

    /// Monitor a PV with callback (legacy API).
    pub async fn camonitor<F>(&self, pv_name: &str, mut callback: F) -> CaResult<()>
    where
        F: FnMut(EpicsValue),
    {
        let ch = self.create_channel(pv_name);
        let mut monitor = ch.subscribe().await?;

        while let Some(result) = monitor.recv().await {
            match result {
                Ok(snap) => callback(snap.value),
                Err(e) => return Err(e),
            }
        }

        Ok(())
    }
}

/// libca `cac::~cac` parity: best-effort graceful drain on drop.
///
/// libca's destructor walks every `tcpiiu` (per-circuit object) and
/// signals shutdown — the send thread flushes pending writes
/// (including the `ClearChannel` frames `ca_context_destroy` emits
/// for every operational channel) before exit, then `pthread_join`
/// waits for both per-circuit threads to actually finish. The result:
/// servers learn their channels are gone *immediately* via wire
/// `ClearChannel`, rather than discovering it via TCP RST + their
/// own watchdog.
///
/// We approximate the same outcome despite the sync `Drop`, spawning a
/// detached cleanup task through the `runtime::task` spawn seam that
/// sends `CoordRequest::Shutdown` (the coordinator's handler at line
/// ~2160 emits `ClearChannel` for every operational channel before
/// returning) and awaits the reply with a 2-s ceiling. Once the reply
/// lands, abort the four top-level tasks. Aborts cascade through the
/// `connections` HashMap → `ServerConnection` Drop → per-circuit
/// read/write tasks.
///
/// The spawn goes through `epics_base_rs::runtime::task::spawn` like the
/// client's other 17 production tasks, not a bare `tokio::spawn` behind a
/// `Handle::try_current()` guard: under the RTEMS exec backend the guard
/// would return `Err` and silently skip the whole cleanup, so the
/// bounded coordinator shutdown would never run on target. The seam
/// dispatches onto the callback-band executor there, so the drain runs on
/// both backends (design doc §5.1, §6 Stage C3).
///
/// Residual differences from libca that this can't bridge in a sync
/// `Drop` body:
///   * Tokio's `JoinHandle::abort` is cooperative cancellation at the
///     next `.await`, not pthread cancellation. Tasks blocked on a
///     non-yielding system call (rare here — every loop has a recv
///     await) would not unblock immediately. libca achieves the same
///     by closing the socket; tokio's drop of the socket on cancel
///     is functionally equivalent.
///   * The detached cleanup task itself is at the runtime's mercy:
///     if the runtime tears down before the cleanup task completes,
///     it gets aborted mid-shutdown. Callers that need GUARANTEED
///     graceful drain must still call `client.shutdown().await`
///     before dropping; that path is bounded by the caller's own
///     await and not by Drop's best effort.
///   * `ServerConnection::Drop` aborts read/write per-circuit tasks
///     immediately. We don't drain pending write_tx queues at
///     per-circuit teardown — the `ClearChannel` frames are queued
///     into `transport_tx` at the coord level and forwarded to
///     write_loop, but we don't explicitly wait for write_loop to
///     flush. This is the same trade-off libca's pre-`SO_LINGER`
///     behaviour makes: the kernel send buffer + RST handles
///     last-mile delivery from the server's perspective.
impl Drop for CaClient {
    fn drop(&mut self) {
        let coord_tx = self.coord_tx.clone();
        let coord_abort = self._coordinator.abort_handle();
        let search_abort = self._search_task.abort_handle();
        let transport_abort = self._transport_task.abort_handle();
        #[cfg(ca_beacon_monitor)]
        let beacon_abort = self._beacon_task.abort_handle();

        // Discovery forwarders hold a `search_tx` clone — abort them
        // so they don't outlive the client. The `_discovery_backends`
        // Vec drops with `self`, tearing down any `ServiceDaemon`.
        #[cfg(feature = "client")]
        for fwd in &self._discovery_forwarders {
            fwd.abort();
        }

        // Drop must be infallible in ANY thread context. On the tokio
        // backend `runtime::task::spawn` panics without an ambient
        // reactor — which is exactly the state of a sync caller
        // unwinding on its own thread, where the panic would mask the
        // error that triggered the unwind. Without a reactor the
        // graceful drain cannot be polled: send the shutdown request
        // (unbounded, sync) and abort the tasks directly —
        // `AbortHandle::abort` is thread-safe and runtime-free. The
        // server reclaims the circuit on TCP close, the same path as a
        // client that dies without `ca_context_destroy`. The guard is
        // tokio-only: on the RTEMS exec backend the executor always
        // exists, and design doc §5.1 forbids silently skipping the
        // drain there.
        #[cfg(tokio_backend)]
        if tokio::runtime::Handle::try_current().is_err() {
            let (tx, _rx) = oneshot::channel();
            let _ = coord_tx.send(CoordRequest::Shutdown { reply: tx });
            coord_abort.abort();
            transport_abort.abort();
            search_abort.abort();
            #[cfg(ca_beacon_monitor)]
            beacon_abort.abort();
            return;
        }

        // Spawn the graceful drain through the `runtime::task` seam so it
        // runs on the RTEMS exec backend too — a bare `tokio::spawn`
        // behind a `Handle::try_current()` guard silently skipped it on
        // target (design doc §5.1).
        epics_base_rs::runtime::task::spawn(async move {
            let (tx, rx) = oneshot::channel();
            if coord_tx.send(CoordRequest::Shutdown { reply: tx }).is_ok() {
                // Bounded so a wedged coordinator doesn't keep
                // the cleanup task alive indefinitely. The bound goes
                // through the same seam as the `spawn` above: this future
                // is polled on the RTEMS exec backend, where `tokio::time`
                // has no timer and panics the band worker.
                let _ = epics_base_rs::runtime::task::timeout(Duration::from_secs(2), rx).await;
            }
            coord_abort.abort();
            transport_abort.abort();
            search_abort.abort();
            #[cfg(ca_beacon_monitor)]
            beacon_abort.abort();
        });
    }
}

/// A persistent CA channel with auto-reconnection.
/// Per-channel lifecycle guard. Holds the coordinator sender so a
/// `DropChannel` request fires exactly once — when the last
/// [`CaChannel`] clone is dropped. Pulled into its own type so that
/// `CaChannel: Clone` does NOT trigger a tear-down on every clone.
struct ChannelLifecycle {
    cid: u32,
    coord_tx: mpsc::UnboundedSender<CoordRequest>,
}

impl Drop for ChannelLifecycle {
    fn drop(&mut self) {
        let _ = self
            .coord_tx
            .send(CoordRequest::DropChannel { cid: self.cid });
    }
}

/// Warm READ_NOTIFY cache: persistent ioid + reusable Sender slot used
/// by `get_many_with_timeout` to skip per-call `alloc_ioid` + DashMap
/// insert/remove. The first successful default GET on a channel
/// populates this; subsequent calls reuse `(ioid, sid, slot)` and only
/// pay the actual `READ_NOTIFY` frame send + response decode.
///
/// Mirrors `epics-pva-rs` `CachedGet` — see `pvget_many` in
/// `client_native/context.rs` for the original design.
///
/// Invalidation: the channel-side caller compares
/// `(server_addr, sid, data_type, element_count)` against the current
/// snapshot before each warm-call. On mismatch (reconnect / DBR change /
/// element-count change) the cached entry is dropped and the
/// dispatcher's DashMap entry is removed; the next call falls back to
/// the cold path.
///
/// On disconnect, `drain_waiters_for_cids` removes the DashMap entry
/// and signals `Disconnected` through `slot`; the channel-side
/// `Option<CachedRead>` is left in place but the (server_addr, sid)
/// mismatch on reconnect re-pulls it into a fresh cold call.
pub(crate) struct CachedRead {
    pub(crate) ioid: u32,
    pub(crate) sid: u32,
    pub(crate) server_addr: SocketAddr,
    pub(crate) data_type: u16,
    pub(crate) element_count: u32,
    pub(crate) slot: types::WarmReplySlot,
}

/// C `nciu::write` (`libca/nciu.cpp`) and its write-callback
/// overload reject `countIn > this->count` before queueing the
/// request, surfacing as `ECA_BADCOUNT`. Pre-fix Rust forwarded any
/// caller-supplied count to the server, which the server would then
/// reject asynchronously (callback path) or accept past its array
/// bound (nowait path). Validate client-side so the call fails
/// synchronously the way libca does.
/// Default deadline for a `CA_PROTO_WRITE_NOTIFY` that the caller did not
/// time-box itself, overridable via `EPICS_CA_PUT_TIMEOUT` (seconds).
///
/// Port-specific knob (libca leaves the deadline to `ca_pend_io`), but it
/// resolves through the same C primitives as every other env-derived
/// double — `epicsScanDouble` plus the saturating `Duration` conversion —
/// so no environment string can panic a `put`.
fn put_timeout() -> Duration {
    static RESOLVED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(|| {
        crate::estdlib::env_double("EPICS_CA_PUT_TIMEOUT")
            .map(crate::estdlib::duration_from_secs)
            .unwrap_or(Duration::from_secs(30))
    })
}

fn validate_put_count(snap: &types::ChannelSnapshotPublic, count: u32) -> CaResult<()> {
    if count > snap.element_count {
        return Err(CaError::Protocol(format!(
            "put count {} exceeds channel element count {} \
             (matches libca nciu::write ECA_BADCOUNT)",
            count, snap.element_count
        )));
    }
    Ok(())
}

/// C `nciu::stringVerify` rejects DBR_STRING elements that
/// exceed `MAX_STRING_SIZE` (40 bytes including the NUL terminator)
/// with `ECA_STRTOBIG`. Pre-fix Rust silently truncated long strings
/// to 39 bytes + NUL inside `EpicsValue::to_bytes()`, so a
/// `put_string()` could write a different value than the caller
/// supplied. Validate up front so the call fails synchronously.
const MAX_STRING_SIZE: usize = 40;
fn validate_string_length(s: impl AsRef<[u8]>) -> CaResult<()> {
    // The CA cap is a byte cap (DBR_STRING is a fixed 40-byte field, 39 + NUL),
    // so measure the raw bytes — a lossy text view would mis-count non-UTF-8.
    // C requires room for the trailing NUL: payload >= len + 1.
    let len = s.as_ref().len();
    if len >= MAX_STRING_SIZE {
        return Err(CaError::Protocol(format!(
            "string of {len} bytes exceeds MAX_STRING_SIZE - 1 = 39 \
             (matches libca nciu::stringVerify ECA_STRTOBIG)"
        )));
    }
    Ok(())
}

/// Reject any DBR_STRING / DBR_STRING_ARRAY element in `value` that
/// exceeds the libca string cap. Scalar / numeric variants pass
/// through unchanged.
fn validate_put_strings(value: &EpicsValue) -> CaResult<()> {
    match value {
        EpicsValue::String(s) => validate_string_length(s),
        EpicsValue::StringArray(arr) => {
            for s in arr {
                validate_string_length(s)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[derive(Clone)]
pub struct CaChannel {
    cid: u32,
    pv_name: Arc<str>,
    /// CA priority (0..=99) this channel was created at. With
    /// the channel's resolved `server_addr` it forms the
    /// [`types::CircuitKey`] every hot-path `TransportCommand` targets,
    /// and the key the `server_writers` sidecar is looked up by.
    priority: u8,
    coord_tx: mpsc::UnboundedSender<CoordRequest>,
    transport_tx: mpsc::UnboundedSender<TransportCommand>,
    /// Shared in-flight registry for reads and writes (Option C
    /// Phase A). `ch.get()` / `ch.put()` insert their reply oneshots
    /// directly here, bypassing the coordinator's `tokio::select!`
    /// loop. The transport's per-server read loop fulfils them on
    /// `ReadResponse` / `WriteResponse` arrival.
    in_flight: InFlightOps,
    /// Per-channel snapshot sidecar (Option C, Phase B). Read by hot
    /// paths in lieu of `CoordRequest::GetChannelInfo`.
    snapshots: ChannelSnapshots,
    /// Per-server writer sidecar (Option C, Phase E). Read/write hot
    /// paths use this after `snapshot()` proves the channel is active.
    server_writers: DirectServerWriters,
    conn_tx: broadcast::Sender<ConnectionEvent>,
    /// Warm-read fast path. `None` until the first successful default
    /// GET; refilled (with a fresh Sender) on every subsequent
    /// `get_many_with_timeout` call. See `CachedRead`.
    ///
    /// `Arc<Mutex<...>>` so all clones of a `CaChannel` share the same
    /// cache slot — otherwise two clones would each pay the cold-path
    /// once on first use.
    cached_read: Arc<Mutex<Option<CachedRead>>>,
    /// Shared per-channel SEARCH attempt count (CA-035). Same map
    /// the SearchEngine bumps on every fanout (immediate + retransmit);
    /// cleared when the channel transitions to Connected.
    search_attempts: types::SearchAttempts,
    /// Opaque per-channel user data — Rust analog of libca
    /// `ca_set_puser`/`ca_puser` `void *` (cadef.h:243/251). Wrapped
    /// in an `Arc<Mutex<>>` so independent clones of the same channel
    /// see the same slot (the libca contract is "one PUSER per chid").
    user_data: Arc<Mutex<Option<Arc<dyn std::any::Any + Send + Sync>>>>,
    /// Refcounted lifecycle guard — see [`ChannelLifecycle`].
    _lifecycle: Arc<ChannelLifecycle>,
}

#[cfg(test)]
impl CaChannel {
    /// Minimal throwaway channel for unit tests that need a
    /// [`MonitorHandle::channel`] field but never exercise the channel
    /// itself. Wires the provided `coord_tx` (so the lifecycle guard
    /// targets a live receiver) and leaves every sidecar empty.
    fn for_test(coord_tx: mpsc::UnboundedSender<CoordRequest>) -> Self {
        let (conn_tx, _) = broadcast::channel(1);
        let (transport_tx, _) = mpsc::unbounded_channel();
        let cid = 0;
        let lifecycle = Arc::new(ChannelLifecycle {
            cid,
            coord_tx: coord_tx.clone(),
        });
        CaChannel {
            cid,
            pv_name: Arc::from("test:pv"),
            priority: 0,
            coord_tx,
            transport_tx,
            in_flight: InFlightOps::new(),
            snapshots: ChannelSnapshots::default(),
            server_writers: DirectServerWriters::default(),
            conn_tx,
            cached_read: Arc::new(Mutex::new(None)),
            search_attempts: types::SearchAttempts::default(),
            user_data: Arc::new(Mutex::new(None)),
            _lifecycle: lifecycle,
        }
    }
}

/// How a CA GET resolves its on-the-wire READ_NOTIFY element count.
///
/// `caget` issues its request in one of two modes, and C resolves the
/// user's `-#` element count differently in each (`caget.c:197-218`):
///
/// - [`ReqCount::Fixed`] mirrors the synchronous `ca_array_get` branch
///   (`caget.c:208`): `reqElems && reqElems < nElems ? reqElems : nElems`.
///   A `0` (no `-#`, or `-# 0`) means "the channel's current native
///   element count", so the request always carries a concrete count.
/// - [`ReqCount::Autosize`] mirrors the `ca_array_get_callback` branch
///   (`caget.c:200-205`): `reqElems > nElems ? nElems : reqElems`. A `0`
///   (no positive `-#`) is sent **verbatim** as the CA autosize request,
///   so the IOC reports each response with the record's *current* element
///   count — a dynamic waveform with `NORD < NELM` returns only `NORD`
///   elements instead of the full `NELM`.
///
/// `From<u32>` maps a bare count to [`ReqCount::Fixed`], so existing
/// callers that pass `0` keep the "0 = native count" contract unchanged;
/// only the callback tool front-end constructs [`ReqCount::Autosize`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReqCount {
    /// Request exactly this many elements; `0` is resolved to the native
    /// element count (synchronous `ca_array_get`, `caget.c:208`).
    Fixed(u32),
    /// Send this count verbatim, including `0` (CA autosize); callback
    /// `ca_array_get_callback`, `caget.c:200`.
    Autosize(u32),
}

impl From<u32> for ReqCount {
    /// A bare element count is a [`ReqCount::Fixed`] request — preserving
    /// the long-standing "0 = full native count" convention for every
    /// caller that has not opted into autosize.
    fn from(count: u32) -> Self {
        ReqCount::Fixed(count)
    }
}

impl ReqCount {
    /// The concrete element count to put on the wire, given the channel's
    /// `native` element count. `Fixed(0)` resolves to `native`; every
    /// other value (including `Autosize(0)`) is passed through. The GET
    /// paths still apply libca's `ECA_BADCOUNT` guard, so a resolved count
    /// greater than `native` is rejected rather than silently clamped.
    pub fn resolve(self, native: u32) -> u32 {
        match self {
            ReqCount::Fixed(0) => native,
            ReqCount::Fixed(n) | ReqCount::Autosize(n) => n,
        }
    }
}

impl CaChannel {
    pub async fn wait_connected(&self, timeout: Duration) -> CaResult<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::WaitConnected {
            cid: self.cid,
            reply: reply_tx,
        });
        epics_base_rs::runtime::task::timeout(timeout, reply_rx)
            .await
            .map_err(|_| CaError::ChannelNotFound(self.pv_name.to_string()))?
            .map_err(|_| CaError::Shutdown)
    }

    /// Get channel-level metadata (native type, element count, host, access rights)
    /// without performing a CA read.
    pub async fn info(&self) -> CaResult<ChannelInfo> {
        let snap = self.snapshot()?;
        Ok(ChannelInfo {
            pv_name: self.pv_name.to_string(),
            server_addr: snap.server_addr,
            native_type: snap.native_type,
            element_count: snap.element_count,
            access_rights: snap.access_rights,
        })
    }

    /// Number of SEARCH attempts the engine has emitted on behalf of
    /// this channel since it was last connected. Counts the immediate
    /// first SEARCH (fired at Schedule time) AND every subsequent
    /// bucket-tick retransmit. One attempt == one fanout call
    /// regardless of how many UDP datagrams the addr_list /
    /// nameserver duplication produces, matching libca
    /// `ca_search_attempts(chid)` semantics (cadef.h:1907).
    ///
    /// Returns 0 for an already-connected channel (the counter is
    /// cleared on successful CREATE_CHANNEL).
    pub fn search_attempts(&self) -> u32 {
        self.search_attempts
            .get(&self.cid)
            .map(|e| e.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Phase B fast-path: read the channel's published snapshot without
    /// touching the coordinator. Returns `Disconnected` if either no
    /// snapshot is published yet (channel is searching/connecting/
    /// failed-to-resolve native type) or the snapshot reflects a
    /// non-operational state.
    fn snapshot(&self) -> CaResult<ChannelSnapshotPublic> {
        match self.snapshots.get(&self.cid) {
            Some(s) if s.state.is_operational() => Ok(s.clone()),
            _ => Err(CaError::Disconnected),
        }
    }

    fn direct_writer(&self, server_addr: SocketAddr) -> Option<DirectServerWriter> {
        // the writer sidecar is keyed by circuit, so look up
        // this channel's `(server_addr, priority)` — a sibling channel
        // to the same server at another priority owns a different
        // circuit and writer.
        self.server_writers
            .get(&(server_addr, self.priority))
            .map(|w| w.clone())
    }

    fn build_read_notify_frame(
        sid: u32,
        data_type: u16,
        count: u32,
        ioid: u32,
        peer_minor: u16,
    ) -> CaResult<Vec<u8>> {
        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.data_type = data_type;
        hdr.cid = sid;
        hdr.available = ioid;
        // C parity (`comQueSend.cpp:285`): extended form is needed for
        // `nElem >= 0xffff`, not just `> 0xffff`. Routing 0xFFFF
        // through the normal branch would wedge `count = 0xFFFF` into
        // `m_count` — which a strict peer interprets as an extended
        // marker with missing annex bytes. `set_payload_size` refuses
        // the extended form for a pre-V49 peer (`comQueSend.cpp:299`
        // `throw cacChannel::outOfBounds()` → ECA_BADCOUNT,
        // `oldChannelNotify.cpp:309`) without a byte reaching the wire.
        // In practice the caller's element bound (ECA_TOLARGE,
        // `tcpiiu.cpp:1470`) fires first on a pre-V49 circuit: MAX_TCP
        // caps the count far below 0xffff.
        if count >= 0xFFFF {
            hdr.set_payload_size(0, count, peer_minor)
                .map_err(|_| CaError::BadCount)?;
        } else {
            hdr.count = count as u16;
        }
        Ok(hdr.to_bytes_extended())
    }

    fn decode_plain_read_reply(reply: ReadReply) -> CaResult<(DbFieldType, EpicsValue)> {
        match reply {
            ReadReply::Plain { dbr_type, value } => Ok((dbr_type, value)),
            ReadReply::Raw {
                data_type,
                count,
                data,
            } => {
                let dbr_type = DbFieldType::from_u16(data_type)?;
                EpicsValue::from_bytes_array(dbr_type, &data, count as usize)
                    .map(|value| (dbr_type, value))
            }
        }
    }

    /// Thin alias for the crate's put-framing owner,
    /// [`crate::protocol::build_put_frame`] (C
    /// `comQueSend::insertRequestWithPayLoad`). The coordinator path in
    /// `transport.rs` frames through the same function, so the wire
    /// bytes cannot drift between the direct-writer and coordinator
    /// routes.
    fn build_write_frame(
        cmd: u16,
        sid: u32,
        data_type: u16,
        count: u32,
        ioid: Option<u32>,
        payload: Vec<u8>,
        peer_minor: u16,
    ) -> CaResult<Vec<u8>> {
        crate::protocol::build_put_frame(cmd, sid, data_type, count, ioid, payload, peer_minor)
    }

    fn send_read_notify_fast(
        &self,
        snap: &ChannelSnapshotPublic,
        data_type: u16,
        count: u32,
        ioid: u32,
    ) -> CaResult<()> {
        // C `libca/nciu.cpp::read()` rejects before queueing
        // when `!accessRightState.readPermit()` (ECA_NORDACCESS).
        // Pre-fix Rust forwarded the request anyway and let the
        // server (or a timeout) surface the denial; the cached
        // access bits from the last CA_PROTO_ACCESS_RIGHTS frame
        // are the authoritative client-side gate libca consults.
        if !snap.access_rights.read {
            return Err(CaError::Protocol(format!(
                "read denied by cached access rights (matches libca \
                 nciu::read ECA_NORDACCESS); ioid {ioid}"
            )));
        }
        // C `tcpiiu::readNotifyRequest` (`tcpiiu.cpp:1463-1478`) bounds the
        // requested element count against what the circuit can carry
        // (ECA_TOLARGE past it), then substitutes the channel's native count
        // for a zero (autosize) request when the peer predates CA_V413 —
        // those servers have no zero-count autosize contract and would read
        // `m_count == 0` as "no elements".
        let count = crate::protocol::read_notify_wire_count(
            count,
            snap.element_count,
            data_type,
            snap.server_minor,
        )?;
        if let Some(writer) = self.direct_writer(snap.server_addr) {
            return writer.send_frame(Self::build_read_notify_frame(
                snap.sid,
                data_type,
                count,
                ioid,
                snap.server_minor,
            )?);
        }

        self.transport_tx
            .send(TransportCommand::ReadNotify {
                sid: snap.sid,
                data_type,
                count,
                ioid,
                server_addr: snap.server_addr,
                priority: self.priority,
            })
            .map_err(|_| CaError::Shutdown)
    }

    /// Send a `CA_PROTO_WRITE_NOTIFY`. `data_type` is the over-the-wire
    /// DBR type — usually the channel native type, but the typed
    /// string-put path passes `DBR_STRING` (0) regardless of native
    /// type so the server resolves the value (e.g. ENUM menu strings).
    fn send_write_notify_fast(
        &self,
        snap: &ChannelSnapshotPublic,
        data_type: u16,
        count: u32,
        ioid: u32,
        payload: Vec<u8>,
    ) -> CaResult<()> {
        // C `libca/nciu.cpp::write()` rejects before queueing
        // when `!accessRightState.writePermit()` (ECA_NOWTACCESS).
        if !snap.access_rights.write {
            return Err(CaError::Protocol(format!(
                "write denied by cached access rights (matches libca \
                 nciu::write ECA_NOWTACCESS); ioid {ioid}"
            )));
        }
        // C `comQueSend::insertRequestWithPayLoad` refuses a DBR type past
        // `INVALID_DB_REQ` (`comQueSend.cpp:323`, → ECA_BADTYPE) and an array
        // past the peer's message-body limit (`:352-364`, → ECA_BADCOUNT),
        // both before a byte is queued. Gate here, ahead of the
        // direct-writer / coordinator split, so neither path can put an
        // unframeable request on the wire — and so a caller-chosen type from
        // `put_as_dbr_*` is answered locally instead of costing the circuit
        // the server would drop it on.
        crate::protocol::check_write_request(count, data_type, snap.server_minor)?;
        if let Some(writer) = self.direct_writer(snap.server_addr) {
            return writer.send_frame(Self::build_write_frame(
                CA_PROTO_WRITE_NOTIFY,
                snap.sid,
                data_type,
                count,
                Some(ioid),
                payload,
                snap.server_minor,
            )?);
        }

        self.transport_tx
            .send(TransportCommand::WriteNotify {
                sid: snap.sid,
                data_type,
                count,
                ioid,
                payload,
                server_addr: snap.server_addr,
                priority: self.priority,
            })
            .map_err(|_| CaError::Shutdown)
    }

    /// Send a fire-and-forget `CA_PROTO_WRITE`. See
    /// [`Self::send_write_notify_fast`] for the `data_type` contract.
    fn send_write_nowait_fast(
        &self,
        snap: &ChannelSnapshotPublic,
        data_type: u16,
        count: u32,
        payload: Vec<u8>,
    ) -> CaResult<()> {
        // cached-access write gate (see send_write_notify_fast).
        // The nowait variant otherwise returned Ok(()) after putting
        // an oversized / forbidden request on the wire.
        if !snap.access_rights.write {
            return Err(CaError::Protocol(
                "write denied by cached access rights (matches libca \
                 nciu::write ECA_NOWTACCESS)"
                    .into(),
            ));
        }
        // Same pre-queue type and element bounds as `send_write_notify_fast`
        // — a fire-and-forget put goes through the identical
        // `comQueSend::insertRequestWithPayLoad`, so `ca_array_put` returns
        // ECA_BADTYPE / ECA_BADCOUNT synchronously rather than sending.
        crate::protocol::check_write_request(count, data_type, snap.server_minor)?;
        // Bind the identity to the request before the request exists on the
        // wire, at the one site both routes pass through. A plain write has
        // no ioid and no completion, so this record is the only thing that
        // survives to tell a later `CA_PROTO_ERROR` what it is about; libca
        // gets the same answer from the `nciu` the write was issued through.
        self.in_flight
            .write_identities
            .insert(self.cid, Arc::clone(&self.pv_name));
        if let Some(writer) = self.direct_writer(snap.server_addr) {
            return writer.send_frame(Self::build_write_frame(
                CA_PROTO_WRITE,
                snap.sid,
                data_type,
                count,
                Some(self.cid),
                payload,
                snap.server_minor,
            )?);
        }

        self.transport_tx
            .send(TransportCommand::Write {
                sid: snap.sid,
                cid: self.cid,
                data_type,
                count,
                payload,
                server_addr: snap.server_addr,
                priority: self.priority,
            })
            .map_err(|_| CaError::Shutdown)
    }

    pub async fn get(&self) -> CaResult<(DbFieldType, EpicsValue)> {
        self.get_with_timeout(Duration::from_secs(30)).await
    }

    pub async fn get_many(channels: &[CaChannel]) -> Vec<CaResult<(DbFieldType, EpicsValue)>> {
        Self::get_many_with_timeout(channels, Duration::from_secs(30)).await
    }

    pub async fn get_many_with_timeout(
        channels: &[CaChannel],
        timeout: Duration,
    ) -> Vec<CaResult<(DbFieldType, EpicsValue)>> {
        // Per-PV state. Cold path = first call on a channel; allocates
        // a fresh ioid + ReadWaiter::OneShot. Warm path = subsequent
        // calls reuse the channel's CachedRead (persistent ioid +
        // ReadWaiter::Warm + reusable Sender slot) — no `alloc_ioid`,
        // no DashMap insert, dispatcher uses a read-locked `get`
        // instead of a write-locked `remove`. Mirrors PVA `pvget_many`
        // (epics-pva-rs `client_native/context.rs`).
        enum PendingKind {
            Cold {
                ioid: u32,
                in_flight: InFlightOps,
                /// Channel state captured at call-time; used to install
                /// a fresh CachedRead after the cold response succeeds.
                cid: u32,
                cached_read_slot: Arc<Mutex<Option<CachedRead>>>,
                sid: u32,
                server_addr: SocketAddr,
                data_type: u16,
                element_count: u32,
            },
            Warm {
                ioid: u32,
                in_flight: InFlightOps,
                cached_read_slot: Arc<Mutex<Option<CachedRead>>>,
                /// Borrowed cache entry — restored on success, evicted
                /// on timeout/shutdown so the next call starts cold.
                cached: CachedRead,
            },
        }

        struct Pending {
            index: usize,
            reply_rx: oneshot::Receiver<CaResult<ReadReply>>,
            kind: PendingKind,
        }

        struct BulkReadGroup {
            writer: DirectServerWriter,
            frame: Vec<u8>,
            pending: Vec<Pending>,
        }

        let mut results: Vec<Option<CaResult<(DbFieldType, EpicsValue)>>> =
            (0..channels.len()).map(|_| None).collect();
        // batch by circuit `(server_addr, priority)`, not by
        // server alone — channels to the same IOC at different
        // priorities ride independent circuits with independent writers,
        // so their READ_NOTIFY frames must not be coalesced onto one.
        let mut groups: HashMap<types::CircuitKey, BulkReadGroup> = HashMap::new();
        let mut pending: Vec<Pending> = Vec::new();

        for (index, ch) in channels.iter().enumerate() {
            let snap = match ch.snapshot() {
                Ok(s) => s,
                Err(e) => {
                    results[index] = Some(Err(e));
                    continue;
                }
            };

            // Warm-path attempt: take the cached entry iff it matches
            // the live snapshot. A mismatch (reconnect ⇒ new sid /
            // server, or DB record edit ⇒ new native type / count)
            // evicts both the channel-side cached_read and the
            // dispatcher-side DashMap entry; the next call falls
            // through to cold and re-populates from scratch.
            let warm_taken: Option<CachedRead> = {
                let mut guard = ch.cached_read.lock();
                let matches = matches!(guard.as_ref(), Some(c)
                    if c.server_addr == snap.server_addr
                        && c.sid == snap.sid
                        && c.data_type == snap.native_type as u16
                        && c.element_count == snap.element_count);
                if matches {
                    guard.take()
                } else if guard.is_some() {
                    let stale = guard.take().unwrap();
                    ch.in_flight.reads.remove(&stale.ioid);
                    None
                } else {
                    None
                }
            };

            let (reply_tx, reply_rx) = oneshot::channel();
            let (frame, kind) = if let Some(cached) = warm_taken {
                // Refill the reusable Sender slot. The dispatcher takes
                // this on response without removing the DashMap entry.
                *cached.slot.lock() = Some(reply_tx);
                let count = match crate::protocol::read_notify_wire_count(
                    cached.element_count,
                    cached.element_count,
                    cached.data_type,
                    snap.server_minor,
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        ch.in_flight.reads.remove(&cached.ioid);
                        results[index] = Some(Err(e));
                        continue;
                    }
                };
                let frame = match Self::build_read_notify_frame(
                    cached.sid,
                    cached.data_type,
                    count,
                    cached.ioid,
                    snap.server_minor,
                ) {
                    Ok(f) => f,
                    Err(e) => {
                        // Unframeable for this peer — drop the borrowed
                        // warm entry so the next call starts cold, and
                        // fail locally without touching the wire.
                        ch.in_flight.reads.remove(&cached.ioid);
                        results[index] = Some(Err(e));
                        continue;
                    }
                };
                let kind = PendingKind::Warm {
                    ioid: cached.ioid,
                    in_flight: ch.in_flight.clone(),
                    cached_read_slot: ch.cached_read.clone(),
                    cached,
                };
                (frame, kind)
            } else {
                let ioid = ch.in_flight.alloc_ioid();
                ch.in_flight.reads.insert(
                    ioid,
                    ReadWaiter::OneShot {
                        cid: ch.cid,
                        mode: ReadReplyMode::Plain,
                        reply_tx,
                    },
                );
                let count = match crate::protocol::read_notify_wire_count(
                    snap.element_count,
                    snap.element_count,
                    snap.native_type as u16,
                    snap.server_minor,
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        ch.in_flight.reads.remove(&ioid);
                        results[index] = Some(Err(e));
                        continue;
                    }
                };
                let frame = match Self::build_read_notify_frame(
                    snap.sid,
                    snap.native_type as u16,
                    count,
                    ioid,
                    snap.server_minor,
                ) {
                    Ok(f) => f,
                    Err(e) => {
                        ch.in_flight.reads.remove(&ioid);
                        results[index] = Some(Err(e));
                        continue;
                    }
                };
                let kind = PendingKind::Cold {
                    ioid,
                    in_flight: ch.in_flight.clone(),
                    cid: ch.cid,
                    cached_read_slot: ch.cached_read.clone(),
                    sid: snap.sid,
                    server_addr: snap.server_addr,
                    data_type: snap.native_type as u16,
                    element_count: snap.element_count,
                };
                (frame, kind)
            };

            let pending_read = Pending {
                index,
                reply_rx,
                kind,
            };

            if let Some(writer) = ch.direct_writer(snap.server_addr) {
                let group = groups
                    .entry((snap.server_addr, ch.priority))
                    .or_insert_with(|| BulkReadGroup {
                        writer,
                        frame: Vec::new(),
                        pending: Vec::new(),
                    });
                group.frame.extend_from_slice(&frame);
                group.pending.push(pending_read);
            } else {
                // No direct writer ⇒ fall back to the transport-mediated
                // send. Cold path can use it; warm path needs a live
                // direct writer (the cached server is gone), so we
                // evict the warm entry and surface Disconnected.
                match pending_read.kind {
                    PendingKind::Cold {
                        ioid, in_flight, ..
                    } => match ch.send_read_notify_fast(
                        &snap,
                        snap.native_type as u16,
                        snap.element_count,
                        ioid,
                    ) {
                        Ok(()) => pending.push(Pending {
                            index,
                            reply_rx: pending_read.reply_rx,
                            kind: PendingKind::Cold {
                                ioid,
                                in_flight,
                                cid: ch.cid,
                                cached_read_slot: ch.cached_read.clone(),
                                sid: snap.sid,
                                server_addr: snap.server_addr,
                                data_type: snap.native_type as u16,
                                element_count: snap.element_count,
                            },
                        }),
                        Err(e) => {
                            in_flight.reads.remove(&ioid);
                            results[index] = Some(Err(e));
                        }
                    },
                    PendingKind::Warm {
                        ioid,
                        in_flight,
                        cached_read_slot,
                        ..
                    } => {
                        in_flight.reads.remove(&ioid);
                        *cached_read_slot.lock() = None;
                        results[index] = Some(Err(CaError::Disconnected));
                    }
                }
            }
        }

        for (_, group) in groups {
            match group.writer.send_frame(group.frame) {
                Ok(()) => pending.extend(group.pending),
                Err(_) => {
                    for p in group.pending {
                        match p.kind {
                            PendingKind::Cold {
                                ioid, in_flight, ..
                            } => {
                                in_flight.reads.remove(&ioid);
                            }
                            PendingKind::Warm {
                                ioid,
                                in_flight,
                                cached_read_slot,
                                ..
                            } => {
                                in_flight.reads.remove(&ioid);
                                *cached_read_slot.lock() = None;
                            }
                        }
                        results[p.index] = Some(Err(CaError::Disconnected));
                    }
                }
            }
        }

        // Sequential drain (mirrors PVA `pvget_many`): the read_loop
        // dispatches all per-server responses back-to-back as the burst
        // arrives, so most rx's are already ready by the time we reach
        // them. Sequential await over ready oneshots is cheap (poll
        // returns immediately, no scheduler hop) and avoids the
        // FuturesUnordered bookkeeping (~50ns per item × 100 items
        // ≈ 5µs). Out-of-order arrival is rare on a single TCP circuit
        // because CA responses are emitted in request order.
        //
        // Phase 2: scalar plain reads are decoded in the read loop, so
        // the hot path avoids allocating/copying one payload Vec per PV.

        let deadline = epics_base_rs::runtime::task::Instant::now() + timeout;
        for p in pending {
            let Pending {
                index,
                reply_rx,
                kind,
            } = p;
            let result = epics_base_rs::runtime::task::timeout_at(deadline, reply_rx).await;
            let decoded: CaResult<(DbFieldType, EpicsValue)> = match result {
                Ok(Ok(Ok(reply))) => Self::decode_plain_read_reply(reply),
                Ok(Ok(Err(e))) => Err(e),
                Ok(Err(_)) => Err(CaError::Shutdown),
                Err(_) => Err(CaError::Timeout),
            };
            let is_local_error = matches!(decoded, Err(CaError::Timeout) | Err(CaError::Shutdown));

            match kind {
                PendingKind::Cold {
                    ioid,
                    in_flight,
                    cid,
                    cached_read_slot,
                    sid,
                    server_addr,
                    data_type,
                    element_count,
                } => {
                    if is_local_error {
                        // read_loop hasn't dispatched (no Warm shortcut
                        // for cold), so we sweep the OneShot entry now.
                        in_flight.reads.remove(&ioid);
                    }
                    if decoded.is_ok() {
                        // First successful default GET ⇒ install warm
                        // cache. Allocate a fresh ioid (separate from
                        // the cold one which read_loop already removed)
                        // and register a persistent Warm waiter. Loser
                        // of any concurrent install race drops its
                        // entry to avoid a leaked DashMap row.
                        let warm_ioid = in_flight.alloc_ioid();
                        let slot: types::WarmReplySlot = Arc::new(parking_lot::Mutex::new(None));
                        in_flight.reads.insert(
                            warm_ioid,
                            ReadWaiter::Warm {
                                cid,
                                mode: ReadReplyMode::Plain,
                                slot: slot.clone(),
                            },
                        );
                        let cached = CachedRead {
                            ioid: warm_ioid,
                            sid,
                            server_addr,
                            data_type,
                            element_count,
                            slot,
                        };
                        let mut guard = cached_read_slot.lock();
                        if guard.is_none() {
                            *guard = Some(cached);
                        } else {
                            drop(guard);
                            in_flight.reads.remove(&warm_ioid);
                        }
                    }
                }
                PendingKind::Warm {
                    ioid,
                    in_flight,
                    cached_read_slot,
                    cached,
                } => {
                    if is_local_error {
                        // Stale cache: drop the entry so the next call
                        // can re-establish from cold. The DashMap entry
                        // here might still receive a late server
                        // response, which the dispatcher will discard.
                        in_flight.reads.remove(&ioid);
                        drop(cached);
                        *cached_read_slot.lock() = None;
                    } else {
                        // Success or server-side error: keep the cache.
                        // Restore the borrowed CachedRead. If a racing
                        // install already populated the slot (e.g.,
                        // duplicate channel in input list), drop ours
                        // and evict its DashMap entry.
                        let mut guard = cached_read_slot.lock();
                        if guard.is_none() {
                            *guard = Some(cached);
                        } else {
                            drop(guard);
                            in_flight.reads.remove(&ioid);
                        }
                    }
                }
            }

            results[index] = Some(decoded);
        }

        results
            .into_iter()
            .map(|r| r.unwrap_or(Err(CaError::Shutdown)))
            .collect()
    }

    /// The channel's native field type, known once connected. Mirrors
    /// libca `ca_field_type(chid)` — the CLI tools read it after connect
    /// to decide the readback DBR type (e.g. ENUM → STRING by default,
    /// see [`enum_string_readback_dbr`]). Returns `Err(Disconnected)`
    /// before the channel is operational.
    pub fn native_field_type(&self) -> CaResult<DbFieldType> {
        Ok(self.snapshot()?.native_type)
    }

    /// The channel's native element count, known once connected. Mirrors
    /// libca `ca_element_count(chid)` — the CLI tools read it after connect
    /// to clamp a `-#` requested element count to the array bound before
    /// issuing the request (C `caget.c:200` / `camonitor.c:169`). Returns
    /// `Err(Disconnected)` before the channel is operational.
    pub fn element_count(&self) -> CaResult<u32> {
        Ok(self.snapshot()?.element_count)
    }

    pub async fn get_with_timeout(&self, timeout: Duration) -> CaResult<(DbFieldType, EpicsValue)> {
        self.get_with_timeout_count(timeout, 0).await
    }

    /// Plain (no-metadata) GET requesting `count` array elements. `count`
    /// accepts a bare `u32` ([`ReqCount::Fixed`], where `0` resolves to the
    /// full native count) or [`ReqCount::Autosize`] (callback mode, where
    /// `0` is sent verbatim as the CA autosize request). The count-aware
    /// sibling of [`Self::get_with_timeout`], mirroring the
    /// metadata/exact-type GET paths ([`Self::get_with_metadata_count`],
    /// [`Self::get_with_dbr_type`]) so `caget -# N` bounds the CA payload at
    /// the request boundary instead of transferring the whole array and
    /// truncating in rendering (C `caget.c:200-215` applies `reqElems` to
    /// the wire request count).
    ///
    /// A resolved count greater than the channel element count is rejected
    /// as `ECA_BADCOUNT`, matching libca `nciu::read`; the tool front-end
    /// clamps to the native count first (C `caget.c:200`
    /// `reqElems > nElems ? nElems : reqElems`), so this guards only
    /// programmatic misuse.
    pub async fn get_with_timeout_count(
        &self,
        timeout: Duration,
        count: impl Into<ReqCount>,
    ) -> CaResult<(DbFieldType, EpicsValue)> {
        let snap = self.snapshot()?;
        let request_count = count.into().resolve(snap.element_count);
        if request_count > snap.element_count {
            return Err(CaError::Protocol(format!(
                "get count {} exceeds channel element count {} \
                 (matches libca nciu::read ECA_BADCOUNT)",
                request_count, snap.element_count
            )));
        }

        let ioid = self.in_flight.alloc_ioid();
        let (reply_tx, reply_rx) = oneshot::channel();
        // Direct registry insert (Option C Phase A) — bypasses the
        // coordinator. `transport::read_loop` removes the entry and
        // fulfils the oneshot when CA_PROTO_READ_NOTIFY arrives.
        // Drop semantics: if the caller drops the future before the
        // response arrives, `reply_rx` drops, the registry's stored
        // sender becomes a zombie until either (a) the response
        // arrives and we send to a dead receiver (no-op), or
        // (b) disconnect cleanup drains it (Phase D). Bounded
        // either way; not a leak.
        self.in_flight.reads.insert(
            ioid,
            ReadWaiter::OneShot {
                cid: self.cid,
                mode: ReadReplyMode::Plain,
                reply_tx,
            },
        );

        if let Err(e) =
            self.send_read_notify_fast(&snap, snap.native_type as u16, request_count, ioid)
        {
            self.in_flight.reads.remove(&ioid);
            return Err(e);
        }

        let result = epics_base_rs::runtime::task::timeout(timeout, reply_rx).await;
        // Always remove the registry entry when control returns —
        // covers the timeout path (response would never arrive) and
        // the success path (already removed by read_loop, the
        // `remove` is a no-op then). `drop` of `reply_rx` happens
        // implicitly on the success path.
        self.in_flight.reads.remove(&ioid);
        let reply = result
            .map_err(|_| CaError::Timeout)?
            .map_err(|_| CaError::Shutdown)??;
        Self::decode_plain_read_reply(reply)
    }

    /// Get a PV value with metadata. Use `DbrClass::Time` for timestamp + alarm,
    /// or `DbrClass::Ctrl` for full control metadata (units, limits, precision).
    /// Pass `count` to limit the number of array elements (0 = all).
    pub async fn get_with_metadata(&self, class: DbrClass) -> CaResult<Snapshot> {
        self.get_with_metadata_count(class, 0).await
    }

    /// Get a PV value with metadata, requesting at most `count` elements.
    /// Pass 0 for the full element count.
    ///
    /// The wire request type is the `*_<native>` member of `class` —
    /// e.g. `DbrClass::Time` on a DOUBLE PV requests `DBR_TIME_DOUBLE`.
    /// This is the right behaviour for monitors and `caget -a`, where C
    /// re-derives the value type from the channel's native type
    /// (`caget.c:175`, `format != specifiedDbr`). For `caget -d`'s
    /// *exact* type request, use [`Self::get_with_dbr_type`] instead.
    pub async fn get_with_metadata_count(
        &self,
        class: DbrClass,
        count: impl Into<ReqCount>,
    ) -> CaResult<Snapshot> {
        let snap = self.snapshot()?;

        let native = DbFieldType::from_u16(snap.native_type as u16)?;
        // `from_u16` only yields the six CA wire types (0..6), so `native`
        // is never `Int64` on this path; every arm below is therefore
        // correct as written. The `Sts/Time/Ctrl/Gr` helpers route
        // through `ca_wire_type()` (which would remap an `Int64` to the
        // `*_DOUBLE` family); the `Plain` arm's `to_dbr_type()` is an
        // identity map and relies solely on `native` being wire-bounded.
        // The helpers are used for consistency, not for Int64 safety.
        let request_type = match class {
            DbrClass::Time => native.time_dbr_type(),
            DbrClass::Ctrl => native.ctrl_dbr_type(),
            DbrClass::Sts => native.sts_dbr_type(),
            DbrClass::Gr => native.gr_dbr_type(),
            DbrClass::Plain => native.to_dbr_type() as u16,
        };

        self.get_dbr_request(snap, request_type, count.into()).await
    }

    /// Get a PV value requesting an **exact** DBR type code (`0..=38`),
    /// sent verbatim over the wire without re-deriving the value type
    /// from the channel's native type.
    ///
    /// This mirrors C `caget -d`'s `pvs[n].dbrType = dbrType`
    /// (`caget.c:172`): `-d DBR_TIME_FLOAT` on a DOUBLE PV requests
    /// `DBR_TIME_FLOAT` (16) — the server converts and returns a float —
    /// rather than the native `DBR_TIME_DOUBLE` (20) that the class-based
    /// [`Self::get_with_metadata_count`] would derive. It also reaches
    /// the type codes that have no value-class analogue, e.g.
    /// `DBR_STSACK_STRING` (37) and `DBR_CLASS_NAME` (38).
    ///
    /// A code past `LAST_BUFFER_TYPE` is refused here with
    /// `CaError::UnsupportedType` (`ECA_BADTYPE`) and never reaches the
    /// wire, as libca's `nciu::read` (`nciu.cpp:292`) does — the server
    /// treats such a type as a protocol violation and drops the circuit,
    /// so the request must not leave the client.
    pub async fn get_with_dbr_type(
        &self,
        dbr_type: u16,
        count: impl Into<ReqCount>,
    ) -> CaResult<Snapshot> {
        let snap = self.snapshot()?;
        self.get_dbr_request(snap, dbr_type, count.into()).await
    }

    /// Shared one-shot READ_NOTIFY body for the metadata/exact-type GET
    /// paths: validate the requested element count, issue the request
    /// with `request_type` verbatim, await the reply, and decode it.
    async fn get_dbr_request(
        &self,
        snap: ChannelSnapshotPublic,
        request_type: u16,
        count: ReqCount,
    ) -> CaResult<Snapshot> {
        // Resolve the request mode (`Fixed(0)` → native count; `Autosize`
        // passes its count through, including the `0` autosize request).
        let request_count = count.resolve(snap.element_count);
        // C `libca/nciu.cpp::read()` rejects `countIn > this->count` with
        // ECA_BADCOUNT before queueing the read. Pre-fix Rust silently
        // clamped with `.min(snap.element_count)`, so a caller bug
        // requesting 100 elements from a 10-element PV got a 10-element
        // response from Rust where libca would have returned ECA_BADCOUNT
        // without sending anything. Validate the resolved count up front;
        // `0` (native or autosize) is always in range.
        if request_count > snap.element_count {
            return Err(CaError::Protocol(format!(
                "get count {} exceeds channel element count {} \
                 (matches libca nciu::read ECA_BADCOUNT)",
                request_count, snap.element_count
            )));
        }

        let ioid = self.in_flight.alloc_ioid();
        let (reply_tx, reply_rx) = oneshot::channel();
        // Direct registry insert (Option C Phase A); see ch.get
        // for drop-semantics commentary.
        self.in_flight.reads.insert(
            ioid,
            ReadWaiter::OneShot {
                cid: self.cid,
                mode: ReadReplyMode::Raw,
                reply_tx,
            },
        );

        if let Err(e) = self.send_read_notify_fast(&snap, request_type, request_count, ioid) {
            self.in_flight.reads.remove(&ioid);
            return Err(e);
        }

        let result = epics_base_rs::runtime::task::timeout(Duration::from_secs(30), reply_rx).await;
        self.in_flight.reads.remove(&ioid);
        let reply = result
            .map_err(|_| CaError::Timeout)?
            .map_err(|_| CaError::Shutdown)??;

        match reply {
            ReadReply::Raw {
                data_type,
                count,
                data,
            } => decode_dbr(data_type, &data, count as usize),
            ReadReply::Plain { .. } => Err(CaError::Protocol(
                "metadata read returned a plain scalar reply".into(),
            )),
        }
    }

    pub async fn put(&self, value: &EpicsValue) -> CaResult<()> {
        let snap = self.snapshot()?;
        // The frame is stamped with the channel's NATIVE type below, so
        // the payload must be the native encoding. A caller variant that
        // differs (e.g. `Long` on an ENUM field) would otherwise ship a
        // native-typed header over foreign bytes, and the server —
        // which decodes strictly against the header type — would read
        // the first two bytes of the big-endian i32 as the enum index:
        // `Long(1)` landed as 0, silently. `convert_to` is the single
        // value-coercion owner (C cast semantics), so the header/payload
        // pairing holds for every variant. Menu labels still go through
        // `put_string`, which the server resolves against its menu.
        let value = value.convert_to(snap.native_type);
        // C `nciu::write` rejects countIn > channel count with
        // ECA_BADCOUNT before queueing the request. Pre-fix Rust sent
        // an oversized write that the server would either accept past
        // its array bound or reject asynchronously. Validated on the
        // converted value — the count of the actual wire request, which
        // conversion can change (String → CHAR-array on a CHAR field).
        validate_put_count(&snap, value.count())?;
        validate_put_strings(&value)?;

        let ioid = self.in_flight.alloc_ioid();
        let (reply_tx, reply_rx) = oneshot::channel();
        // Direct registry insert (Option C Phase A).
        self.in_flight.writes.insert(ioid, (self.cid, reply_tx));

        let payload = value.to_bytes();
        let count = value.count() as u32;
        if let Err(e) =
            self.send_write_notify_fast(&snap, snap.native_type as u16, count, ioid, payload)
        {
            self.in_flight.writes.remove(&ioid);
            return Err(e);
        }

        let result = epics_base_rs::runtime::task::timeout(put_timeout(), reply_rx).await;
        self.in_flight.writes.remove(&ioid);
        result
            .map_err(|_| CaError::Timeout)?
            .map_err(|_| CaError::Shutdown)?
    }

    /// Write with completion callback and configurable timeout.
    pub async fn put_with_timeout(&self, value: &EpicsValue, timeout: Duration) -> CaResult<()> {
        let snap = self.snapshot()?;
        // Native encoding to match the native-typed header; see `put`.
        let value = value.convert_to(snap.native_type);
        validate_put_count(&snap, value.count())?;
        validate_put_strings(&value)?;

        let ioid = self.in_flight.alloc_ioid();
        let (reply_tx, reply_rx) = oneshot::channel();
        // Direct registry insert (Option C Phase A).
        self.in_flight.writes.insert(ioid, (self.cid, reply_tx));

        let payload = value.to_bytes();
        let count = value.count() as u32;
        if let Err(e) =
            self.send_write_notify_fast(&snap, snap.native_type as u16, count, ioid, payload)
        {
            self.in_flight.writes.remove(&ioid);
            return Err(e);
        }

        let result = epics_base_rs::runtime::task::timeout(timeout, reply_rx).await;
        self.in_flight.writes.remove(&ioid);
        result
            .map_err(|_| CaError::Timeout)?
            .map_err(|_| CaError::Shutdown)?
    }

    /// Fire-and-forget put (CA_PROTO_WRITE). Returns immediately without
    /// waiting for server acknowledgement. Used by ophyd's EpicsMotor.set()
    /// which monitors DMOV for completion instead.
    pub async fn put_nowait(&self, value: &EpicsValue) -> CaResult<()> {
        let snap = self.snapshot()?;
        // Native encoding to match the native-typed header; see `put`.
        let value = value.convert_to(snap.native_type);
        validate_put_count(&snap, value.count())?;
        validate_put_strings(&value)?;

        let payload = value.to_bytes();
        let count = value.count() as u32;
        self.send_write_nowait_fast(&snap, snap.native_type as u16, count, payload)
    }

    /// Write `value`'s payload under an explicit DBR wire type
    /// (`dbr_type`) rather than the channel's native type, waiting for the
    /// completion callback with `timeout`.
    ///
    /// This is the C `caput` write model: the tool — not the channel —
    /// selects `dbrType` (`DBR_STRING` for CLI string/number conversion,
    /// `DBR_CHAR` for `-S` long strings) and hands the matching buffer to
    /// `ca_array_put_callback(dbrType, count, chid, pbuf)`
    /// (`caput.c:556-563`); the server then converts to the native field
    /// type. `count` and the payload come from `value`, so an
    /// `EpicsValue::String` writes one 40-byte `DBR_STRING` and an
    /// `EpicsValue::CharArray` writes `bytes.len()` `DBR_CHAR`.
    ///
    /// Distinct from [`Self::put_with_timeout`], which forces the channel
    /// native type and is the programmatic native-typed write API. Use
    /// this only for C-tool-parity paths that must control the wire type.
    ///
    /// A `dbr_type` past `LAST_BUFFER_TYPE` is refused here with
    /// `CaError::UnsupportedType` (`ECA_BADTYPE`) and never reaches the
    /// wire, matching `comQueSend::insertRequestWithPayLoad`
    /// (`comQueSend.cpp:323`).
    pub async fn put_as_dbr_with_timeout(
        &self,
        dbr_type: u16,
        value: &EpicsValue,
        timeout: Duration,
    ) -> CaResult<()> {
        let snap = self.snapshot()?;
        let count = value.count() as u32;
        validate_put_count(&snap, count)?;
        validate_put_strings(value)?;

        let ioid = self.in_flight.alloc_ioid();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.in_flight.writes.insert(ioid, (self.cid, reply_tx));

        let payload = value.to_bytes();
        if let Err(e) = self.send_write_notify_fast(&snap, dbr_type, count, ioid, payload) {
            self.in_flight.writes.remove(&ioid);
            return Err(e);
        }

        let result = epics_base_rs::runtime::task::timeout(timeout, reply_rx).await;
        self.in_flight.writes.remove(&ioid);
        result
            .map_err(|_| CaError::Timeout)?
            .map_err(|_| CaError::Shutdown)?
    }

    /// Fire-and-forget variant of [`Self::put_as_dbr_with_timeout`] —
    /// sends the payload under `dbr_type` via `CA_PROTO_WRITE` without
    /// waiting for server acknowledgement.
    pub async fn put_as_dbr_nowait(&self, dbr_type: u16, value: &EpicsValue) -> CaResult<()> {
        let snap = self.snapshot()?;
        let count = value.count() as u32;
        validate_put_count(&snap, count)?;
        validate_put_strings(value)?;
        let payload = value.to_bytes();
        self.send_write_nowait_fast(&snap, dbr_type, count, payload)
    }

    /// Write a string value to the channel as `DBR_STRING` (DBR type 0)
    /// regardless of the channel's native type, and wait for the
    /// completion callback.
    ///
    /// This mirrors C `caput`'s ENUM handling (`caput.c:485-510`): a
    /// `DBR_ENUM` channel is written with the value as a `DBR_STRING`
    /// and the **server** resolves the menu string. That is the only
    /// way to write a site-custom ENUM by name — the client has no
    /// access to the IOC's menu definitions. Plain integer ENUM
    /// indices should still go through [`Self::put`] with
    /// [`EpicsValue::Enum`].
    ///
    /// Note: a CA `DBR_STRING` is a fixed 40-byte field, so a `value` of 40
    /// bytes or more is REJECTED (`ECA_STRTOBIG`), not truncated — libca
    /// `nciu::stringVerify` refuses it before the request is queued (R6-29).
    /// ENUM menu names are well within the 39-byte cap.
    pub async fn put_string(&self, value: &str) -> CaResult<()> {
        let snap = self.snapshot()?;
        validate_put_count(&snap, 1)?;
        validate_string_length(value)?;

        let ioid = self.in_flight.alloc_ioid();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.in_flight.writes.insert(ioid, (self.cid, reply_tx));

        let payload = EpicsValue::String(value.into()).to_bytes();
        // DBR_STRING = 0; count 1. The server interprets the bytes
        // against its own field type (e.g. ENUM menu resolution).
        if let Err(e) = self.send_write_notify_fast(&snap, 0, 1, ioid, payload) {
            self.in_flight.writes.remove(&ioid);
            return Err(e);
        }

        let result = epics_base_rs::runtime::task::timeout(put_timeout(), reply_rx).await;
        self.in_flight.writes.remove(&ioid);
        result
            .map_err(|_| CaError::Timeout)?
            .map_err(|_| CaError::Shutdown)?
    }

    /// Fire-and-forget variant of [`Self::put_string`] — sends the
    /// value as `DBR_STRING` via `CA_PROTO_WRITE` without waiting for
    /// server acknowledgement.
    pub async fn put_string_nowait(&self, value: &str) -> CaResult<()> {
        let snap = self.snapshot()?;
        validate_put_count(&snap, 1)?;
        validate_string_length(value)?;
        let payload = EpicsValue::String(value.into()).to_bytes();
        self.send_write_nowait_fast(&snap, 0, 1, payload)
    }

    /// Array variant of [`Self::put_string`] — writes `values` as a
    /// `DBR_STRING` array (`count = values.len()`) regardless of the
    /// channel's native type, and waits for the completion callback.
    ///
    /// This is the ENUM-waveform-by-name path: C `caput -a` on a
    /// `DBR_ENUM` waveform writes each element as a `DBR_STRING` and the
    /// **server** resolves each menu string. Each element is subject to the
    /// same 39-byte cap as [`Self::put_string`]: an over-long element is
    /// rejected with `ECA_STRTOBIG`, not truncated.
    pub async fn put_string_array(&self, values: &[String]) -> CaResult<()> {
        let snap = self.snapshot()?;
        validate_put_count(&snap, values.len() as u32)?;
        for s in values {
            validate_string_length(s)?;
        }

        let ioid = self.in_flight.alloc_ioid();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.in_flight.writes.insert(ioid, (self.cid, reply_tx));

        let payload =
            EpicsValue::StringArray(values.iter().map(PvString::from).collect()).to_bytes();
        let count = values.len() as u32;
        // DBR_STRING = 0. The server interprets each 40-byte element
        // against its own field type (e.g. ENUM menu resolution).
        if let Err(e) = self.send_write_notify_fast(&snap, 0, count, ioid, payload) {
            self.in_flight.writes.remove(&ioid);
            return Err(e);
        }

        let result = epics_base_rs::runtime::task::timeout(put_timeout(), reply_rx).await;
        self.in_flight.writes.remove(&ioid);
        result
            .map_err(|_| CaError::Timeout)?
            .map_err(|_| CaError::Shutdown)?
    }

    /// Fire-and-forget variant of [`Self::put_string_array`].
    pub async fn put_string_array_nowait(&self, values: &[String]) -> CaResult<()> {
        let snap = self.snapshot()?;
        validate_put_count(&snap, values.len() as u32)?;
        for s in values {
            validate_string_length(s)?;
        }
        let payload =
            EpicsValue::StringArray(values.iter().map(PvString::from).collect()).to_bytes();
        let count = values.len() as u32;
        self.send_write_nowait_fast(&snap, 0, count, payload)
    }

    pub async fn subscribe(&self) -> CaResult<MonitorHandle> {
        self.subscribe_with_deadband(0.0).await
    }

    /// Subscribe with client-side deadband filtering.
    /// Events where |new - old| < deadband are suppressed (scalar values only).
    pub async fn subscribe_with_deadband(&self, deadband: f64) -> CaResult<MonitorHandle> {
        self.subscribe_with_mask(deadband, DBE_VALUE | DBE_LOG | DBE_ALARM)
            .await
    }

    /// Subscribe with an explicit `DBE_*` event mask (`camonitor
    /// -m`). The server queues an EVENT only when `mask & post_select`
    /// is non-zero, so e.g. `DBE_ALARM` alone delivers only alarm
    /// transitions. `deadband` applies client-side deadband filtering as
    /// in [`Self::subscribe_with_deadband`].
    ///
    /// No element-count cap is given, so the wire request uses CA autosize
    /// (`count = 0`): the server reports each event at the record's current
    /// element count rather than its native capacity — the standard CA-tool
    /// default (`camonitor.c:168-169`). For an explicit `-#` cap use
    /// [`Self::subscribe_with_mask_readback_count`].
    ///
    /// ENUM fields are monitored in their native type (numeric index).
    /// For the `camonitor` state-label default use
    /// [`Self::subscribe_with_mask_enum_as_string`].
    pub async fn subscribe_with_mask(&self, deadband: f64, mask: u16) -> CaResult<MonitorHandle> {
        self.subscribe_with_mask_enum_as_string(deadband, mask, false)
            .await
    }

    /// Subscribe with an explicit `DBE_*` event mask and CA *autosize*
    /// element count (`count = 0` on the wire), the count-zero subscription
    /// standard CA tools and ca-gateway use (`gatePv.cc:765-774`
    /// `ca_create_subscription(eventType(), 0, chID, GR->eventMask(), ...)`).
    ///
    /// The server reports the record's CURRENT element count in every
    /// event's response header and sends only that many elements
    /// (`rsrv/camessage.c:504-509,537-568`), so a dynamic waveform
    /// (`NORD < NELM`) delivers `NORD` elements per event instead of a
    /// max-capacity array with a padded/stale tail. Identical wire
    /// behaviour to [`Self::subscribe_with_mask`] (which also requests no
    /// cap); this is the intent-revealing entry for callers — like the CA
    /// gateway — whose correctness depends on autosize, so the count-zero
    /// contract is explicit at the call site and a future edit cannot
    /// silently introduce a fixed-capacity cap.
    pub async fn subscribe_with_mask_autosize(
        &self,
        deadband: f64,
        mask: u16,
    ) -> CaResult<MonitorHandle> {
        self.subscribe_with_mask_readback_count(deadband, mask, EnumReadback::Native, false, None)
            .await
    }

    /// Like [`Self::subscribe_with_mask`], but when `enum_as_string` is
    /// set an ENUM field is monitored in its `DBR_TIME_STRING` form so
    /// values arrive as state labels — the `camonitor` default (C
    /// `camonitor.c:156-160`, which requests `DBR_TIME_STRING` for enums
    /// unless `-n`). The substitution is applied by the coordinator at
    /// connect-time type derivation (and re-applied on reconnect), so it
    /// holds even though the subscribe is issued before connect.
    pub async fn subscribe_with_mask_enum_as_string(
        &self,
        deadband: f64,
        mask: u16,
        enum_as_string: bool,
    ) -> CaResult<MonitorHandle> {
        self.subscribe_with_mask_readback(deadband, mask, enum_as_string, false)
            .await
    }

    /// Like [`Self::subscribe_with_mask_enum_as_string`], but also honours
    /// `float_as_string`: when set, a FLOAT/DOUBLE field is monitored in
    /// its `DBR_TIME_STRING` form so the server renders the value to a
    /// string at record precision (`camonitor -s` / `floatAsString`; C
    /// `camonitor.c:162-166`). The ENUM substitution takes precedence over
    /// the float one, matching C's `if (ENUM) … else if (floatAsString …)`.
    pub async fn subscribe_with_mask_readback(
        &self,
        deadband: f64,
        mask: u16,
        enum_as_string: bool,
        float_as_string: bool,
    ) -> CaResult<MonitorHandle> {
        // The library convenience API only distinguishes label-vs-native for
        // ENUM (it has no numeric mode — only `camonitor -n` does, via the
        // [`EnumReadback`]-typed `subscribe_with_mask_readback_count`): set →
        // `Label` (state labels), unset → `Native` (numeric index, the
        // existing library/gateway default).
        let enum_readback = if enum_as_string {
            EnumReadback::Label
        } else {
            EnumReadback::Native
        };
        self.subscribe_with_mask_readback_count(
            deadband,
            mask,
            enum_readback,
            float_as_string,
            None,
        )
        .await
    }

    /// Like [`Self::subscribe_with_mask_readback`], but bounds the monitor
    /// to at most `req_count` array elements (`camonitor -#`); `None`
    /// requests the full native count. The cap is clamped to the channel's
    /// native element count at connect-time — `reqElems > nElems` requests
    /// `nElems`, mirroring C `camonitor.c:169` — and re-clamped on reconnect
    /// against the fresh native count, so a large-waveform monitor transfers
    /// and decodes only the requested slice on every event instead of the
    /// whole array (C `camonitor.c:168-180` applies `reqElems` to the
    /// `ca_create_subscription` count).
    pub async fn subscribe_with_mask_readback_count(
        &self,
        deadband: f64,
        mask: u16,
        enum_readback: EnumReadback,
        float_as_string: bool,
        req_count: Option<u32>,
    ) -> CaResult<MonitorHandle> {
        let env = epics_base_rs::runtime::env::get("EPICS_CA_MONITOR_QUEUE")
            .and_then(|s| s.parse::<usize>().ok());
        let queue_size = resolve_monitor_queue_size(env);
        let (callback_tx, callback_rx) = mpsc::channel(queue_size);
        let coalesce_slot = subscription::CoalesceSlot::new();

        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::Subscribe {
            cid: self.cid,
            mask,
            deadband,
            callback_tx,
            coalesce_slot: coalesce_slot.clone(),
            req_count,
            enum_readback,
            float_as_string,
            reply: reply_tx,
        });

        // the coordinator allocates the subid against its live
        // subscription table and returns it on success.
        let subid = reply_rx.await.map_err(|_| CaError::Shutdown)??;

        Ok(MonitorHandle {
            subid,
            callback_rx,
            coalesce_slot,
            coord_tx: self.coord_tx.clone(),
            channel: self.clone(),
        })
    }

    /// Subscribe to per-channel lifecycle events:
    /// `Connected` / `Disconnected` / `Unresponsive` and
    /// `AccessRightsChanged { access }`. Mirrors libca's
    /// `ca_create_channel(... connection_callback ...)` plus
    /// `ca_replace_access_rights_event` at a single broadcast
    /// surface — a `tokio::sync::broadcast::Receiver` so multiple
    /// subscribers per channel are cheap.
    ///
    /// The receiver is bounded (16); slow consumers see
    /// `RecvError::Lagged` and should re-subscribe after polling
    /// the current state via [`Self::info`] — its `access_rights` field
    /// carries the current rights, and its `Err(CaError::Disconnected)` IS
    /// the disconnected state.
    pub fn connection_events(&self) -> broadcast::Receiver<ConnectionEvent> {
        self.conn_tx.subscribe()
    }

    /// Convenience wrapper around [`Self::connection_events`] that
    /// invokes `cb` on every `AccessRightsChanged`. Returns an
    /// [`EventWatcher`] guard — drop it (or call `.abort()`) to stop
    /// watching; the watcher task is aborted on drop. Mirrors libca
    /// `ca_replace_access_rights_event` at the callback-registration
    /// shape.
    pub fn on_access_rights_change<F>(&self, mut cb: F) -> EventWatcher
    where
        F: FnMut(AccessRights) + Send + 'static,
    {
        let mut rx = self.conn_tx.subscribe();
        let handle = epics_base_rs::runtime::task::spawn(async move {
            while let Ok(evt) = rx.recv().await {
                if let ConnectionEvent::AccessRightsChanged { read, write } = evt {
                    cb(AccessRights { read, write });
                }
            }
        });
        EventWatcher { handle }
    }

    /// Convenience wrapper around [`Self::connection_events`] that
    /// invokes `cb(true)` on `Connected` and `cb(false)` on
    /// `Disconnected`. Mirrors libca
    /// `ca_change_connection_event(chid, callback)` (oldChannelNotify.cpp:229).
    /// Returns an [`EventWatcher`] guard — drop it (or call `.abort()`)
    /// to stop watching; the watcher task is aborted on drop.
    pub fn on_connection_change<F>(&self, mut cb: F) -> EventWatcher
    where
        F: FnMut(bool) + Send + 'static,
    {
        let mut rx = self.conn_tx.subscribe();
        let handle = epics_base_rs::runtime::task::spawn(async move {
            while let Ok(evt) = rx.recv().await {
                match evt {
                    ConnectionEvent::Connected => cb(true),
                    ConnectionEvent::Disconnected => cb(false),
                    _ => {}
                }
            }
        });
        EventWatcher { handle }
    }

    /// The server's host NAME, as libca `ca_host_name(chid)`
    /// (`oldChannelNotify.cpp:189` → `tcpiiu::getHostName` → `hostNameCache`)
    /// reports it: reverse-resolved, with the port appended
    /// (`"ioc1.lab:5064"`), and only the dotted IP when the address has no
    /// PTR record. This used to hand back the raw `SocketAddr`, which is what
    /// put a dotted IP on `cainfo`'s `Host:` line where C prints a name
    /// (W10-B5) — the resolution belongs here, at the `ca_host_name` analog,
    /// not in the tool.
    ///
    /// The lookup is `types::peer_resolved_name` — `hostname::ip_addr_to_a`
    /// on a blocking thread: this is an `async fn` a caller awaits, so it can
    /// wait for the resolver exactly as C's `cainfo` does, and no other
    /// channel's progress is behind it.
    ///
    /// Returns `Err` if the channel hasn't connected yet — pvxs
    /// returns `"<disconnected>"` for the same case; we surface
    /// the typed error instead so callers can decide.
    pub async fn host_name(&self) -> CaResult<String> {
        let info = self.info().await?;
        let addr = info.server_addr;
        Ok(types::peer_resolved_name(addr).await)
    }

    /// Server's CA minor protocol version, parsed from the
    /// `CA_PROTO_VERSION` reply on the TCP virtual circuit.
    /// Returns `None` when the channel isn't operational or no
    /// VERSION reply has been processed yet. Mirrors libca
    /// `ca_host_minor_protocol(chid)` (oldChannelNotify.cpp,
    /// BUG_ARCHAEOLOGY d763541).
    pub async fn host_minor_protocol(&self) -> Option<u16> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::GetHostMinorProtocol {
            cid: self.cid,
            reply: reply_tx,
        });
        reply_rx.await.ok().flatten()
    }

    /// Server supports CA v4.2+ features (notably asynchronous
    /// `ACCESS_RIGHTS` events delivered without a request from the
    /// client). Mirrors libca `ca_v42_ok(chid)` (cadef.h:1853).
    /// Returns `false` when the channel is still searching/connecting
    /// or the server's minor protocol version is below 2.
    pub async fn v42_ok(&self) -> bool {
        matches!(self.host_minor_protocol().await, Some(v) if v >= 2)
    }

    /// Stash an arbitrary `Arc<T: Any+Send+Sync>` on this channel —
    /// Rust analog of libca `ca_set_puser(chid, void*)` (cadef.h:243).
    /// All clones of the same channel share one slot.
    pub fn set_user_data<T: std::any::Any + Send + Sync>(&self, data: Arc<T>) {
        *self.user_data.lock() = Some(data);
    }

    /// Retrieve the user-data slot if present, downcast to `T`.
    /// Returns `None` if the slot is empty or the stored type does
    /// not match. Mirrors libca `ca_puser(chid)` (cadef.h:251) modulo
    /// the type-tag.
    pub fn user_data<T: std::any::Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.user_data
            .lock()
            .as_ref()
            .and_then(|any| Arc::clone(any).downcast::<T>().ok())
    }

    /// Clear the user-data slot. Returns the previous contents (if any).
    pub fn clear_user_data(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        self.user_data.lock().take()
    }

    /// Time since the underlying TCP virtual circuit last received
    /// any message from the server. Mirrors libca
    /// `ca_receive_watchdog_delay(chid)` (oldChannelNotify.cpp:703) —
    /// hung-server detection. Returns `Duration::ZERO` when the
    /// channel isn't operational (the watchdog isn't running).
    pub async fn receive_watchdog_delay(&self) -> Duration {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::GetWatchdogDelay {
            cid: self.cid,
            reply: reply_tx,
        });
        match reply_rx.await {
            Ok(Some(d)) => d,
            _ => Duration::ZERO,
        }
    }
}

/// libca `ca_version()` (`cadef.h`) — re-exported from its one owner,
/// [`crate::protocol::ca_version`]. C returns the wire protocol revision
/// (`"4.13"`); a caller comparing against it gets that, not a library banner.
pub use crate::protocol::ca_version;

/// Handle for a monitor subscription. Dropping it cancels the subscription.
pub struct MonitorHandle {
    subid: u32,
    callback_rx: mpsc::Receiver<CaResult<Snapshot>>,
    /// Shared coalesce slot — drained after the bounded `callback_rx`
    /// is empty so the latest under-pressure snapshot is always
    /// delivered. See [`subscription::CoalesceSlot`].
    coalesce_slot: std::sync::Arc<subscription::CoalesceSlot>,
    coord_tx: mpsc::UnboundedSender<CoordRequest>,
    /// Parent channel — kept so [`MonitorHandle::channel`] can return
    /// the same handle libca's `ca_evid_to_chid(evid)` would. Cheap to
    /// clone (CaChannel is Arc-internally).
    channel: CaChannel,
}

impl MonitorHandle {
    /// Recover the channel handle this subscription is attached to.
    /// Mirrors libca `ca_evid_to_chid(evid)` (cadef.h:1220).
    pub fn channel(&self) -> CaChannel {
        self.channel.clone()
    }

    /// Subscription identifier (matches the libca `evid` opaque).
    /// Useful for logging / diagnostic correlation across reconnects.
    pub fn subid(&self) -> u32 {
        self.subid
    }

    /// Receive the next monitor update.
    ///
    /// Order of preference:
    /// 1. Anything buffered in the bounded channel (preserves FIFO
    ///    under normal load).
    /// 2. The coalesce slot, populated when the channel was full
    ///    at delivery time (`SubscriptionRecord::coalesce_slot`).
    /// 3. Block until either a new channel item lands or the
    ///    producer signals the slot via `Notify`.
    ///
    /// `None` means the producer side closed (subscription cancelled
    /// or coordinator shut down).
    ///
    /// While the subscription is [paused](Self::pause) the bounded
    /// channel backlog (pre-pause values) and any bypassing errors
    /// (e.g. `ECA_DISCONN`) are still delivered, but the held latest
    /// value in the coalesce slot is withheld until [`Self::resume`].
    pub async fn recv(&mut self) -> Option<CaResult<Snapshot>> {
        use tokio::sync::mpsc::error::TryRecvError;
        loop {
            // 1. Drain the bounded channel first. The channel carries
            //    pre-pause backlog (3a) and pause-bypassing errors, so
            //    it is always readable regardless of pause state.
            match self.callback_rx.try_recv() {
                Ok(msg) => return Some(msg),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => {}
            }
            // 2. Channel empty — take the coalesce slot if anything is
            //    deliverable now. `take_deliverable` returns an `Err`
            //    even while paused (errors bypass pause), a value when
            //    not paused or buffered before the pause, and `None`
            //    for a value held during pause. Atomic against `resume`.
            //
            if let Some(msg) = self.coalesce_slot.take_deliverable() {
                return Some(msg);
            }
            // 3. Nothing deliverable — wait. `notified` fires on a
            //    non-paused slot write, an error write, or `resume`.
            //    On a value arriving during pause the producer holds it
            //    in the slot WITHOUT notifying, so we correctly stay
            //    parked until resume or an error.
            let notified = self.coalesce_slot.notified();
            tokio::pin!(notified);
            tokio::select! {
                msg = self.callback_rx.recv() => return msg,
                _ = &mut notified => {
                    // Loop and recheck — slot/channel/pause state may
                    // have changed. Each subscription owns its slot, so
                    // a spurious wake is harmless.
                }
            }
        }
    }

    /// Pause value-monitor delivery — client-side only, **no CA wire
    /// message** (so wire compatibility is preserved). While paused:
    ///
    /// - new value updates coalesce into the latest slot and are NOT
    ///   yielded by [`Self::recv`] (semantic 2a);
    /// - the bounded-channel backlog captured before the pause stays
    ///   readable and drains normally on `recv` (semantic 3a);
    /// - connection/terminal errors (`ECA_DISCONN`, monitor status
    ///   errors) bypass the pause and are delivered immediately.
    ///
    /// On [`Self::resume`] the channel backlog drains first, then the
    /// single held latest value is delivered.
    ///
    /// This is the option-1 virtual pause. A wire-level
    /// `EVENT_CANCEL`/`EVENT_ADD` pause (option 2) has different
    /// semantics — round-trip, re-subscribe, event gap, subid
    /// lifecycle — and is intentionally NOT folded into this method;
    /// it would be a separate `pause_server`-style API.
    pub fn pause(&self) {
        self.coalesce_slot.set_paused(true);
    }

    /// Resume a [paused](Self::pause) subscription. Wakes a parked
    /// [`Self::recv`] so the held latest value is flushed after the
    /// channel backlog. No-op (and no wake) if not currently paused.
    pub fn resume(&self) {
        self.coalesce_slot.set_paused(false);
    }

    /// Whether the subscription is currently paused.
    pub fn is_paused(&self) -> bool {
        self.coalesce_slot.is_paused()
    }
}

impl Drop for MonitorHandle {
    fn drop(&mut self) {
        let _ = self
            .coord_tx
            .send(CoordRequest::Unsubscribe { subid: self.subid });
    }
}

/// Abort-on-drop guard for a connection / access-rights watcher task.
///
/// A bare `tokio::task::JoinHandle` *detaches* on drop — it does not
/// abort the spawned task. Returning one from
/// [`CaChannel::on_connection_change`] / [`CaChannel::on_access_rights_change`]
/// while documenting "drop to stop" would leak a running task. This
/// guard mirrors the `ServerConnection` abort-on-drop pattern: dropping
/// it aborts the inner watcher task.
#[must_use = "dropping the EventWatcher immediately stops the watcher task; \
              bind it to a variable to keep watching"]
pub struct EventWatcher {
    // Spawned via `runtime::task::spawn`; seam handle (see `ServerConnection`).
    handle: epics_base_rs::runtime::task::TaskHandle<()>,
}

impl EventWatcher {
    /// Stop watching now. Equivalent to dropping the guard; provided
    /// for callers that want an explicit, named teardown.
    pub fn abort(self) {
        // Drop runs `handle.abort()`.
    }
}

impl Drop for EventWatcher {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

// --- Coordinator ---

/// Floor for the per-subscription bounded monitor channel. A queue this
/// small coalesces almost everything into the slot; keep a little headroom
/// so short bursts arrive as discrete updates.
const MIN_MONITOR_QUEUE_SIZE: usize = 10;

/// Resolve the per-subscription bounded-queue size from the optional
/// `EPICS_CA_MONITOR_QUEUE` value. Defaults to 256, floored at
/// [`MIN_MONITOR_QUEUE_SIZE`].
fn resolve_monitor_queue_size(env: Option<usize>) -> usize {
    env.unwrap_or(256).max(MIN_MONITOR_QUEUE_SIZE)
}
#[allow(clippy::too_many_arguments)]
async fn run_coordinator(
    mut coord_rx: mpsc::UnboundedReceiver<CoordRequest>,
    mut search_rx: mpsc::UnboundedReceiver<SearchResponse>,
    mut transport_rx: mpsc::UnboundedReceiver<TransportEvent>,
    search_tx: mpsc::UnboundedSender<SearchRequest>,
    transport_tx: mpsc::UnboundedSender<TransportCommand>,
    in_flight: types::InFlightOps,
    cid_alloc: types::CidAllocator,
    snapshots: ChannelSnapshots,
    last_rx_at: ServerLastRxAt,
    diag: Arc<CaDiagnostics>,
    exception_slot: types::CaExceptionSlot,
    search_attempts: types::SearchAttempts,
    #[cfg(ca_beacon_monitor)] beacon_ctrl_tx: mpsc::UnboundedSender<beacon_monitor::BeaconControl>,
) {
    let mut channels: HashMap<u32, ChannelInner> = HashMap::new();
    let mut pending_wait_connected: HashMap<u32, Vec<oneshot::Sender<()>>> = HashMap::new();
    let mut pending_found: HashMap<u32, SocketAddr> = HashMap::new();
    let mut subscriptions = SubscriptionRegistry::new();
    // Reverse index: circuit `(server_addr, priority)` -> set of cids
    // last seen on that circuit. Keep disconnected channels indexed so
    // beacon anomalies can trigger immediate re-search for the affected
    // IOC. keyed by circuit so tearing down one priority
    // circuit only re-searches its own channels.
    let mut server_channels: HashMap<types::CircuitKey, HashSet<u32>> = HashMap::new();
    // per-circuit flow control. EVENTS_OFF/ON is a per-tcpiiu
    // CA message, so the outstanding-event count and the on/off gate
    // are tracked per `(server_addr, priority)`.
    // Per-circuit CA minor protocol version, populated from
    // CA_PROTO_VERSION on TCP handshake. Powers `host_minor_protocol`.
    let mut server_minor_version: HashMap<types::CircuitKey, u16> = HashMap::new();

    loop {
        tokio::select! {
            req = coord_rx.recv() => {
                let Some(req) = req else { return };
                match req {
                    CoordRequest::RegisterChannel { cid, pv_name, priority, conn_tx } => {
                        // Drain any waiters that arrived before registration.
                        let early_waiters = pending_wait_connected
                            .remove(&cid)
                            .unwrap_or_default();
                        channels.insert(cid, ChannelInner {
                            cid,
                            pv_name: pv_name.clone(),
                            priority,
                            state: ChannelState::Searching,
                            sid: 0,
                            native_type: None,
                            element_count: 0,
                            server_addr: None,
                            access_rights: AccessRights::from_u32(0),
                            connect_waiters: early_waiters,
                            conn_tx,
                            reconnect_count: 0,
                            last_connected_at: None,
                        });
                        // Process any Found response that arrived before registration.
                        if let Some(server_addr) = pending_found.remove(&cid) {
                            let ch = channels.get_mut(&cid).unwrap();
                            ch.state = ChannelState::Connecting;
                            ch.server_addr = Some(server_addr);
                            server_channels.entry((server_addr, priority)).or_default().insert(cid);
                            let _ = transport_tx.send(TransportCommand::CreateChannel {
                                cid,
                                pv_name,
                                server_addr,
                                priority,
                            });
                        }
                    }
                    CoordRequest::WaitConnected { cid, reply } => {
                        if let Some(ch) = channels.get_mut(&cid) {
                            if ch.state == ChannelState::Connected {
                                let _ = reply.send(());
                            } else {
                                ch.connect_waiters.push(reply);
                            }
                        } else {
                            // Channel not yet registered — stash the waiter
                            // so RegisterChannel can drain it when it arrives.
                            pending_wait_connected
                                .entry(cid)
                                .or_default()
                                .push(reply);
                        }
                    }
                    CoordRequest::Subscribe { cid, mask, deadband, callback_tx, coalesce_slot, req_count, enum_readback, float_as_string, reply } => {
                        if let Some(ch) = channels.get(&cid) {
                            let server_addr = ch.server_addr.unwrap_or_else(|| {
                                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
                            });
                            let priority = ch.priority;
                            let connected = ch.state == ChannelState::Connected;
                            // Single owner of the subscribe-time readback
                            // derivation: ENUM substituted per enum_readback
                            // (Label→DBR_TIME_STRING, Numeric→DBR_TIME_INT,
                            // Native→keep), else FLOAT/DOUBLE under -s becomes
                            // DBR_TIME_STRING, else native TIME type
                            // (C camonitor.c:155-166).
                            let data_type = ch
                                .native_type
                                .map(|t| subscription_readback_dbr(t, enum_readback, float_as_string));
                            // Single owner of the reqElems→wire-count rule: a
                            // `-#` cap is clamped to the native element count,
                            // no cap resolves to wire 0 (autosize) (C
                            // camonitor.c:168-169). Reconnect re-resolves via
                            // restore_for_channel against the fresh count.
                            let count = ch
                                .native_type
                                .map(|_| subscription::resolve_subscription_count(req_count, ch.element_count));
                            // C keeps the user's cap in `netSubscription::count`
                            // and resolves the WIRE count per request via
                            // `getCount(guard, CA_V413(minor))` (`netIO.h:241-251`)
                            // — so the zero-count autosize contract is only used
                            // with a peer that implements it.
                            let peer_minor = ch
                                .server_addr
                                .and_then(|addr| {
                                    server_minor_version.get(&(addr, ch.priority)).copied()
                                })
                                .unwrap_or(0);

                            // allocate the subid here, where the
                            // live subscription table lives, so the wrap
                            // probe sees every active subscription. The
                            // immutable `alloc_subid` borrow ends before
                            // the `add` mutable borrow below.
                            let subid = subscriptions.alloc_subid();

                            subscriptions.add(subscription::SubscriptionRecord {
                                subid,
                                cid,
                                data_type,
                                count,
                                // Requested cap preserved across reconnects;
                                // restore_for_channel re-clamps it to the
                                // fresh native count each connect.
                                req_count,
                                // The public subscribe API auto-derives the
                                // DBR type from the channel's native type, so
                                // it must re-derive on NativeTypeChanged. The
                                // `enum_readback` / `float_as_string`
                                // preferences are carried on the record so the
                                // restore re-derivation applies the same
                                // substitution chain.
                                type_user_supplied: false,
                                enum_readback,
                                float_as_string,
                                mask,
                                server_addr,
                                priority,
                                deadband,
                                callback_tx,
                                coalesce_slot,
                                needs_restore: !connected,
                                last_value: None,
                                nreplace: 0,
                            });

                            if connected {
                                let data_type =
                                    data_type.expect("connected channel has native type");
                                // C `tcpiiu::subscriptionRequest`
                                // (`tcpiiu.cpp:1572-1585`): resolve the wire
                                // count, then bound it against what the circuit
                                // can carry. Past the bound the subscription is
                                // never installed and `ca_create_subscription`
                                // returns ECA_TOLARGE.
                                let wire_count = crate::protocol::subscription_wire_count(
                                    count.expect("connected channel has element count"),
                                    ch.element_count,
                                    data_type,
                                    peer_minor,
                                );
                                let wire_count = match wire_count {
                                    Ok(c) => c,
                                    Err(e) => {
                                        subscriptions.remove(subid);
                                        let _ = reply.send(Err(e));
                                        continue;
                                    }
                                };
                                let _ = transport_tx.send(TransportCommand::Subscribe {
                                    sid: ch.sid,
                                    data_type,
                                    count: wire_count,
                                    subid,
                                    mask,
                                    server_addr,
                                    priority,
                                });
                            }
                            let _ = reply.send(Ok(subid));
                        } else {
                            let _ = reply.send(Err(CaError::Disconnected));
                        }
                    }
                    CoordRequest::Unsubscribe { subid } => {
                        if let Some(rec) = subscriptions.get(subid) {
                            let cid = rec.cid;
                            if let Some(ch) = channels.get(&cid) {
                                if ch.state == ChannelState::Connected {
                                    if let Some(data_type) = rec.data_type {
                                        let server_addr = ch.server_addr.unwrap();
                                        // C `tcpiiu::subscriptionCancelRequest`
                                        // (`tcpiiu.cpp:1659`) re-resolves the wire
                                        // count through the same
                                        // `getCount(CA_V413(minor))` gate the
                                        // EVENT_ADD used, so the cancel echoes the
                                        // count the server was given.
                                        let peer_minor = server_minor_version
                                            .get(&(server_addr, ch.priority))
                                            .copied()
                                            .unwrap_or(0);
                                        let _ = transport_tx.send(TransportCommand::Unsubscribe {
                                            sid: ch.sid,
                                            subid,
                                            data_type,
                                            count: crate::protocol::subscription_cancel_wire_count(
                                                rec.count.unwrap_or(0),
                                                ch.element_count,
                                                peer_minor,
                                            ),
                                            server_addr,
                                            priority: ch.priority,
                                        });
                                    }
                                }
                            }
                        }
                        subscriptions.remove(subid);
                    }
                    CoordRequest::DropChannel { cid } => {
                        // Cancel all subscriptions for this channel
                        let sub_ids = subscriptions.for_cid(cid);
                        for subid in sub_ids {
                            if let Some(rec) = subscriptions.get(subid) {
                                if let Some(ch) = channels.get(&cid) {
                                    if ch.state == ChannelState::Connected {
                                        if let Some(data_type) = rec.data_type {
                                            let server_addr = ch.server_addr.unwrap();
                                            let peer_minor = server_minor_version
                                                .get(&(server_addr, ch.priority))
                                                .copied()
                                                .unwrap_or(0);
                                            let _ = transport_tx.send(TransportCommand::Unsubscribe {
                                                sid: ch.sid,
                                                subid,
                                                data_type,
                                                count:
                                                    crate::protocol::subscription_cancel_wire_count(
                                                        rec.count.unwrap_or(0),
                                                        ch.element_count,
                                                        peer_minor,
                                                    ),
                                                server_addr,
                                                priority: ch.priority,
                                            });
                                        }
                                    }
                                }
                            }
                            subscriptions.remove(subid);
                        }

                        // Clear channel on server + clean reverse index
                        let mut cleared_on_server = false;
                        if let Some(ch) = channels.get(&cid) {
                            if ch.state.is_operational() {
                                let _ = transport_tx.send(TransportCommand::ClearChannel {
                                    cid,
                                    sid: ch.sid,
                                    server_addr: ch.server_addr.unwrap(),
                                    priority: ch.priority,
                                });
                                cleared_on_server = true;
                            }
                            // Cancel search for any non-connected state
                            match ch.state {
                                ChannelState::Searching
                                | ChannelState::Connecting
                                | ChannelState::Disconnected => {
                                    let _ = search_tx.send(SearchRequest::Cancel { cid });
                                }
                                _ => {}
                            }
                            if let Some(addr) = ch.server_addr {
                                remove_server_channel(&mut server_channels, (addr, ch.priority), cid);
                            }
                        }
                        channels.remove(&cid);
                        // release the cid back to the allocator at
                        // the single channel-removal site, in lockstep with
                        // `channels.remove`, so the live-set never lags the
                        // authoritative table.
                        cid_alloc.release(cid);
                        snapshots.remove(&cid);
                        // A channel that never reached the server cannot be
                        // the subject of a later ERROR, so its write identity
                        // closes here. When a CLEAR_CHANNEL did go out the
                        // window stays open until the server confirms it —
                        // that confirmation is the fence, and dropping the
                        // identity here instead is exactly the race this
                        // record exists to remove.
                        if !cleared_on_server {
                            in_flight.write_identities.remove(&cid);
                        }
                        // Drop any in-flight read/write entries for this
                        // cid. Normally `self.in_flight.reads/writes
                        // .remove(&ioid)` in the op future already cleans
                        // up; this catches the case where a caller drops
                        // the future (cancel) and the channel together
                        // before either the response arrives or a
                        // disconnect drain runs.
                        let mut affected = HashSet::with_capacity(1);
                        affected.insert(cid);
                        drain_waiters_for_cids(&affected, &in_flight);
                    }
                    CoordRequest::GetWatchdogDelay { cid, reply } => {
                        let delay = channels.get(&cid).and_then(|ch| {
                            if !ch.state.is_operational() {
                                return None;
                            }
                            let addr = ch.server_addr?;
                            last_rx_at.get(&(addr, ch.priority)).map(|e| e.value().elapsed())
                        });
                        let _ = reply.send(delay);
                    }
                    CoordRequest::GetHostMinorProtocol { cid, reply } => {
                        let v = channels.get(&cid).and_then(|ch| {
                            if !ch.state.is_operational() {
                                return None;
                            }
                            let addr = ch.server_addr?;
                            server_minor_version.get(&(addr, ch.priority)).copied()
                        });
                        let _ = reply.send(v);
                    }
                    CoordRequest::GetIocConnectionCount { reply } => {
                        // libca counts virtual circuits, not channels,
                        // and keys each circuit on (address, priority)
                        // — so two priorities to one IOC are two
                        // circuits. The dedup lives in
                        // `operational_circuit_count` so it is
                        // unit-testable without the coordinator.
                        let states = channels
                            .values()
                            .map(|ch| (ch.state, ch.server_addr, ch.priority));
                        let _ = reply.send(operational_circuit_count(states));
                    }
                    CoordRequest::Shutdown { reply } => {
                        // Send ClearChannel for all connected channels
                        for ch in channels.values() {
                            if ch.state.is_operational() {
                                if let Some(addr) = ch.server_addr {
                                    let _ = transport_tx.send(TransportCommand::ClearChannel {
                                        cid: ch.cid,
                                        sid: ch.sid,
                                        server_addr: addr,
                                        priority: ch.priority,
                                    });
                                }
                            }
                        }
                        let _ = reply.send(());
                        return; // Exit coordinator loop
                    }
                    #[cfg(ca_beacon_monitor)]
                    CoordRequest::ForceRescanServer { server_addr, kind } => {
                        // FirstSighting is a per-client bookkeeping
                        // event (our beacon map was empty for this
                        // server), not a server-side anomaly. Logging
                        // it as a warning every time a fresh CaClient
                        // hears its first beacon was misleading and
                        // would over-promote a benign condition.
                        // Reserve the warn-level message for the
                        // classifications libca counts as anomalies
                        // (`cac.cpp:498` `beaconAnomalyCount++`).
                        let anomaly_msg = match kind {
                            beacon_monitor::BeaconAnomalyKind::FirstSighting => None,
                            beacon_monitor::BeaconAnomalyKind::IdMismatch => {
                                Some("beacon sequence restarted — IOC may have restarted")
                            }
                            beacon_monitor::BeaconAnomalyKind::ShortPeriod => Some(
                                "beacon period collapsed below 0.80x of the running average \
                                 — IOC may have restarted",
                            ),
                            beacon_monitor::BeaconAnomalyKind::LongPeriod => Some(
                                "beacon period exceeded 3.25x of the running average \
                                 — server was unreachable and is back",
                            ),
                        };
                        if let Some(msg) = anomaly_msg {
                            diag.beacon_anomalies.fetch_add(1, Ordering::Relaxed);
                            diag.record(DiagEvent::BeaconAnomaly { server: server_addr });
                            tracing::warn!(
                                server = %server_addr,
                                ?kind,
                                "{msg}"
                            );
                            metrics::counter!(
                                "ca_client_beacon_anomalies_total",
                                "server" => server_addr.to_string()
                            )
                            .increment(1);
                        } else {
                            tracing::debug!(
                                server = %server_addr,
                                "first sighting of beacon source — waking pending searches"
                            );
                            metrics::counter!(
                                "ca_client_beacon_first_sighting_total",
                                "server" => server_addr.to_string()
                            )
                            .increment(1);
                        }

                        // Rescan all disconnected/searching channels.
                        // The beacon's announced address may use
                        // INADDR_ANY and won't match our stored
                        // server_addr, so a per-server lookup would
                        // be unreliable. Operational circuits get
                        // their watchdog state updated through the
                        // separate `BeaconArrival` path — this branch
                        // no longer issues EchoProbe directly, mirror-
                        // ing libca's split between udpiiu (search
                        // wake) and tcpRecvWatchdog (lazy probe).
                        for ch in channels.values() {
                            if ch.state == ChannelState::Disconnected
                                || ch.state == ChannelState::Searching
                            {
                                let _ = search_tx.send(SearchRequest::Schedule {
                                    cid: ch.cid,
                                    pv_name: ch.pv_name.to_string(),
                                    reason: SearchReason::BeaconAnomaly,
                                });
                            }
                        }
                    }
                    #[cfg(ca_beacon_monitor)]
                    CoordRequest::BeaconArrival { server_addr, anomaly } => {
                        // Pure transport-watchdog signal. The
                        // routing decision (exact match vs.
                        // port-only fallback for INADDR_ANY /
                        // multi-homed IOCs) lives in
                        // `beacon_arrival_targets` so it can be
                        // unit-tested without standing up the full
                        // coordinator. See the function's doc
                        // comment for the full rationale.
                        let states = channels.values().map(|ch| (ch.state, ch.server_addr));
                        for target in beacon_arrival_targets(states, server_addr) {
                            let _ = transport_tx.send(
                                TransportCommand::BeaconArrivalNotify {
                                    server_addr: target,
                                    anomaly,
                                },
                            );
                        }
                    }
                }
            }
            resp = search_rx.recv() => {
                let Some(resp) = resp else { return };
                match resp {
                    SearchResponse::Found { cid, server_addr } => {
                        if let Some(ch) = channels.get_mut(&cid) {
                            if ch.state == ChannelState::Searching || ch.state == ChannelState::Disconnected {
                                let priority = ch.priority;
                                if let Some(old_addr) = ch.server_addr {
                                    remove_server_channel(&mut server_channels, (old_addr, priority), cid);
                                }
                                ch.state = ChannelState::Connecting;
                                ch.server_addr = Some(server_addr);
                                server_channels.entry((server_addr, priority)).or_default().insert(cid);
                                let _ = transport_tx.send(TransportCommand::CreateChannel {
                                    cid,
                                    pv_name: ch.pv_name.to_string(),
                                    server_addr,
                                    priority,
                                });
                            }
                        } else {
                            // Channel not registered yet — stash the Found
                            // response so RegisterChannel can process it.
                            pending_found.insert(cid, server_addr);
                        }
                    }
                    // deliver ECA_DBLCHNL via the registered
                    // exception handler so library users see the
                    // multiply-defined-PV condition the same way they
                    // see it under libca (`pvMultiplyDefinedNotify`
                    // → `this->exception(... ECA_DBLCHNL, ...)`).
                    // The search-side already logs and metrics.
                    SearchResponse::MultiplyDefined {
                        pv_name,
                        prev_addr,
                        new_addr,
                    } => {
                        types::dispatch_exception(
                            &exception_slot,
                            types::multiply_defined_pv_exception(pv_name, prev_addr, new_addr),
                        );
                    }
                }
            }
            evt = transport_rx.recv() => {
                let Some(evt) = evt else { return };
                // The per-server "last RX" stamp is now bumped directly
                // in the transport `read_loop` via the shared
                // `ServerLastRxAt` sidecar — covers READ_NOTIFY /
                // WRITE_NOTIFY / EVENT_ADD frames that no longer round-
                // trip through this match (Option C, Phase A/D). This
                // arm therefore no longer touches `last_rx_at`.
                match evt {
                    TransportEvent::ChannelCreated { cid, sid, data_type, element_count, access, server_addr, priority } => {
                        if let Some(ch) = channels.get_mut(&cid) {
                            let was_disconnected = matches!(ch.state, ChannelState::Disconnected);
                            let dbr_type = DbFieldType::from_u16(data_type).ok();
                            // epics-base `16877577` parity: detect native
                            // DBR type change across reconnects so consumers
                            // (camonitor, archiver) can rebuild their
                            // per-type decoders / display metadata before
                            // the next event flows.
                            let previous_native = ch.native_type;
                            let native_changed = match (previous_native, dbr_type) {
                                (Some(p), Some(c)) => p != c,
                                (Some(_), None) | (None, Some(_)) => true,
                                (None, None) => false,
                            };
                            ch.state = ChannelState::Connected;
                            ch.sid = sid;
                            ch.native_type = dbr_type;
                            ch.element_count = element_count;
                            ch.server_addr = Some(server_addr);
                            ch.access_rights = access;
                            ch.last_connected_at = Some(std::time::Instant::now());

                            // Phase B: publish snapshot for fast-path readers.
                            // Only publish if we have a usable native_type;
                            // otherwise the channel is still effectively
                            // unusable and CaChannel hot paths should fall
                            // back to "no snapshot → Disconnected".
                            if let Some(dbr) = dbr_type {
                                snapshots.insert(
                                    cid,
                                    types::ChannelSnapshotPublic {
                                        sid,
                                        native_type: dbr,
                                        element_count,
                                        server_addr,
                                        access_rights: access,
                                        state: ChannelState::Connected,
                                        // The circuit's VERSION frame is parsed
                                        // (and its ServerVersion event queued)
                                        // before the CREATE_CHAN_RESP that
                                        // produced this event, so the map is
                                        // populated for any compliant server.
                                        // A server that never sent VERSION is
                                        // treated as pre-V49 — C's `tcpiiu`
                                        // starts from the same conservative
                                        // minor version.
                                        server_minor: server_minor_version
                                            .get(&(server_addr, priority))
                                            .copied()
                                            .unwrap_or(0),
                                    },
                                );
                            } else {
                                snapshots.remove(&cid);
                            }

                            if was_disconnected {
                                tracing::info!(pv = %ch.pv_name, cid, sid, server = %server_addr, "channel reconnected");
                            } else {
                                tracing::info!(pv = %ch.pv_name, cid, sid, server = %server_addr, "channel connected");
                            }
                            metrics::counter!("ca_client_connections_total", "server" => server_addr.to_string()).increment(1);
                            metrics::gauge!("ca_client_channels_connected").increment(1.0);

                            // Clear the diagnostic counter SYNCHRONOUSLY here,
                            // before any waiter wakes or Connected fires. The
                            // search task also clears via SearchRequest::ConnectResult
                            // below, but that's an async hop — without this
                            // synchronous remove, a caller awakened by the
                            // Connected event and immediately calling
                            // CaChannel::search_attempts() can briefly observe
                            // the pre-connect non-zero count, contradicting the
                            // documented "0 once connected" contract. The map
                            // is Arc-shared so this remove races nothing the
                            // search task hasn't already accepted as in-flight
                            // for cleanup.
                            search_attempts.remove(&cid);

                            // Wake connect waiters
                            for waiter in ch.connect_waiters.drain(..) {
                                let _ = waiter.send(());
                            }

                            // Broadcast connected + access rights events
                            let _ = ch.conn_tx.send(ConnectionEvent::Connected);
                            let _ = ch.conn_tx.send(ConnectionEvent::AccessRightsChanged {
                                read: access.read,
                                write: access.write,
                            });
                            // epics-base `16877577` parity: surface native
                            // DBR type changes so consumers can react before
                            // the next event flows. Only a transition from a
                            // *known* prior native type counts: on the first
                            // connect `previous_native` is `None` — the type is
                            // being discovered, not changed, and the `Connected`
                            // event already carries it. Firing NativeTypeChanged
                            // here too would make every first connect look like a
                            // type change and push consumers into a redundant
                            // metadata refetch (and, if their connect handler is
                            // not idempotent, a duplicate initial value). The
                            // `native_changed` flag itself is left as-is — it
                            // still drives the auto-derived decoder reset in
                            // `restore_for_channel`; only the consumer-facing
                            // event is gated on a real prior type.
                            if native_changed && previous_native.is_some() {
                                if let Some(cur) = dbr_type {
                                    let _ = ch.conn_tx.send(ConnectionEvent::NativeTypeChanged {
                                        previous: previous_native,
                                        current: cur,
                                    });
                                    tracing::info!(
                                        pv = %ch.pv_name,
                                        previous = ?previous_native,
                                        current = ?cur,
                                        "channel native DBR type changed"
                                    );
                                }
                            }

                            // Restore subscriptions
                            let (restored, stale) = subscriptions.restore_for_channel(
                                cid,
                                sid,
                                data_type,
                                element_count,
                                native_changed,
                                server_addr,
                                server_minor_version
                                    .get(&(server_addr, ch.priority))
                                    .copied()
                                    .unwrap_or(0),
                                &transport_tx,
                            );
                            diag.connections.fetch_add(1, Ordering::Relaxed);
                            diag.record(DiagEvent::Connected { pv: ch.pv_name.to_string(), server: server_addr });
                            if restored > 0 || stale > 0 {
                                diag.reconnections.fetch_add(1, Ordering::Relaxed);
                                diag.subscriptions_restored.fetch_add(restored as u64, Ordering::Relaxed);
                                diag.subscriptions_stale.fetch_add(stale as u64, Ordering::Relaxed);
                                diag.record(DiagEvent::Reconnected { pv: ch.pv_name.to_string(), restored, stale });
                                tracing::debug!(
                                    pv = %ch.pv_name,
                                    restored,
                                    stale,
                                    "CA reconnect: subscriptions restored"
                                );
                            }

                            // Notify search engine of successful connect (clears penalty).
                            let _ = search_tx.send(SearchRequest::ConnectResult {
                                cid,
                                success: true,
                                server_addr,
                            });
                        } else {
                            // CREATE_CHAN response arrived for
                            // an unknown CID — the user dropped the
                            // channel after CREATE_CHAN went out but
                            // before the server reply landed. C
                            // `libca/cac.cpp::createChannelRespAction`
                            // immediately sends
                            // `tcpiiu::clearChannelRequest(sid, cid)`
                            // to free the server SID it just learned;
                            // pre-fix Rust ignored the response, so
                            // the server-side channel sat allocated
                            // until the TCP circuit closed. Forward
                            // a CLEAR_CHANNEL to the transport
                            // manager so it can drain the leaked
                            // server-side state.
                            tracing::debug!(
                                cid,
                                sid,
                                server = %server_addr,
                                "late CREATE_CHAN response for unknown CID; sending CLEAR_CHANNEL (libca parity)"
                            );
                            let _ = transport_tx.send(TransportCommand::ClearChannel {
                                cid,
                                sid,
                                server_addr,
                                priority,
                            });
                        }
                    }
                    TransportEvent::MonitorData { subid, data_type, count, data } => {
                        use subscription::MonitorDeliveryOutcome;
                        match subscriptions.on_monitor_data(subid, data_type, count, &data) {
                            MonitorDeliveryOutcome::Queued => {}
                            MonitorDeliveryOutcome::Slotted => {
                                // Coalesced into the slot (overflow or
                                // pause-held). Diagnostic only — flow
                                // control keys on socket occupancy in
                                // `read_loop`, never on consumer backlog.
                                metrics::counter!(
                                    "ca_client_coalesced_monitors_total"
                                )
                                .increment(1);
                            }
                            MonitorDeliveryOutcome::Dropped => {
                                // With the coalesce slot, this is reachable
                                // only when the consumer channel is closed —
                                // i.e. the application dropped the
                                // `MonitorHandle`. Bumping the counter is
                                // still useful as a stale-handle signal.
                                diag.dropped_monitors.fetch_add(1, Ordering::Relaxed);
                                tracing::warn!(
                                    subid,
                                    "monitor dropped (consumer handle closed)"
                                );
                                metrics::counter!("ca_client_dropped_monitors_total").increment(1);
                            }
                            MonitorDeliveryOutcome::Filtered
                            | MonitorDeliveryOutcome::NotFound => {}
                        }
                    }
                    TransportEvent::MonitorStatusError { subid, eca_status } => {
                        // libca `cac::eventAddRespAction`
                        // (`cac.cpp:973-977`) routes a monitor frame
                        // whose `m_cid` is non-NORMAL through the
                        // per-subscription exception callback. The
                        // SubscriptionRegistry helper performs the
                        // equivalent — pushes
                        // `Err(CaError::ServerError(eca_status))`
                        // into the callback channel. Best-effort
                        // (silently dropped if the consumer queue
                        // is full or closed); matches libca's
                        // exception-callback semantics.
                        use subscription::MonitorDeliveryOutcome;
                        match subscriptions.on_monitor_error(subid, eca_status) {
                            MonitorDeliveryOutcome::Queued => {}
                            MonitorDeliveryOutcome::Slotted => {
                                metrics::counter!(
                                    "ca_client_coalesced_monitors_total"
                                )
                                .increment(1);
                            }
                            MonitorDeliveryOutcome::Dropped => {
                                diag.dropped_monitors.fetch_add(1, Ordering::Relaxed);
                                metrics::counter!(
                                    "ca_client_monitor_error_drops_total"
                                )
                                .increment(1);
                            }
                            MonitorDeliveryOutcome::Filtered
                            | MonitorDeliveryOutcome::NotFound => {}
                        }
                    }
                    TransportEvent::AccessRightsChanged { cid, access } => {
                        if let Some(ch) = channels.get_mut(&cid) {
                            ch.access_rights = access;
                            let _ = ch.conn_tx.send(ConnectionEvent::AccessRightsChanged {
                                read: access.read,
                                write: access.write,
                            });
                            if let Some(mut snap) = snapshots.get_mut(&cid) {
                                snap.access_rights = access;
                            }
                        }
                    }
                    TransportEvent::ChannelCreateFailed { cid } => {
                        if let Some(ch) = channels.get_mut(&cid) {
                            let server_addr = ch.server_addr;
                            // Keep connect waiters pending. ChannelCreateFailed
                            // only means this specific attempt/server failed;
                            // the channel will immediately re-search and may
                            // still connect before the caller's timeout.
                            ch.state = ChannelState::Disconnected;
                            snapshots.remove(&cid);
                            let _ = ch.conn_tx.send(ConnectionEvent::Disconnected);
                            let _ = search_tx.send(SearchRequest::Schedule {
                                cid,
                                pv_name: ch.pv_name.to_string(),
                                reason: SearchReason::Reconnect,
                            });
                            // Notify search engine of failed connect (penalty box).
                            if let Some(addr) = server_addr {
                                let _ = search_tx.send(SearchRequest::ConnectResult {
                                    cid,
                                    success: false,
                                    server_addr: addr,
                                });
                            }
                        }
                    }
                    TransportEvent::TcpClosed { server_addr, priority } => {
                        let circuit = (server_addr, priority);
                        let n_affected = server_channels
                            .get(&circuit)
                            .map(|s| s.len())
                            .unwrap_or(0);
                        tracing::warn!(server = %server_addr, priority, channels = n_affected, "TCP circuit closed");
                        metrics::counter!("ca_client_tcp_closed_total", "server" => server_addr.to_string()).increment(1);
                        last_rx_at.remove(&circuit);
                        server_minor_version.remove(&circuit);
                        handle_disconnect(&mut channels, &mut subscriptions, &mut server_channels, &search_tx, circuit, &diag, &in_flight, &snapshots, &exception_slot);
                    }
                    TransportEvent::ServerDisconnect { cid, server_addr } => {
                        handle_server_disconnect(
                            &mut channels,
                            &mut subscriptions,
                            &search_tx,
                            cid,
                            server_addr,
                            &in_flight,
                            &snapshots,
                            &exception_slot,
                        );
                    }
                    TransportEvent::CircuitUnresponsive { server_addr, priority } => {
                        diag.unresponsive_events.fetch_add(1, Ordering::Relaxed);
                        diag.record(DiagEvent::Unresponsive { server: server_addr });
                        tracing::warn!(server = %server_addr, priority, "circuit unresponsive (echo timeout)");
                        metrics::counter!("ca_client_unresponsive_total", "server" => server_addr.to_string()).increment(1);
                        // C `tcpiiu::unresponsiveCircuitNotify`
                        // (`tcpiiu.cpp:899-941`) fires the global exception
                        // hook with ECA_UNRESPTMO — gated, like the
                        // circuit-gone raise, on the circuit still having
                        // connected channels (`tcpiiu.cpp:922`
                        // `connectedList.count()`) — and then walks each of
                        // them through `nciu::unresponsiveCircuitNotify`.
                        if channels.values().any(|ch| {
                            ch.server_addr == Some(server_addr)
                                && ch.priority == priority
                                && ch.state == ChannelState::Connected
                        }) {
                            types::dispatch_exception(
                                &exception_slot,
                                types::unresponsive_circuit_exception(server_addr),
                            );
                        }
                        // Same owner as the two circuit-gone paths: the
                        // ECA_DISCONN fan-out to in-flight reads / writes /
                        // subscriptions and the no-rights transition are the
                        // shared part; only the terminal state and the
                        // connection event differ.
                        disconnect_channels(
                            &mut channels,
                            &mut subscriptions,
                            &in_flight,
                            &snapshots,
                            DisconnectKind::Unresponsive,
                            |ch| {
                                ch.server_addr == Some(server_addr)
                                    && ch.priority == priority
                                    && ch.state == ChannelState::Connected
                            },
                            |_| {},
                        );
                    }
                    TransportEvent::CircuitResponsive { server_addr, priority } => {
                        diag.record(DiagEvent::Responsive { server: server_addr });
                        tracing::info!(server = %server_addr, priority, "circuit responsive again");
                        // C `tcpiiu::responsiveCircuitNotify`
                        // (`libca/tcpiiu.cpp:861-877`) walks every
                        // formerly-unresponsive channel, calls
                        // `pChan->connect()` to move it through
                        // `subscripUpdateReqPend`, and the send thread
                        // then issues a fresh `READ_NOTIFY` per active
                        // subscription (`tcpiiu.cpp:1610-1644`
                        // `subscriptionUpdateRequest`) so the
                        // subscriber sees the post-recovery value.
                        // Pre-fix Rust only flipped state; values
                        // changed during the gap remained invisible.
                        let mut recovered_cids: Vec<u32> = Vec::new();
                        for ch in channels.values_mut() {
                            if ch.server_addr == Some(server_addr)
                                && ch.priority == priority
                                && ch.state == ChannelState::Unresponsive
                            {
                                ch.state = ChannelState::Connected;
                                if let Some(mut snap) = snapshots.get_mut(&ch.cid) {
                                    snap.state = ChannelState::Connected;
                                }
                                let _ = ch.conn_tx.send(ConnectionEvent::Connected);
                                recovered_cids.push(ch.cid);
                            }
                        }
                        for cid in recovered_cids {
                            for sub_id in subscriptions.for_cid(cid) {
                                if let Some(rec) = subscriptions.get(sub_id) {
                                    if let (Some(data_type), Some(_count)) =
                                        (rec.data_type, rec.count)
                                    {
                                        if let Some(ch) = channels.get(&cid) {
                                            if let Some(addr) = ch.server_addr {
                                                let peer_minor = server_minor_version
                                                    .get(&(addr, ch.priority))
                                                    .copied()
                                                    .unwrap_or(0);
                                                match crate::protocol::read_notify_wire_count(
                                                    rec.count.unwrap_or(0),
                                                    ch.element_count,
                                                    data_type,
                                                    peer_minor,
                                                ) {
                                                    Ok(count) => {
                                                        // C issues this
                                                        // READ_NOTIFY under
                                                        // the subscription's
                                                        // own id
                                                        // (`tcpiiu.cpp:1636-1641`)
                                                        // so the reply reaches
                                                        // the monitor
                                                        // callback. The port's
                                                        // ioid space is
                                                        // separate, so record
                                                        // the reply's owner.
                                                        let ioid = in_flight
                                                            .register_sub_update(cid, sub_id);
                                                        let _ = transport_tx.send(
                                                            TransportCommand::ReadNotify {
                                                                sid: ch.sid,
                                                                data_type,
                                                                count,
                                                                ioid,
                                                                server_addr: addr,
                                                                priority: ch.priority,
                                                            },
                                                        );
                                                    }
                                                    Err(e) => {
                                                        tracing::error!(
                                                            pv = %ch.pv_name,
                                                            error = %e,
                                                            "skipping post-recovery re-read: \
                                                             request exceeds what this circuit \
                                                             can carry"
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    TransportEvent::ServerVersion { server_addr, priority, minor_version } => {
                        // libca exposes this via `ca_host_minor_protocol`
                        // (BUG_ARCHAEOLOGY d763541): used by gateways /
                        // nameservers to report the connected server's
                        // CA wire version. Read from CA_PROTO_VERSION
                        // during TCP handshake; cleared on TcpClosed.
                        server_minor_version.insert((server_addr, priority), minor_version);
                    }
                    #[cfg(feature = "client")]
                    TransportEvent::ServerConnected { server_addr } => {
                        // C hands the peer address to `ipAddrToAsciiEngine` in
                        // the `tcpiiu` constructor, so the name is resolved
                        // long before anything prints it. Warm the same cache
                        // here — otherwise the first ask is the exception block
                        // of a circuit that just died, and it would print the
                        // dotted IP where C prints `localhost:5064`.
                        crate::hostname::warm(server_addr);
                        // libca bhe-on-connect parity: tell the beacon
                        // monitor to drop its per-server EMA so the
                        // next beacon reseeds `period_estimate` from
                        // the live cadence. Without this, an archiver
                        // that reconnects to a server in the middle of
                        // its `online_notify_task` ramp-up would log a
                        // short-period anomaly cascade against its stale
                        // steady-state estimate.
                        #[cfg(ca_beacon_monitor)]
                        let _ = beacon_ctrl_tx.send(
                            beacon_monitor::BeaconControl::ResetServer { server_addr },
                        );
                    }
                }
            }
        }
    }
}

/// Which way a channel leaves the connected set. The two variants differ
/// only in the terminal [`ChannelState`] and the snapshot treatment — i.e.
/// in how the channel *recovers*. What the application sees is identical
/// and deliberately so: C reaches `nciu::disconnectNotify` (`CA_OP_CONN_DOWN`)
/// from both, so [`ConnectionEvent::Disconnected`] is the only event either
/// path can emit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DisconnectKind {
    /// The circuit is gone: the socket closed (`TcpClosed`,
    /// C `tcpiiu::disconnectAllChannels`) or the server retired this cid
    /// (`CA_PROTO_SERVER_DISCONN`, C `cac::disconnectChannel`). Recovery is
    /// a fresh search.
    CircuitGone,
    /// Echo timeout: the socket is still up but the peer is silent.
    /// C `tcpiiu::unresponsiveCircuitNotify`. Recovery is
    /// `tcpiiu::responsiveCircuitNotify` on the same socket — no re-search.
    Unresponsive,
}

impl DisconnectKind {
    fn terminal_state(self) -> ChannelState {
        match self {
            Self::CircuitGone => ChannelState::Disconnected,
            Self::Unresponsive => ChannelState::Unresponsive,
        }
    }
}

/// THE owner of every "channel leaves the connected set" transition.
///
/// **Invariant (MUST):** a channel that leaves the connected set — by TCP
/// close, by `CA_PROTO_SERVER_DISCONN`, or by echo timeout — has its
/// pending get/put IOs and its subscriptions failed with `ECA_DISCONN`,
/// its cached access rights reset to no-rights, and emits the connection
/// event followed by `AccessRightsChanged { read: false, write: false }`.
/// **MUST NOT:** any of those steps happen on one path and not another.
///
/// In C all three paths converge on `nciu::unresponsiveCircuitNotify`
/// (`nciu.cpp:161-182`), reached from `tcpiiu::disconnectAllChannels`
/// (`tcpiiu.cpp:1848-1855`), from `cac::disconnectChannel`
/// (`cac.cpp:1185-1195`) and from the echo-timeout path — so they cannot
/// disagree. Rust hand-rolled the transition three times and did disagree:
/// only the echo-timeout path reset access rights, so after a socket close
/// the cached rights stayed at their last value (possibly read+write) until
/// reconnect (R8-17).
///
/// `select` picks the channels this transition covers; `per_channel` runs
/// the path-specific extras (reconnect backoff, re-search scheduling) that
/// emit no user-visible callback. Returns the affected cids.
fn disconnect_channels(
    channels: &mut HashMap<u32, ChannelInner>,
    subscriptions: &mut SubscriptionRegistry,
    in_flight: &types::InFlightOps,
    snapshots: &ChannelSnapshots,
    kind: DisconnectKind,
    select: impl Fn(&ChannelInner) -> bool,
    mut per_channel: impl FnMut(&mut ChannelInner),
) -> Vec<u32> {
    let affected: Vec<u32> = channels
        .values()
        .filter(|ch| select(ch))
        .map(|ch| ch.cid)
        .collect();
    if affected.is_empty() {
        return affected;
    }

    const NO_RIGHTS: AccessRights = AccessRights {
        read: false,
        write: false,
    };

    // Step 1 — `disconnectAllIO` (`nciu.cpp:168`), and it comes FIRST:
    // every pending get/put IO of the channel fails with ECA_DISCONN and
    // the subscriptions park the same status, all of it *before* the
    // connection callback runs (`nciu.cpp:168-170` — disconnectAllIO, then
    // disconnectNotify). libca hands the user the per-IO failures before it
    // says "the channel is down", so a handler that reacts to the
    // connection event never observes an IO of the dead channel still
    // pending.
    subscriptions.mark_disconnected(&affected);
    let affected_set: HashSet<u32> = affected.iter().copied().collect();
    drain_waiters_for_cids(&affected_set, in_flight);

    for cid in &affected {
        let Some(ch) = channels.get_mut(cid) else {
            continue;
        };
        // Step 2 — C `nciu::setServerAddressUnknown` (`nciu.cpp:183-195`)
        // clears both permits as the channel leaves the circuit, before any
        // callback runs, so a handler that reads the rights sees no-rights.
        ch.state = kind.terminal_state();
        ch.access_rights = NO_RIGHTS;
        match kind {
            DisconnectKind::CircuitGone => {
                snapshots.remove(cid);
            }
            DisconnectKind::Unresponsive => {
                if let Some(mut snap) = snapshots.get_mut(cid) {
                    snap.state = ChannelState::Unresponsive;
                    snap.access_rights = NO_RIGHTS;
                }
            }
        }
        // Steps 3 and 4 — C `disconnectNotify` then
        // `accessRightsNotify(noRights)` (`nciu.cpp:170-177`), in that
        // order. Both kinds emit `Disconnected`: C has no third connection
        // state, and an event only the port can emit is an event no
        // consumer handles.
        let _ = ch.conn_tx.send(ConnectionEvent::Disconnected);
        let _ = ch.conn_tx.send(ConnectionEvent::AccessRightsChanged {
            read: false,
            write: false,
        });
        per_channel(ch);
    }

    affected
}

/// `CA_PROTO_SERVER_DISCONN`: the server retired one cid.
///
/// C `cac::verifyAndDisconnectChan` → `cac::disconnectChannel`
/// (`cac.cpp:1172-1195`) runs the SAME transition as a circuit close —
/// `disconnectAllIO`, no-rights, `disconnectNotify`, `accessRightsNotify` —
/// only scoped to one channel. It therefore crosses the same owner
/// ([`disconnect_channels`]) as `TcpClosed` and the echo timeout.
#[allow(clippy::too_many_arguments)]
fn handle_server_disconnect(
    channels: &mut HashMap<u32, ChannelInner>,
    subscriptions: &mut SubscriptionRegistry,
    search_tx: &mpsc::UnboundedSender<SearchRequest>,
    cid: u32,
    server_addr: SocketAddr,
    in_flight: &types::InFlightOps,
    snapshots: &ChannelSnapshots,
    exception_slot: &types::CaExceptionSlot,
) {
    let mut pv_name = String::new();
    let affected = disconnect_channels(
        channels,
        subscriptions,
        in_flight,
        snapshots,
        DisconnectKind::CircuitGone,
        |ch| ch.cid == cid && ch.server_addr == Some(server_addr),
        |ch| {
            pv_name = ch.pv_name.to_string();
            let _ = search_tx.send(SearchRequest::Schedule {
                cid: ch.cid,
                pv_name: ch.pv_name.to_string(),
                reason: SearchReason::Reconnect,
            });
        },
    );
    if affected.is_empty() {
        return;
    }
    // CA-130: surface to per-client handler.
    types::dispatch_exception(
        exception_slot,
        types::CaException {
            kind: types::CaExceptionKind::ServerDisconnect,
            message: "server-initiated channel close".to_string(),
            server_addr: Some(server_addr),
            pv_name: Some(pv_name),
            status: None,
            op: types::CaOp::Other,
            data_type: None,
            count: None,
            // No C site to name: `CA_PROTO_SERVER_DISCONN` never reaches
            // `vSignal` at all (`cac::verifyAndDisconnectChan` takes it out
            // through the channel's connection callback), so this kind renders
            // to nothing. The field is unread here.
            source: types::ExceptionSite::NullFile,
        },
    );
}

#[allow(clippy::too_many_arguments)]
fn handle_disconnect(
    channels: &mut HashMap<u32, ChannelInner>,
    subscriptions: &mut SubscriptionRegistry,
    server_channels: &mut HashMap<types::CircuitKey, HashSet<u32>>,
    search_tx: &mpsc::UnboundedSender<SearchRequest>,
    circuit: types::CircuitKey,
    diag: &CaDiagnostics,
    in_flight: &types::InFlightOps,
    snapshots: &ChannelSnapshots,
    exception_slot: &types::CaExceptionSlot,
) {
    let (server_addr, priority) = circuit;
    let now = std::time::Instant::now();

    // C `cac::destroyIIU` (`cac.cpp:1236-1240`): a circuit that still carried
    // channels when it died raises ECA_DISCONN on the global exception hook —
    // and raises it *before* `disconnectAllChannels` (`cac.cpp:1251`), so a
    // handler learns the circuit is gone ahead of the per-channel callbacks.
    // It belongs here, in the circuit-gone owner, not at one call site: every
    // path that destroys a circuit is a path C raises this on.
    if server_channels
        .get(&circuit)
        .is_some_and(|cids| !cids.is_empty())
    {
        types::dispatch_exception(
            exception_slot,
            types::circuit_disconnect_exception(server_addr),
        );
    }

    // only channels on THIS circuit `(server_addr, priority)`
    // are torn down — a sibling circuit to the same server at another
    // priority keeps its channels connected.
    let affected_cids = disconnect_channels(
        channels,
        subscriptions,
        in_flight,
        snapshots,
        DisconnectKind::CircuitGone,
        |ch| {
            ch.server_addr == Some(server_addr)
                && ch.priority == priority
                && (ch.state.is_operational() || ch.state == ChannelState::Connecting)
        },
        |ch| {
            // Reconnection backoff: if the connection was short-lived (<30s),
            // increment reconnect_count for exponential backoff. Sustained
            // connections reset the counter.
            let sustained = ch
                .last_connected_at
                .map(|t| now.duration_since(t).as_secs() > 30)
                .unwrap_or(false);
            if sustained {
                ch.reconnect_count = 0;
            } else {
                ch.reconnect_count = ch.reconnect_count.saturating_add(1);
            }
            // Bucket scheduler distributes Reconnect searches by cid hash
            // across all 30 buckets — naturally prevents the reconnection
            // storm the legacy lane scheduler had to dampen by setting
            // `initial_lane = reconnect_count.clamp(1, 8)`.
            let _ = search_tx.send(SearchRequest::Schedule {
                cid: ch.cid,
                pv_name: ch.pv_name.to_string(),
                reason: SearchReason::Reconnect,
            });
        },
    );
    if !affected_cids.is_empty() {
        diag.disconnections.fetch_add(1, Ordering::Relaxed);
        diag.record(DiagEvent::Disconnected {
            server: server_addr,
            channels: affected_cids.len(),
        });
        tracing::warn!(
            server = %server_addr,
            affected = affected_cids.len(),
            "disconnect: scheduling reconnect for affected channels"
        );
        metrics::counter!("ca_client_disconnections_total", "server" => server_addr.to_string())
            .increment(1);
        metrics::gauge!("ca_client_channels_connected").decrement(affected_cids.len() as f64);
    }
    // Clean up stale server_channels entries so beacon anomaly
    // lookups don't reference disconnected channels.
    server_channels.remove(&circuit);
}

/// Drop every entry in the shared in-flight registry whose cid is in
/// `cids` and signal each Sender with `Err(CaError::Disconnected)`. Used
/// by both bulk-disconnect (TcpClosed → handle_disconnect) and the
/// per-cid SERVER_DISCONN path so blocked `caget` / `caput` futures
/// surface as disconnect errors instead of stalling on the caller's
/// outer timeout. Phase A: ops live in `InFlightOps` (DashMap), no
/// longer in coordinator-local HashMaps.
pub(crate) fn drain_waiters_for_cids(cids: &HashSet<u32>, in_flight: &types::InFlightOps) {
    let stale_reads: Vec<u32> = in_flight
        .reads
        .iter()
        .filter(|entry| cids.contains(&entry.value().cid()))
        .map(|entry| *entry.key())
        .collect();
    for ioid in stale_reads {
        if let Some((_, waiter)) = in_flight.reads.remove(&ioid) {
            waiter.send(Err(CaError::Disconnected));
        }
    }
    let stale_writes: Vec<u32> = in_flight
        .writes
        .iter()
        .filter(|entry| cids.contains(&entry.value().0))
        .map(|entry| *entry.key())
        .collect();
    for ioid in stale_writes {
        if let Some((_, (_, sender))) = in_flight.writes.remove(&ioid) {
            let _ = sender.send(Err(CaError::Disconnected));
        }
    }
    // The recovery re-reads of a channel that just left the connected set
    // will never be answered; their ECA_DISCONN reaches the subscriber via
    // `mark_disconnected`, so drop the records rather than leak them.
    in_flight
        .sub_updates
        .retain(|_, (cid, _)| !cids.contains(cid));
}

/// Decide which operational circuits should receive a
/// `BeaconArrivalNotify` for a beacon announced from `beacon_addr`.
///
/// Common (and cheapest) path: the beacon's announced address
/// matches an operational circuit's stored `server_addr` exactly,
/// in which case we deliver to just that circuit.
///
/// Two fallbacks share a single port-only matching path:
///   * INADDR_ANY (`beacon_addr.ip().is_unspecified()`) — the IOC
///     sent `available = 0` and the upstream repeater didn't
///     rewrite it. There's no exact match to find.
///   * Multi-homed IOC — the beacon arrived through NIC A and the
///     search-reply that established the circuit came from NIC B.
///     The announced address is `A:port`, the circuit's stored
///     address is `B:port`, so exact-match silently misses; we
///     match by port instead.
///
/// Cross-host port collisions (two unrelated IOCs both on port
/// 5064 across different machines, both with operational
/// circuits) cause a benign false-refresh: the wrong circuit's
/// deadline gets pushed by 30 s, but its own watchdog still
/// detects death within 30 + 5 s if it actually died. We accept
/// that trade to recover correct behaviour on the multi-homed
/// case, which is real and was previously a silent regression.
///
/// The returned `Vec` is what the coordinator forwards to the
/// transport manager — one `BeaconArrivalNotify` per element.
#[cfg(ca_beacon_monitor)]
fn beacon_arrival_targets<I>(channel_states: I, beacon_addr: SocketAddr) -> Vec<SocketAddr>
where
    I: IntoIterator<Item = (ChannelState, Option<SocketAddr>)>,
{
    let beacon_unspec = beacon_addr.ip().is_unspecified();
    let mut found_exact = false;
    let mut port_targets: HashSet<SocketAddr> = HashSet::new();

    for (state, addr_opt) in channel_states {
        if !state.is_operational() {
            continue;
        }
        let Some(addr) = addr_opt else { continue };
        if !beacon_unspec && addr == beacon_addr {
            // Exact match dominates — don't bother collecting
            // port matches we'd discard. Order of HashMap
            // iteration is non-deterministic but this break is
            // safe regardless: when we know the exact circuit
            // exists, we never look at the fallback set.
            found_exact = true;
            break;
        }
        if addr.port() == beacon_addr.port() {
            port_targets.insert(addr);
        }
    }

    if found_exact {
        vec![beacon_addr]
    } else {
        port_targets.into_iter().collect()
    }
}

/// Count distinct operational virtual circuits for
/// `ca_get_ioc_connection_count`. libca keys its circuit table on
/// `(address, CA priority)` (`caServerID::operator==`,
/// `caServerID.h:47-55`) and the count is `circuitList.count`
/// (`cac::circuitCount`, `cac.cpp:403`), i.e. the number of `tcpiiu`
/// circuits — so two channels to the same IOC at different CA
/// priorities are two circuits. We therefore dedup on the full
/// [`types::CircuitKey`] `(addr, priority)`, not the address alone
/// (which under-counted once circuits are split by priority).
/// Extracted as a free function so the count is unit-testable without
/// standing up the coordinator (mirrors `beacon_arrival_targets`).
fn operational_circuit_count<I>(channel_states: I) -> usize
where
    I: IntoIterator<Item = (ChannelState, Option<SocketAddr>, u8)>,
{
    let mut circuits = HashSet::<types::CircuitKey>::new();
    for (state, addr_opt, priority) in channel_states {
        if !state.is_operational() {
            continue;
        }
        let Some(addr) = addr_opt else { continue };
        circuits.insert((addr, priority));
    }
    circuits.len()
}

fn remove_server_channel(
    server_channels: &mut HashMap<types::CircuitKey, HashSet<u32>>,
    circuit: types::CircuitKey,
    cid: u32,
) {
    if let Some(set) = server_channels.get_mut(&circuit) {
        set.remove(&cid);
        if set.is_empty() {
            server_channels.remove(&circuit);
        }
    }
}

/// Resolve one `EPICS_CA_ADDR_LIST` SEARCH destination.
/// `EPICS_CA_NAME_SERVERS` entries go through `parse_nameserver_list` instead.
fn resolve_host(host: &str, port: u16) -> CaResult<SocketAddr> {
    // Try direct IP parse first (fast path)
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)));
    }
    // DNS resolution — prefer IPv4 (CA protocol is IPv4-only)
    use std::net::ToSocketAddrs;
    let addr_str = format!("{host}:{port}");
    let addrs: Vec<SocketAddr> = addr_str
        .to_socket_addrs()
        .map_err(|e| CaError::Protocol(format!("cannot resolve '{host}': {e}")))?
        .collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .copied()
        .ok_or_else(|| CaError::Protocol(format!("no addresses for '{host}'")))
}

/// One entry in `EPICS_CA_ADDR_LIST` with its original DNS form
/// retained — addresses the Launchpad #488 / GitHub #862/#863 family
/// where startup-time-only DNS resolution leaves stale IPs after a
/// peer restarts on a new IP.
///
/// `hostname == None` means the entry was a literal IPv4; nothing to
/// re-resolve. `hostname == Some(name)` means the entry started life
/// as a DNS name and a reconnection path may call `resolve_host`
/// again to refresh `sock`.
#[derive(Debug, Clone)]
pub(crate) struct AddrEntry {
    pub sock: SocketAddr,
    pub hostname: Option<String>,
    pub port: u16,
    /// TLS-only: SNI registry this entry publishes fresh resolutions
    /// into. Attached ([`AddrEntry::with_sni`]) only to
    /// `EPICS_CA_NAME_SERVERS` entries; `None` for addr-list entries.
    #[cfg(feature = "experimental-rust-tls")]
    sni: Option<std::sync::Arc<SniOverrides>>,
}

impl AddrEntry {
    pub fn new(sock: SocketAddr, hostname: Option<String>, port: u16) -> Self {
        Self {
            sock,
            hostname,
            port,
            #[cfg(feature = "experimental-rust-tls")]
            sni: None,
        }
    }

    /// Publish this entry's future [`refresh_dns`](Self::refresh_dns)
    /// resolutions into `sni`, so a TLS data circuit to the re-resolved
    /// address still finds the operator-supplied hostname.
    #[cfg(feature = "experimental-rust-tls")]
    pub fn with_sni(mut self, sni: std::sync::Arc<SniOverrides>) -> Self {
        self.sni = Some(sni);
        self
    }

    /// Re-resolve the hostname (if any) and return the freshened
    /// SocketAddr. Returns `Ok(self.sock)` unchanged when there's
    /// nothing to refresh (literal IP entry).
    ///
    /// Wired into the search engine's periodic
    /// refresh task. The task runs every `EPICS_CA_DNS_REFRESH_SECS`
    /// (default 60 s) and calls this on every `AddrEntry`; a
    /// changed resolution updates `self.sock` so the next
    /// `fire_searches` batch uses the fresh IP.
    pub fn refresh_dns(&mut self) -> CaResult<SocketAddr> {
        let Some(host) = self.hostname.as_deref() else {
            return Ok(self.sock);
        };
        let new_sock = resolve_host(host, self.port)?;
        self.sock = new_sock;
        // The one place a tracked hostname is ever resolved, so the SNI
        // registry row can never lag the address actually being dialed.
        #[cfg(feature = "experimental-rust-tls")]
        if let Some(sni) = &self.sni {
            sni.record_resolution(host, new_sock);
        }
        Ok(new_sock)
    }
}

/// `parse_addr_list` variant that retains hostname per entry.
/// This is the live caller from
/// `new_with_config()`; the search engine periodically calls
/// `AddrEntry::refresh_dns` on each entry so that an
/// `EPICS_CA_ADDR_LIST` hostname whose DNS resolution changes
/// (e.g. an IOC migrates between hosts) is picked up at runtime
/// instead of permanently pinning the client to the first-resolved
/// IP. Closes the long-standing upstream-tracking item for
/// epics-base#488.
pub(crate) fn parse_addr_list_with_hostnames() -> CaResult<Vec<AddrEntry>> {
    let mut addrs: Vec<AddrEntry> = Vec::new();
    let default_port = epics_base_rs::runtime::net::ca_server_port();
    // EPICS_RS_CLIENT_IGNORE — Rust-only client-side IP quarantine
    // (NOT the C EPICS_IOC_IGNORE_SERVERS, which is server-side and
    // operates on server NAMES, not IPs — see epics_rs_client_ignore
    // docstring for the naming rationale).
    let ignore_ips = epics_rs_client_ignore();

    // C `configureChannelAccessAddressList` (`iocinf.cpp:195-227`) builds this
    // list in ONE order: the auto-discovered broadcasts first (deduped among
    // themselves, silently), the operator's `EPICS_CA_ADDR_LIST` appended
    // after them, and a single `removeDuplicateAddresses(…, silent=0)` over the
    // result. The port had the two halves the other way round, so an operator
    // entry that names a broadcast address C would report as a duplicate would
    // instead have silenced the auto entry.
    // C `ca/src/client/iocinf.cpp:186-193` uses substring
    // semantics: `yes = true; if (strstr(pstr,"no") || strstr(pstr,
    // "NO")) yes = false`. Any value not containing "no"/"NO" keeps
    // auto-discovery enabled — so `"1"`, `"true"`, `"on"`, `"bogus"`
    // all enable on C. Pre-fix Rust used strict `eq_ignore_ascii_case
    // ("YES")`, silently disabling auto-discovery on those values.
    // Server-side `addr_list::from_env` keeps the strict semantic
    // because the C server var uses `envGetBoolConfigParam` (strict);
    // only the CLIENT var has the strstr quirk that Rust must mirror.
    let auto_addr = epics_base_rs::runtime::env_table::EPICS_CA_AUTO_ADDR_LIST
        .get()
        .unwrap_or_default();
    let auto_addr_enabled = !(auto_addr.contains("no") || auto_addr.contains("NO"));
    if auto_addr_enabled {
        let bcasts = crate::server::addr_list::discover_broadcast_addrs();
        append_auto_addr_entries(&mut addrs, &bcasts, default_port);
    }

    if let Some(list) = epics_base_rs::runtime::env_table::EPICS_CA_ADDR_LIST.get() {
        // C `iocinf.cpp:225`. The tokenizer owns the bad-token diagnostic; the
        // only thing left to do here is the Rust-only IP quarantine.
        for tok in crate::iocinf::add_addr_to_channel_access_address_list(
            &list,
            "EPICS_CA_ADDR_LIST",
            default_port,
        ) {
            if let SocketAddr::V4(v4) = tok.sock {
                if ignore_ips.contains(v4.ip()) {
                    tracing::debug!(
                        target: "epics_ca_rs::client",
                        ip = %v4.ip(),
                        "EPICS_RS_CLIENT_IGNORE: dropping ADDR_LIST entry"
                    );
                    continue;
                }
            }
            addrs.push(AddrEntry::new(tok.sock, tok.hostname, tok.port));
        }
    }

    // C `iocinf.cpp:227`: `removeDuplicateAddresses ( pList, &tmpList, 0 )`.
    // The port kept every duplicate token, so `EPICS_CA_ADDR_LIST="A A"` sent
    // two copies of every search datagram to A for the life of the client.
    Ok(crate::iocinf::remove_duplicate_addresses(addrs, |e| e.sock))
}

/// AUTO_ADDR_LIST=YES expansion: append broadcast NICs, then a C-parity
/// loopback fallback (when bcast enumeration is empty), then the
/// Rust-only limited-broadcast safety net. Extracted from
/// `parse_addr_list_with_hostnames` so the bcast list can be injected
/// in unit tests (real-NIC enumeration is environment-dependent).
fn append_auto_addr_entries(addrs: &mut Vec<AddrEntry>, bcasts: &[Ipv4Addr], server_port: u16) {
    for bcast in bcasts {
        let sock = SocketAddr::V4(SocketAddrV4::new(*bcast, server_port));
        if !addrs.iter().any(|e| e.sock == sock) {
            addrs.push(AddrEntry::new(sock, None, server_port));
        }
    }
    // C parity (libca `iocinf.cpp::configureChannelAccessAddressList`,
    // lines 199-223): when AUTO=YES AND broadcast-interface
    // enumeration returns nothing, add `INADDR_LOOPBACK` so a
    // pure-localhost setup (CI container, single-NIC dev box, host
    // with only `lo`) still surfaces an in-host IOC. Without this,
    // the only fallback below (`255.255.255.255`) reaches no
    // interface on a host without a routable subnet, and
    // localhost-only IOCs become invisible. Distinct from the
    // explicit `EPICS_CA_ADDR_LIST` path above — that one already
    // handles operators who name their loopback IOC explicitly.
    if bcasts.is_empty() {
        let loopback = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port));
        if !addrs.iter().any(|e| e.sock == loopback) {
            addrs.push(AddrEntry::new(loopback, None, server_port));
        }
    }
    // Rust-only safety net (no C analogue): limited broadcast as a
    // last-resort fallback. Multi-NIC enumeration may return only
    // unusable broadcasts (point-to-point, IPv6-only), and
    // `255.255.255.255` is the kernel's degenerate destination that
    // the routing table usually pins to the default-route NIC.
    // Cheap to include — `removeDuplicateAddresses` upstream is the
    // CA-side dedupe, ours is the `!any` guard above.
    let fallback = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, server_port));
    if !addrs.iter().any(|e| e.sock == fallback) {
        addrs.push(AddrEntry::new(fallback, None, server_port));
    }
}

#[cfg(test)]
mod enum_readback_tests {
    use super::*;

    // the readback-type rule for the CLI tools. Covered by
    // invariant boundary (ENUM vs not × want_time × enum_as_string),
    // not by per-tool scenario.
    #[test]
    fn enum_string_when_opted_in() {
        // ENUM + opt-in → STRING form; TIME class iff want_time.
        assert_eq!(
            enum_string_readback_dbr(DbFieldType::Enum, true, true),
            Some(DBR_TIME_STRING)
        );
        assert_eq!(
            enum_string_readback_dbr(DbFieldType::Enum, false, true),
            Some(DBR_STRING)
        );
    }

    #[test]
    fn native_when_opted_out_or_not_enum() {
        // ENUM but `-n` / library default → keep native (None).
        assert_eq!(
            enum_string_readback_dbr(DbFieldType::Enum, true, false),
            None
        );
        assert_eq!(
            enum_string_readback_dbr(DbFieldType::Enum, false, false),
            None
        );
        // Non-ENUM fields are never substituted, even when opted in.
        assert_eq!(
            enum_string_readback_dbr(DbFieldType::Double, true, true),
            None
        );
        assert_eq!(
            enum_string_readback_dbr(DbFieldType::Long, false, true),
            None
        );
    }

    // `-s` (floatAsString) readback substitution. Boundary axes:
    // FLOAT/DOUBLE vs other native types (C only substitutes the two
    // float kinds, caget.c:183-187 / camonitor.c:162-166).
    #[test]
    fn float_as_string_only_for_float_and_double() {
        assert_eq!(
            float_as_string_readback_dbr(DbFieldType::Float),
            Some(DBR_TIME_STRING)
        );
        assert_eq!(
            float_as_string_readback_dbr(DbFieldType::Double),
            Some(DBR_TIME_STRING)
        );
        // Every non-float native type keeps its native path.
        for t in [
            DbFieldType::String,
            DbFieldType::Short,
            DbFieldType::Enum,
            DbFieldType::Char,
            DbFieldType::Long,
        ] {
            assert_eq!(float_as_string_readback_dbr(t), None, "{t:?}");
        }
    }

    // The caget/camonitor
    // ENUM substitution always replaces native ENUM (C `caget.c:177-181` /
    // `camonitor.c:156-162`): `-n` → DBR_TIME_INT, otherwise DBR_TIME_STRING.
    // Both branches are TIME class regardless of output mode; non-ENUM is
    // None so the caller keeps its native path.
    #[test]
    fn enum_cli_readback_substitutes_int_or_string() {
        // `-n` (enum_as_number) → DBR_TIME_INT, never native DBR_TIME_ENUM.
        assert_eq!(
            enum_cli_readback_dbr(DbFieldType::Enum, true),
            Some(DBR_TIME_INT)
        );
        // Default (labels) → DBR_TIME_STRING.
        assert_eq!(
            enum_cli_readback_dbr(DbFieldType::Enum, false),
            Some(DBR_TIME_STRING)
        );
        // Non-ENUM fields are never substituted here (caller keeps native).
        for t in [
            DbFieldType::String,
            DbFieldType::Short,
            DbFieldType::Float,
            DbFieldType::Double,
            DbFieldType::Char,
            DbFieldType::Long,
        ] {
            assert_eq!(enum_cli_readback_dbr(t, true), None, "{t:?} -n");
            assert_eq!(enum_cli_readback_dbr(t, false), None, "{t:?} default");
        }
    }

    // The unified subscribe-time chain, by invariant boundary:
    // EnumReadback {Native, Label, Numeric} × float-substitution × native
    // fallback, mirroring C `if (ENUM) … else if (float …) … else native`.
    #[test]
    fn subscription_readback_chain_precedence() {
        // ENUM + Label (camonitor default) wins even when float_as_string is
        // also set (C tests the ENUM case before the float `else if`).
        assert_eq!(
            subscription_readback_dbr(DbFieldType::Enum, EnumReadback::Label, true),
            DBR_TIME_STRING
        );
        // ENUM + Numeric (`camonitor -n`) → DBR_TIME_INT (numeric index),
        // regardless of float_as_string (ENUM is not a float kind). C
        // `camonitor.c:158` `if (enumAsNr) ppv->dbrType = DBR_TIME_INT` — an
        // ENUM is never left on the native DBR_TIME_ENUM type under `-n`.
        assert_eq!(
            subscription_readback_dbr(DbFieldType::Enum, EnumReadback::Numeric, true),
            DBR_TIME_INT
        );
        // ENUM + Native (the plain library/gateway subscribe) keeps the
        // native ENUM TIME type — NOT substituted to INT/STRING — even when
        // float_as_string is set. This is the library contract the bool
        // `enum_as_string=false` conflated with `-n`: a sum type keeps
        // Native distinct from Numeric.
        assert_eq!(
            subscription_readback_dbr(DbFieldType::Enum, EnumReadback::Native, true),
            DbFieldType::Enum.time_dbr_type()
        );
        // FLOAT/DOUBLE under `-s` → DBR_TIME_STRING (ENUM mode is Native, so
        // it does not pre-empt the float substitution).
        assert_eq!(
            subscription_readback_dbr(DbFieldType::Float, EnumReadback::Native, true),
            DBR_TIME_STRING
        );
        assert_eq!(
            subscription_readback_dbr(DbFieldType::Double, EnumReadback::Native, true),
            DBR_TIME_STRING
        );
        // FLOAT without `-s` → native TIME type (no substitution).
        assert_eq!(
            subscription_readback_dbr(DbFieldType::Double, EnumReadback::Native, false),
            DbFieldType::Double.time_dbr_type()
        );
        // Non-float, non-enum native type is untouched by any ENUM mode —
        // Label/Numeric only substitute ENUM fields.
        assert_eq!(
            subscription_readback_dbr(DbFieldType::Long, EnumReadback::Label, true),
            DbFieldType::Long.time_dbr_type()
        );
        assert_eq!(
            subscription_readback_dbr(DbFieldType::Long, EnumReadback::Numeric, false),
            DbFieldType::Long.time_dbr_type()
        );
    }
}

#[cfg(test)]
mod ignore_servers_tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial(epics_env)]
    fn empty_when_unset() {
        unsafe { std::env::remove_var("EPICS_RS_CLIENT_IGNORE") };
        assert!(epics_rs_client_ignore().is_empty());
    }

    #[test]
    #[serial(epics_env)]
    fn parses_ip_literals() {
        unsafe { std::env::set_var("EPICS_RS_CLIENT_IGNORE", "10.0.0.1 192.168.1.42") };
        let v = epics_rs_client_ignore();
        assert_eq!(v.len(), 2);
        assert!(v.contains(&Ipv4Addr::new(10, 0, 0, 1)));
        assert!(v.contains(&Ipv4Addr::new(192, 168, 1, 42)));
        unsafe { std::env::remove_var("EPICS_RS_CLIENT_IGNORE") };
    }

    #[test]
    #[serial(epics_env)]
    fn strips_port_suffix() {
        unsafe { std::env::set_var("EPICS_RS_CLIENT_IGNORE", "10.0.0.1:5064") };
        let v = epics_rs_client_ignore();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0], Ipv4Addr::new(10, 0, 0, 1));
        unsafe { std::env::remove_var("EPICS_RS_CLIENT_IGNORE") };
    }

    #[test]
    #[serial(epics_env)]
    fn silently_drops_garbage_entries() {
        unsafe {
            std::env::set_var(
                "EPICS_RS_CLIENT_IGNORE",
                "1.2.3.4 not-an-ip-or-host-that-resolves.invalid 5.6.7.8",
            )
        };
        let v = epics_rs_client_ignore();
        // The middle entry must be dropped (DNS resolution fails for
        // `.invalid` TLD per RFC 6761 — every resolver returns NXDOMAIN).
        assert!(v.contains(&Ipv4Addr::new(1, 2, 3, 4)));
        assert!(v.contains(&Ipv4Addr::new(5, 6, 7, 8)));
        assert_eq!(v.len(), 2);
        unsafe { std::env::remove_var("EPICS_RS_CLIENT_IGNORE") };
    }
}

// `tokio_backend`: `append_auto_addr_entries` and `AddrEntry` are the UDP
// SEARCH destination surface, compiled out on `exec_backend`.
#[cfg(all(test, tokio_backend))]
mod auto_addr_list_tests {
    use super::*;

    fn sock(ip: Ipv4Addr, port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(ip, port))
    }

    /// C parity — `iocinf.cpp::configureChannelAccessAddressList`
    /// lines 199-223 add INADDR_LOOPBACK when broadcast enumeration
    /// returns nothing. Without this, a pure-localhost host hits the
    /// 255.255.255.255 path only and loopback IOCs are invisible.
    #[test]
    fn loopback_added_when_no_broadcasts() {
        let mut addrs: Vec<AddrEntry> = Vec::new();
        append_auto_addr_entries(&mut addrs, &[], 5064);
        assert!(
            addrs
                .iter()
                .any(|e| e.sock == sock(Ipv4Addr::LOCALHOST, 5064))
        );
        assert!(
            addrs
                .iter()
                .any(|e| e.sock == sock(Ipv4Addr::BROADCAST, 5064))
        );
    }

    /// When real broadcast NICs exist, loopback is NOT injected
    /// (matches C: `if ellCount(&tmpList) == 0` gate).
    #[test]
    fn loopback_not_added_when_broadcasts_present() {
        let mut addrs: Vec<AddrEntry> = Vec::new();
        let bcasts = vec![Ipv4Addr::new(192, 168, 1, 255)];
        append_auto_addr_entries(&mut addrs, &bcasts, 5064);
        assert!(
            addrs
                .iter()
                .any(|e| e.sock == sock(Ipv4Addr::new(192, 168, 1, 255), 5064))
        );
        assert!(
            !addrs
                .iter()
                .any(|e| e.sock == sock(Ipv4Addr::LOCALHOST, 5064))
        );
        // Rust-only safety net still added.
        assert!(
            addrs
                .iter()
                .any(|e| e.sock == sock(Ipv4Addr::BROADCAST, 5064))
        );
    }

    /// Dedup: pre-populated entries (e.g. explicit ADDR_LIST) are not
    /// re-added even when AUTO=YES would otherwise propose them.
    #[test]
    fn does_not_duplicate_existing_entries() {
        let mut addrs: Vec<AddrEntry> =
            vec![AddrEntry::new(sock(Ipv4Addr::LOCALHOST, 5064), None, 5064)];
        append_auto_addr_entries(&mut addrs, &[], 5064);
        let count = addrs
            .iter()
            .filter(|e| e.sock == sock(Ipv4Addr::LOCALHOST, 5064))
            .count();
        assert_eq!(count, 1, "loopback must not be duplicated");
    }

    /// Custom EPICS_CA_SERVER_PORT carries into all synthesized
    /// entries (loopback + safety-net broadcast).
    #[test]
    fn respects_custom_server_port() {
        let mut addrs: Vec<AddrEntry> = Vec::new();
        append_auto_addr_entries(&mut addrs, &[], 5066);
        assert!(
            addrs
                .iter()
                .any(|e| e.sock == sock(Ipv4Addr::LOCALHOST, 5066))
        );
        assert!(
            addrs
                .iter()
                .any(|e| e.sock == sock(Ipv4Addr::BROADCAST, 5066))
        );
    }
}

// `tokio_backend`: `AddrEntry` is the UDP SEARCH destination type, compiled
// out on `exec_backend`.
#[cfg(all(test, tokio_backend))]
mod addr_entry_tests {
    use super::*;

    #[test]
    fn ip_literal_has_no_hostname() {
        let entry = AddrEntry::new(
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 5064)),
            None,
            5064,
        );
        assert!(entry.hostname.is_none());
    }

    #[test]
    fn refresh_noop_for_literal_ip() {
        let original_sock = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 5064));
        let mut entry = AddrEntry::new(original_sock, None, 5064);
        let refreshed = entry.refresh_dns().expect("noop refresh succeeds");
        assert_eq!(refreshed, original_sock);
    }
}

/// Apply libca-compatible PV-name expansion when
/// `EPICS_CA_USE_SHELL_VARS=YES`.
fn expand_pv_name(name: &str) -> String {
    // EPICS_CA_USE_SHELL_VARS=YES expands ${VAR}/$(VAR) tokens in PV
    // names against the process environment, matching libca behaviour.
    // Not an EPICS Base `ENV_PARAM` — it is in no `envDefs.h`, so there is no
    // compiled default and the caller owns the fallback (`getenv`-shaped).
    if epics_base_rs::runtime::env::get("EPICS_CA_USE_SHELL_VARS")
        .is_some_and(|v| v.eq_ignore_ascii_case("YES"))
    {
        expand_shell_vars(name)
    } else {
        name.to_string()
    }
}

/// Expand shell-style `${VAR}` and `$(VAR)` references in `s` against the
/// process environment. Unknown variables expand to the empty string,
/// matching libca's expandedClient behaviour. Plain `$` and unmatched
/// braces/parens are left intact.
fn expand_shell_vars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() {
            let close = match bytes[i + 1] {
                b'{' => Some(b'}'),
                b'(' => Some(b')'),
                _ => None,
            };
            if let Some(end) = close {
                if let Some(j) = bytes[i + 2..].iter().position(|&b| b == end) {
                    let name = &s[i + 2..i + 2 + j];
                    let value = epics_base_rs::runtime::env::get(name).unwrap_or_default();
                    out.push_str(&value);
                    i += 3 + j;
                    continue;
                }
            }
        }
        out.push(s.as_bytes()[i] as char);
        i += 1;
    }
    out
}

/// Live SNI / cert-verification-name registry, shared between the
/// transport manager (reader, one [`SniOverrides::lookup`] per TLS
/// connect) and every nameserver [`AddrEntry::refresh_dns`] (writer,
/// one [`SniOverrides::record_resolution`] per successful re-resolve).
///
/// Two layers, one meaning each:
/// - `static_map` — operator-pinned `EPICS_CA_TLS_SNI_MAP` rows,
///   immutable for the client's lifetime (exact `ip:port` keys plus
///   `port == 0` wildcard rows).
/// - `nameservers` — hostname → *current* DNS resolution, one row per
///   `EPICS_CA_NAME_SERVERS` entry given by name. Keyed by hostname,
///   not by resolved address, so a nameserver that moves behind a
///   stable DNS name cannot leave a stale address key behind: the same
///   `refresh_dns` that redials the new address rewrites the row.
///   (Replaces the startup-frozen `SocketAddr`-keyed snapshot — the
///   epics-base#488 residual that lost a moved nameserver's SNI
///   override until client restart.)
#[cfg(feature = "experimental-rust-tls")]
#[derive(Debug, Default)]
pub(crate) struct SniOverrides {
    static_map: std::collections::HashMap<SocketAddr, String>,
    nameservers: parking_lot::Mutex<std::collections::HashMap<String, SocketAddr>>,
}

#[cfg(feature = "experimental-rust-tls")]
impl SniOverrides {
    fn new(
        static_map: std::collections::HashMap<SocketAddr, String>,
        nameservers: impl IntoIterator<Item = (String, SocketAddr)>,
    ) -> Self {
        Self {
            static_map,
            nameservers: parking_lot::Mutex::new(nameservers.into_iter().collect()),
        }
    }

    /// Resolve the SNI name for `addr`. Precedence is that of the
    /// snapshot map this replaces: exact `EPICS_CA_TLS_SNI_MAP` row,
    /// then a nameserver whose current resolution is `addr`, then the
    /// wildcard (`port == 0`) map row.
    pub(crate) fn lookup(&self, addr: SocketAddr) -> Option<String> {
        if let Some(h) = self.static_map.get(&addr) {
            return Some(h.clone());
        }
        if let Some((host, _)) = self
            .nameservers
            .lock()
            .iter()
            .find(|(_, sock)| **sock == addr)
        {
            return Some(host.clone());
        }
        self.static_map.get(&SocketAddr::new(addr.ip(), 0)).cloned()
    }

    fn record_resolution(&self, hostname: &str, sock: SocketAddr) {
        self.nameservers.lock().insert(hostname.to_string(), sock);
    }
}

/// Parse `EPICS_CA_TLS_SNI_MAP` — whitespace-separated `IP[:port]=hostname`
/// entries. Returns a vec of `(SocketAddr, hostname)` pairs ready to
/// merge into the per-server SNI override map.
///
/// The CA SEARCH wire protocol carries no hostname information, so a
/// UDP-discovered TLS IOC at e.g. `10.0.0.1:5064` cannot be reached
/// with a hostname-bound cert unless operators provide an explicit
/// IP→hostname mapping. This env (added April 2026) lets multi-IOC
/// TLS deployments work for both EPICS_CA_NAME_SERVERS-listed IOCs
/// and UDP-broadcast-discovered IOCs.
///
/// Entry syntax:
///
///   `10.0.0.1=ioc1.lab.example.com`               (any port)
///   `10.0.0.1:5064=ioc1.lab.example.com`          (specific port)
///   `192.168.1.10:5064=ioc.example.com 10.0.0.2=other.example.com`
///
/// Bad entries (missing `=`, unparseable IP) are silently skipped
/// with a tracing warn — start-time misconfiguration shouldn't kill
/// the client.
#[cfg(feature = "experimental-rust-tls")]
fn parse_tls_sni_map() -> Vec<(SocketAddr, String)> {
    let Some(list) = epics_base_rs::runtime::env::get("EPICS_CA_TLS_SNI_MAP") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in list.split_whitespace() {
        let Some((addr_part, host)) = entry.split_once('=') else {
            tracing::warn!(entry = %entry,
                "EPICS_CA_TLS_SNI_MAP entry missing '=', skipping");
            continue;
        };
        if host.is_empty() {
            tracing::warn!(entry = %entry,
                "EPICS_CA_TLS_SNI_MAP entry has empty hostname, skipping");
            continue;
        }
        let addr = if addr_part.contains(':') {
            match addr_part.parse::<SocketAddr>() {
                Ok(a) => a,
                Err(_) => {
                    tracing::warn!(entry = %entry,
                        "EPICS_CA_TLS_SNI_MAP entry has unparseable IP:port, skipping");
                    continue;
                }
            }
        } else {
            // Bare IP — match any port via the wildcard port=0 form.
            // The transport manager's pick_sni() falls back to port-0
            // lookup when the exact (ip, port) isn't found.
            match addr_part.parse::<std::net::IpAddr>() {
                Ok(ip) => SocketAddr::new(ip, 0),
                Err(_) => {
                    tracing::warn!(entry = %entry,
                        "EPICS_CA_TLS_SNI_MAP entry has unparseable IP, skipping");
                    continue;
                }
            }
        };
        out.push((addr, host.to_string()));
    }
    out
}

/// Parse `EPICS_CA_NAME_SERVERS` — whitespace-separated `host[:port]` entries
/// reachable over TCP. Returns each entry's resolved [`SocketAddr`] alongside
/// the operator-supplied hostname when one was given (None for raw-IP
/// entries). The hostname is later threaded into the TLS handshake as the
/// SNI / cert-verification name for that specific server, so multi-IOC TLS
/// deployments with hostname-bound certs work without a single global
/// `EPICS_CA_TLS_SERVER_NAME` override.
pub(crate) fn parse_nameserver_list() -> Vec<AddrEntry> {
    let Some(list) = epics_base_rs::runtime::env_table::EPICS_CA_NAME_SERVERS.get() else {
        return Vec::new();
    };
    // C `cac.cpp:259` defaults bare-hostname entries to
    // `_serverPort` (from `EPICS_CA_SERVER_PORT`, default 5064),
    // not the hardcoded protocol constant. Pre-fix Rust used
    // `CA_SERVER_PORT` (compile-time 5064) for the bare-hostname
    // branch, diverging from C when sites set
    // `EPICS_CA_SERVER_PORT=5066` to coexist with parallel C
    // deployments. The `host:port` branch already honours the
    // explicit port; only the bare-hostname default changes here.
    let default_server_port: u16 = epics_base_rs::runtime::net::ca_server_port();
    // C `cac.cpp:259` — the SAME tokenizer the client address list is built
    // with, which is why a bad token here is reported in exactly the same two
    // lines, naming this variable.
    // `AddrEntry`, not a bare `SocketAddr`: the hostname must survive to
    // the per-nameserver circuit so its redial path can re-resolve DNS
    // (the same Launchpad-#488 family `EPICS_CA_ADDR_LIST` already
    // handles via `AddrEntry::refresh_dns`). The resolved port carries
    // over — explicit `host:port` entries keep it, bare hostnames got
    // `default_server_port` above.
    let out: Vec<AddrEntry> = crate::iocinf::add_addr_to_channel_access_address_list(
        &list,
        "EPICS_CA_NAME_SERVERS",
        default_server_port,
    )
    .into_iter()
    .map(|tok| AddrEntry::new(tok.sock, tok.hostname, tok.sock.port()))
    .collect();
    // C `cac.cpp:260`: `removeDuplicateAddresses ( &dest, &tmpList, 0 )` — the
    // name-server list is deduped by the SAME helper as `EPICS_CA_ADDR_LIST`,
    // and warns about what it drops for the same reason: a repeated entry
    // otherwise buys a second TCP circuit to that name server.
    crate::iocinf::remove_duplicate_addresses(out, |e| e.sock)
}

// Legacy `parse_addr_list() -> Vec<SocketAddr>`
// removed. The hostname-preserving `parse_addr_list_with_hostnames()`
// (line ~2944) is the only live caller — it carries DNS context
// for the search engine's periodic refresh loop and matches the
// legacy function's AUTO_ADDR_LIST + per-NIC broadcast + limited-
// broadcast fallback semantics.

/// Parse `EPICS_RS_CLIENT_IGNORE` — whitespace-separated IPv4
/// literals or hostnames whose resolved addresses should be ignored
/// by the CA **client**.
///
/// ## Naming note (Rust extension — not C parity)
///
/// Earlier revisions of this function read
/// `EPICS_IOC_IGNORE_SERVERS`, the env var added by epics-base
/// commit `6efe2924` (2017). But that variable has a different
/// meaning in C — `src/ioc/db/dbServer.c::dbRegisterServer` reads
/// it on the **server** side and refuses to register a server
/// (`rsrv`, `qsrv`, …) whose **name** appears in the list. It is
/// not an IP / hostname list and it does not affect client
/// behaviour. Conflating the two broke C IOC startup scripts that
/// set `EPICS_IOC_IGNORE_SERVERS=rsrv` expecting the CA server to
/// stay disabled.
///
/// The Rust client-side quarantine feature is real and useful, just
/// not the same thing — so it now lives under `EPICS_RS_CLIENT_IGNORE`
/// (a Rust-only extension; no C counterpart). The C-parity
/// `EPICS_IOC_IGNORE_SERVERS` semantic (refuse-to-register on the
/// server side) is a separate, not-yet-implemented item — tracked in
/// the upstream-features backlog.
///
/// Returns an empty `Vec` when the env var is unset or contains only
/// unparseable entries. Reads at every call site so tests that mutate
/// the env see the updated list without a process restart.
pub(crate) fn epics_rs_client_ignore() -> Vec<Ipv4Addr> {
    let Some(list) = epics_base_rs::runtime::env::get("EPICS_RS_CLIENT_IGNORE") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in list.split_whitespace() {
        // Strip optional `:port` — the protocol uses one IP for the
        // entire IOC, so we filter on IP only.
        let host = entry.rsplit_once(':').map(|(h, _)| h).unwrap_or(entry);
        if let Ok(ip) = host.parse::<Ipv4Addr>() {
            out.push(ip);
            continue;
        }
        // Fall back to DNS so hostnames work the same as in
        // EPICS_CA_ADDR_LIST.
        use std::net::ToSocketAddrs;
        if let Ok(mut iter) = format!("{host}:0").to_socket_addrs() {
            if let Some(SocketAddr::V4(v4)) = iter.find(|sa: &SocketAddr| sa.is_ipv4()) {
                out.push(*v4.ip());
            } else {
                tracing::debug!(
                    target: "epics_ca_rs::client",
                    entry = %entry,
                    "EPICS_RS_CLIENT_IGNORE: dropped unresolvable entry"
                );
            }
        }
    }
    out
}

#[cfg(all(test, ca_beacon_monitor))]
mod beacon_arrival_routing_tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// Common path: announced address matches an operational
    /// circuit exactly. Result is a single-element vec containing
    /// that address — port-only fallback is NOT consulted.
    #[test]
    fn exact_match_dominates() {
        let states = vec![
            (ChannelState::Connected, Some(addr("10.0.0.1:5064"))),
            (ChannelState::Connected, Some(addr("10.0.0.2:5064"))),
        ];
        let targets = beacon_arrival_targets(states, addr("10.0.0.1:5064"));
        assert_eq!(targets, vec![addr("10.0.0.1:5064")]);
    }

    /// INADDR_ANY beacon (e.g. `0.0.0.0:5064`) — exact-match is
    /// impossible, so we fall back to port-only across operational
    /// circuits. Both 10.0.0.1:5064 and 10.0.0.2:5064 should
    /// receive the notify.
    #[test]
    fn unspecified_addr_falls_back_to_port_match() {
        let states = vec![
            (ChannelState::Connected, Some(addr("10.0.0.1:5064"))),
            (ChannelState::Connected, Some(addr("10.0.0.2:5064"))),
            (ChannelState::Connected, Some(addr("10.0.0.3:6000"))),
        ];
        let mut targets = beacon_arrival_targets(states, addr("0.0.0.0:5064"));
        targets.sort();
        assert_eq!(
            targets,
            vec![addr("10.0.0.1:5064"), addr("10.0.0.2:5064")],
            ":6000 must NOT be a target for a :5064 beacon"
        );
    }

    /// Multi-homed IOC: beacon announces NIC A but the circuit was
    /// established via NIC B. Exact match misses, port-only
    /// fallback matches B's address.
    #[test]
    fn multi_homed_falls_back_to_port_match() {
        let states = vec![
            // Circuit was established via NIC B (10.0.0.2).
            (ChannelState::Connected, Some(addr("10.0.0.2:5064"))),
        ];
        // Beacon arrives via NIC A (10.0.0.1) — different IP, same port.
        let targets = beacon_arrival_targets(states, addr("10.0.0.1:5064"));
        assert_eq!(
            targets,
            vec![addr("10.0.0.2:5064")],
            "multi-homed IOC must be reachable via port-only fallback"
        );
    }

    /// Non-operational channels (Searching, Disconnected, etc.)
    /// never match — the watchdog only exists for operational
    /// circuits.
    #[test]
    fn non_operational_channels_do_not_match() {
        let states = vec![
            (ChannelState::Searching, Some(addr("10.0.0.1:5064"))),
            (ChannelState::Disconnected, Some(addr("10.0.0.2:5064"))),
        ];
        let targets = beacon_arrival_targets(states, addr("10.0.0.1:5064"));
        assert!(
            targets.is_empty(),
            "non-operational channels must not generate watchdog notifies"
        );
    }

    /// No matching circuit at all — empty vec, no spurious sends.
    #[test]
    fn no_match_returns_empty() {
        let states = vec![(ChannelState::Connected, Some(addr("10.0.0.1:5064")))];
        let targets = beacon_arrival_targets(states, addr("10.0.0.99:5065"));
        assert!(targets.is_empty());
    }

    /// Multiple circuits to the same exact address (rare but
    /// possible during state transitions) — `vec![beacon_addr]`
    /// is a single notify regardless. Transport's per-circuit
    /// keying handles dedup downstream.
    #[test]
    fn exact_match_emits_single_notify() {
        let states = vec![
            (ChannelState::Connected, Some(addr("10.0.0.1:5064"))),
            (ChannelState::Connected, Some(addr("10.0.0.1:5064"))),
        ];
        let targets = beacon_arrival_targets(states, addr("10.0.0.1:5064"));
        assert_eq!(targets, vec![addr("10.0.0.1:5064")]);
    }
}

#[cfg(test)]
mod ioc_connection_count_tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// The defect this fix closes: two channels to the SAME IOC at
    /// DIFFERENT CA priorities are two virtual circuits in libca
    /// (`caServerID` keys on `(addr, priority)`), so the count must be 2.
    /// Address-only dedup reported 1.
    #[test]
    fn two_priorities_to_one_ioc_count_as_two_circuits() {
        let states = vec![
            (ChannelState::Connected, Some(addr("10.0.0.1:5064")), 0u8),
            (ChannelState::Connected, Some(addr("10.0.0.1:5064")), 1u8),
        ];
        assert_eq!(operational_circuit_count(states), 2);
    }

    /// Same `(addr, priority)` shared by many channels is one circuit —
    /// channels multiplex onto a circuit, the count is circuit-level.
    #[test]
    fn many_channels_one_circuit_count_as_one() {
        let states = vec![
            (ChannelState::Connected, Some(addr("10.0.0.1:5064")), 0u8),
            (ChannelState::Connected, Some(addr("10.0.0.1:5064")), 0u8),
            (ChannelState::Connected, Some(addr("10.0.0.1:5064")), 0u8),
        ];
        assert_eq!(operational_circuit_count(states), 1);
    }

    /// Distinct server addresses at the same priority are distinct
    /// circuits.
    #[test]
    fn distinct_addresses_count_separately() {
        let states = vec![
            (ChannelState::Connected, Some(addr("10.0.0.1:5064")), 0u8),
            (ChannelState::Connected, Some(addr("10.0.0.2:5064")), 0u8),
        ];
        assert_eq!(operational_circuit_count(states), 2);
    }

    /// Non-operational channels hold no live circuit and are excluded.
    #[test]
    fn non_operational_channels_excluded() {
        let states = vec![
            (ChannelState::Searching, Some(addr("10.0.0.1:5064")), 0u8),
            (ChannelState::Disconnected, Some(addr("10.0.0.2:5064")), 1u8),
        ];
        assert_eq!(operational_circuit_count(states), 0);
    }

    /// An operational channel that has not yet bound a server address
    /// contributes no circuit.
    #[test]
    fn missing_server_addr_excluded() {
        let states = vec![(ChannelState::Connected, None, 0u8)];
        assert_eq!(operational_circuit_count(states), 0);
    }
}

#[cfg(test)]
mod tls_sni_config_tests {
    #[cfg(feature = "experimental-rust-tls")]
    use super::*;

    /// `tls_server_name` defaults to `None` and accepts `Some(...)` so
    /// callers can pin the SNI / cert-hostname-verification name when
    /// the server cert is hostname-bound.
    #[cfg(feature = "experimental-rust-tls")]
    #[test]
    fn tls_server_name_round_trip() {
        let mut cfg = CaClientConfig::default();
        assert!(cfg.tls_server_name.is_none(), "default must be None");
        cfg.tls_server_name = Some("ioc.example.com".into());
        assert_eq!(cfg.tls_server_name.as_deref(), Some("ioc.example.com"));
    }

    /// epics-base#488 residual: the SNI row must follow the hostname,
    /// not the address it resolved to at startup. `refresh_dns` — the
    /// one place a tracked hostname is resolved — rewrites the row, so
    /// a TLS circuit to the moved nameserver's new address still gets
    /// its hostname and the stale address stops matching.
    #[cfg(feature = "experimental-rust-tls")]
    #[test]
    fn a_moved_nameserver_keeps_its_sni_override() {
        let stale: SocketAddr = "192.0.2.1:5064".parse().unwrap();
        let sni = std::sync::Arc::new(SniOverrides::new(
            std::collections::HashMap::new(),
            [("localhost".to_string(), stale)],
        ));
        assert_eq!(sni.lookup(stale).as_deref(), Some("localhost"));

        // The nameserver task's redial path: refresh through the entry.
        let mut entry = AddrEntry::new(stale, Some("localhost".into()), 5064).with_sni(sni.clone());
        let fresh = entry.refresh_dns().expect("localhost resolves");
        assert_ne!(fresh, stale, "precondition: the resolution moved");
        assert_eq!(sni.lookup(fresh).as_deref(), Some("localhost"));
        assert_eq!(
            sni.lookup(stale),
            None,
            "hostname-keyed row must not leave the startup address behind"
        );
    }

    /// Precedence of the snapshot map this registry replaces: exact
    /// static row, then live nameserver binding, then wildcard row.
    #[cfg(feature = "experimental-rust-tls")]
    #[test]
    fn static_sni_rows_win_and_wildcard_comes_last() {
        let pinned: SocketAddr = "10.0.0.1:5064".parse().unwrap();
        let sni = SniOverrides::new(
            [
                (pinned, "pin.example.com".to_string()),
                (
                    "10.0.0.1:0".parse().unwrap(),
                    "wild.example.com".to_string(),
                ),
            ]
            .into(),
            [("ns.example.com".to_string(), pinned)],
        );
        // Exact static row beats the nameserver binding for the same addr.
        assert_eq!(sni.lookup(pinned).as_deref(), Some("pin.example.com"));
        // No exact row, no binding at this port → wildcard row.
        assert_eq!(
            sni.lookup("10.0.0.1:5065".parse().unwrap()).as_deref(),
            Some("wild.example.com")
        );
        assert_eq!(sni.lookup("10.0.0.2:5064".parse().unwrap()), None);
    }
}

#[cfg(test)]
mod waiter_drain_tests {
    use super::*;
    use std::collections::HashSet;
    use tokio::sync::oneshot;

    /// `drain_waiters_for_cids` must wake every blocked read/write
    /// future whose cid is in the provided set with
    /// `Err(CaError::Disconnected)`. This is the wake path that
    /// SERVER_DISCONN (cmd 27) and bulk TcpClosed both rely on so a
    /// blocked `caget`/`caput` future surfaces the disconnect
    /// instead of stalling on its outer timeout.
    #[tokio::test(flavor = "current_thread")]
    async fn drain_wakes_matching_cid_only() {
        let in_flight = types::InFlightOps::new();

        // ioid 1001 / 1002 belong to cid=42 (will be disconnected).
        // ioid 2001 / 2002 belong to cid=99 (must survive).
        let (rtx_42, rrx_42) = oneshot::channel();
        let (rtx_99, rrx_99) = oneshot::channel();
        let (wtx_42, wrx_42) = oneshot::channel();
        let (wtx_99, wrx_99) = oneshot::channel();
        in_flight.reads.insert(
            1001,
            types::ReadWaiter::OneShot {
                cid: 42,
                mode: types::ReadReplyMode::Raw,
                reply_tx: rtx_42,
            },
        );
        in_flight.reads.insert(
            2001,
            types::ReadWaiter::OneShot {
                cid: 99,
                mode: types::ReadReplyMode::Raw,
                reply_tx: rtx_99,
            },
        );
        in_flight.writes.insert(1002, (42, wtx_42));
        in_flight.writes.insert(2002, (99, wtx_99));

        let mut affected = HashSet::new();
        affected.insert(42u32);
        drain_waiters_for_cids(&affected, &in_flight);

        // cid=42 waiters: ioids removed from maps + Senders fired with Disconnected.
        assert!(!in_flight.reads.contains_key(&1001));
        assert!(!in_flight.writes.contains_key(&1002));
        assert!(matches!(rrx_42.await, Ok(Err(CaError::Disconnected))));
        assert!(matches!(wrx_42.await, Ok(Err(CaError::Disconnected))));

        // cid=99 waiters: untouched.
        assert!(in_flight.reads.contains_key(&2001));
        assert!(in_flight.writes.contains_key(&2002));
        // Drop the registry to release Senders so the rx awaits don't hang.
        drop(in_flight);
        assert!(rrx_99.await.is_err()); // sender dropped
        assert!(wrx_99.await.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drain_with_empty_cid_set_is_noop() {
        let in_flight = types::InFlightOps::new();
        let (rtx, rrx) = oneshot::channel();
        let (wtx, wrx) = oneshot::channel();
        in_flight.reads.insert(
            10,
            types::ReadWaiter::OneShot {
                cid: 1,
                mode: types::ReadReplyMode::Raw,
                reply_tx: rtx,
            },
        );
        in_flight.writes.insert(20, (2, wtx));

        let affected: HashSet<u32> = HashSet::new();
        drain_waiters_for_cids(&affected, &in_flight);

        assert!(in_flight.reads.contains_key(&10));
        assert!(in_flight.writes.contains_key(&20));
        // Drop the registry so the rx awaits don't hang in CI.
        drop(in_flight);
        assert!(rrx.await.is_err());
        assert!(wrx.await.is_err());
    }

    /// Phase D regression: the response-vs-disconnect race must NOT
    /// produce a spurious `Disconnected` error after the response was
    /// already delivered. With Option C, both the transport read loop
    /// (success delivery) and the drain path (disconnect) call
    /// `in_flight.reads.remove(ioid)`. Whichever wins the race fulfils
    /// the oneshot; the other no-ops.
    #[tokio::test(flavor = "current_thread")]
    async fn response_arrives_before_disconnect_drain() {
        let in_flight = types::InFlightOps::new();
        let (rtx, rrx) = oneshot::channel();
        in_flight.reads.insert(
            100,
            types::ReadWaiter::OneShot {
                cid: 7,
                mode: types::ReadReplyMode::Raw,
                reply_tx: rtx,
            },
        );

        // Transport delivers the response first.
        if let Some((_, waiter)) = in_flight.reads.remove(&100) {
            waiter.send(Ok(types::ReadReply::Raw {
                data_type: 6,
                count: 1,
                data: vec![1, 0, 0, 0],
            }));
        }

        // Disconnect drain runs immediately after — should find nothing
        // and leave the receiver's Ok intact.
        let mut affected = HashSet::new();
        affected.insert(7u32);
        drain_waiters_for_cids(&affected, &in_flight);

        let result = rrx.await.expect("oneshot still alive");
        assert!(matches!(
            result,
            Ok(types::ReadReply::Raw {
                data_type: 6,
                count: 1,
                ..
            })
        ));
    }
}

#[cfg(test)]
mod event_watcher_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// Wait for a condition driven by *another* executor.
    ///
    /// These tests run on a `current_thread` tokio runtime but the watcher
    /// task runs on whatever backend the `runtime::task` seam selects — a
    /// tokio worker by default, a background-executor band worker under
    /// `rtems-exec-model`. `yield_now()` only reschedules on *this*
    /// runtime, so it establishes no happens-before edge with the other
    /// executor: spinning on it is a race, not a wait. Poll with a real
    /// time budget instead, which is a correct wait on either backend.
    async fn wait_for(label: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..2_000 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("{label} (waited 2s)");
    }

    /// Dropping an `EventWatcher` must abort the spawned watcher task
    /// (a bare `JoinHandle` would only detach, leaking the task). The
    /// task here loops forever; after the guard drops, `is_finished()`
    /// must become true.
    #[tokio::test(flavor = "current_thread")]
    async fn drop_aborts_watcher_task() {
        let ran = Arc::new(AtomicBool::new(false));
        let ran_in_task = ran.clone();
        let handle = epics_base_rs::runtime::task::spawn(async move {
            ran_in_task.store(true, Ordering::SeqCst);
            loop {
                tokio::task::yield_now().await;
            }
        });
        let abort_handle = handle.abort_handle();
        let watcher = EventWatcher { handle };
        wait_for("watcher task should have started", || {
            ran.load(Ordering::SeqCst)
        })
        .await;
        assert!(
            !abort_handle.is_finished(),
            "task still running before drop"
        );
        drop(watcher);
        wait_for("EventWatcher::drop must abort the watcher task", || {
            abort_handle.is_finished()
        })
        .await;
    }

    /// `EventWatcher::abort()` is an explicit, named teardown that
    /// behaves identically to dropping the guard.
    #[tokio::test(flavor = "current_thread")]
    async fn explicit_abort_stops_watcher_task() {
        let handle = epics_base_rs::runtime::task::spawn(async move {
            loop {
                tokio::task::yield_now().await;
            }
        });
        let abort_handle = handle.abort_handle();
        let watcher = EventWatcher { handle };
        watcher.abort();
        wait_for("EventWatcher::abort must stop the watcher task", || {
            abort_handle.is_finished()
        })
        .await;
    }
}

#[cfg(test)]
mod typed_string_put_tests {
    use super::*;

    /// The typed string-put path (`CaChannel::put_string` /
    /// `put_string_nowait`) must put the value on the wire as
    /// `DBR_STRING` (DBR type code 0) regardless of the channel's
    /// native type, so the server resolves it (e.g. ENUM menu
    /// strings). This drives the shared `build_write_frame` builder
    /// the way `put_string_nowait` does and asserts the header field.
    #[test]
    fn put_string_frame_uses_dbr_string_type() {
        let payload = EpicsValue::String("Running".into()).to_bytes();
        let frame = CaChannel::build_write_frame(
            CA_PROTO_WRITE,
            /* sid */ 77,
            /* data_type — forced DBR_STRING */ 0,
            /* count */ 1,
            /* ioid */ None,
            payload,
            CA_MINOR_VERSION,
        )
        .expect("modern peer accepts the frame");
        let (hdr, _consumed) =
            CaHeader::from_bytes_extended(&frame).expect("frame header must parse");
        assert_eq!(hdr.cmmd, CA_PROTO_WRITE, "command must be CA_PROTO_WRITE");
        assert_eq!(
            hdr.data_type, 0,
            "typed string-put must wire DBR_STRING (0), not the native type"
        );
        assert_eq!(hdr.cid, 77, "sid echoed in cid field");
    }

    /// Contrast: a native-typed put of a non-string channel carries
    /// the native DBR type, NOT 0. Proves the string-put choice of 0
    /// is a deliberate override, not the default.
    #[test]
    fn native_typed_put_frame_keeps_native_type() {
        let payload = EpicsValue::Double(1.5).to_bytes();
        let native_double = DbFieldType::Double as u16; // 6
        let frame = CaChannel::build_write_frame(
            CA_PROTO_WRITE_NOTIFY,
            /* sid */ 5,
            native_double,
            /* count */ 1,
            /* ioid */ Some(42),
            payload,
            CA_MINOR_VERSION,
        )
        .expect("modern peer accepts the frame");
        let (hdr, _consumed) =
            CaHeader::from_bytes_extended(&frame).expect("frame header must parse");
        assert_eq!(hdr.data_type, native_double);
        assert_ne!(
            hdr.data_type, 0,
            "native Double put must not collapse to DBR_STRING"
        );
        assert_eq!(hdr.available, 42, "ioid echoed in available field");
    }

    /// The ENUM-waveform-by-name path (`put_string_array`) must wire a
    /// `DBR_STRING` array: data_type 0, count = element count, and a
    /// 40-byte-per-element payload, so the server resolves each menu
    /// string. Drives the same `build_write_frame` builder.
    #[test]
    fn put_string_array_frame_uses_dbr_string_type_and_count() {
        let values = [
            "Running".to_string(),
            "Stopped".to_string(),
            "Paused".to_string(),
        ];
        let payload =
            EpicsValue::StringArray(values.iter().map(|s| s.clone().into()).collect()).to_bytes();
        assert_eq!(
            payload.len(),
            values.len() * 40,
            "DBR_STRING array is 40 bytes per element"
        );
        let frame = CaChannel::build_write_frame(
            CA_PROTO_WRITE_NOTIFY,
            /* sid */ 9,
            /* data_type — forced DBR_STRING */ 0,
            /* count */ values.len() as u32,
            /* ioid */ Some(7),
            payload,
            CA_MINOR_VERSION,
        )
        .expect("modern peer accepts the frame");
        let (hdr, _consumed) =
            CaHeader::from_bytes_extended(&frame).expect("frame header must parse");
        assert_eq!(
            hdr.data_type, 0,
            "string-array put must wire DBR_STRING (0)"
        );
        assert_eq!(hdr.count, 3, "count must be the element count");
    }
}

#[cfg(test)]
mod user_data_tests {
    use super::*;

    #[test]
    fn set_get_downcast_clone_share_and_clear() {
        let (coord_tx, _rx) = mpsc::unbounded_channel();
        let ch = CaChannel::for_test(coord_tx);

        // Empty slot to start.
        assert!(ch.user_data::<u32>().is_none());

        // Store, then read back with the matching type.
        ch.set_user_data(Arc::new(42u32));
        assert_eq!(ch.user_data::<u32>().as_deref(), Some(&42u32));

        // A wrong-type downcast misses rather than panicking.
        assert!(ch.user_data::<String>().is_none());

        // Clones share the one slot (libca "one PUSER per chid").
        let ch2 = ch.clone();
        assert_eq!(ch2.user_data::<u32>().as_deref(), Some(&42u32));

        // Clear returns the previous contents and empties the shared slot.
        assert!(ch.clear_user_data().is_some());
        assert!(ch.user_data::<u32>().is_none());
        assert!(ch2.user_data::<u32>().is_none());
    }
}

#[cfg(test)]
mod monitor_pause_tests {
    use super::*;
    use epics_base_rs::server::snapshot::Snapshot;
    use epics_base_rs::types::EpicsValue;
    use std::time::{Duration, SystemTime};

    fn snap(v: i32) -> Snapshot {
        Snapshot::new(EpicsValue::Long(v), 0, 0, SystemTime::now())
    }

    /// A′ end-to-end recv-side gate:
    /// 1. pre-pause channel backlog is delivered even while paused (3a);
    /// 2. a value arriving during pause is held — `recv` blocks (2a);
    /// 3. `resume` releases the held latest, in order after the backlog.
    #[epics_macros_rs::epics_test]
    async fn pause_holds_value_resume_releases_after_backlog() {
        let (callback_tx, callback_rx) = mpsc::channel::<CaResult<Snapshot>>(4);
        let (coord_tx, _coord_rx) = mpsc::unbounded_channel();
        let coalesce_slot = subscription::CoalesceSlot::new();
        let mut handle = MonitorHandle {
            subid: 1,
            callback_rx,
            coalesce_slot: coalesce_slot.clone(),
            channel: CaChannel::for_test(coord_tx.clone()),
            coord_tx,
        };

        // Pre-pause backlog: value 1 already in the channel.
        callback_tx.try_send(Ok(snap(1))).expect("enqueue backlog");

        handle.pause();

        // A value arrives DURING pause → producer holds it in the slot
        // (route_value Slotted path).
        assert!(matches!(
            coalesce_slot.route_value(snap(2)),
            subscription::ValueRoute::Slotted
        ));

        // (3a) The pre-pause backlog is still delivered while paused.
        let v1 = epics_base_rs::runtime::task::timeout(Duration::from_millis(200), handle.recv())
            .await
            .expect("backlog should deliver promptly")
            .expect("Some")
            .expect("Ok");
        assert_eq!(v1.value, EpicsValue::Long(1), "pre-pause backlog (1)");

        // (2a) The held value (2) must NOT be delivered while paused —
        // recv blocks.
        let blocked =
            epics_base_rs::runtime::task::timeout(Duration::from_millis(150), handle.recv()).await;
        assert!(
            blocked.is_err(),
            "held value must stay withheld while paused (recv-side gate)"
        );

        // resume → the held latest is released.
        handle.resume();
        let v2 = epics_base_rs::runtime::task::timeout(Duration::from_millis(200), handle.recv())
            .await
            .expect("held value should deliver after resume")
            .expect("Some")
            .expect("Ok");
        assert_eq!(v2.value, EpicsValue::Long(2), "held latest after resume");
    }

    /// a configured queue smaller than MIN_MONITOR_QUEUE_SIZE is
    /// clamped up so a lone subscription's full channel can still reach
    /// the threshold and trip EVENTS_OFF (slot overflow is out of flow
    /// control, so an unclamped small queue would coalesce forever).
    #[test]
    fn monitor_queue_clamped_to_min_size() {
        // Below threshold → clamped up.
        assert_eq!(resolve_monitor_queue_size(Some(5)), MIN_MONITOR_QUEUE_SIZE);
        assert_eq!(resolve_monitor_queue_size(Some(9)), MIN_MONITOR_QUEUE_SIZE);
        // At/above threshold → unchanged.
        assert_eq!(
            resolve_monitor_queue_size(Some(MIN_MONITOR_QUEUE_SIZE)),
            MIN_MONITOR_QUEUE_SIZE
        );
        assert_eq!(resolve_monitor_queue_size(Some(1000)), 1000);
        // Unset → default 256 (well above the threshold).
        assert_eq!(resolve_monitor_queue_size(None), 256);
    }

    /// A′ error policy end-to-end: an error arriving during pause is
    /// delivered immediately (bypasses the gate), without waiting for
    /// resume.
    #[epics_macros_rs::epics_test]
    async fn pause_does_not_hide_errors() {
        let (callback_tx, callback_rx) = mpsc::channel::<CaResult<Snapshot>>(4);
        let (coord_tx, _coord_rx) = mpsc::unbounded_channel();
        let coalesce_slot = subscription::CoalesceSlot::new();
        let mut handle = MonitorHandle {
            subid: 1,
            callback_rx,
            coalesce_slot,
            channel: CaChannel::for_test(coord_tx.clone()),
            coord_tx,
        };

        handle.pause();

        // An error reaches the channel (errors bypass pause; they do
        // not go through the value-only `route_value` gate).
        callback_tx
            .try_send(Err(CaError::ServerError(192))) // ECA_DISCONN
            .expect("enqueue error");

        let got = epics_base_rs::runtime::task::timeout(Duration::from_millis(200), handle.recv())
            .await
            .expect("error must deliver while paused")
            .expect("Some");
        assert!(
            matches!(got, Err(CaError::ServerError(192))),
            "ECA_DISCONN must bypass pause"
        );
    }
}

#[cfg(test)]
mod disconnect_transition_tests {
    //! R8-17 / R8-20: the three paths that take a channel out of the
    //! connected set — TCP close, `CA_PROTO_SERVER_DISCONN`, echo timeout —
    //! all run the same transition in C, because all three converge on
    //! `nciu::unresponsiveCircuitNotify` (`nciu.cpp:161-182`):
    //! `disconnectAllIO` → no-rights → `disconnectNotify` →
    //! `accessRightsNotify(noRights)`. Rust hand-rolled it three times and
    //! only the echo-timeout path reset the access rights. These tests pin
    //! the invariant on the owner, once per path.
    use super::*;
    use epics_base_rs::runtime::sync::broadcast;
    use tokio::sync::oneshot;

    fn addr() -> SocketAddr {
        "127.0.0.1:5064".parse().unwrap()
    }

    /// A Connected channel holding read+write rights — the pre-condition
    /// that makes the missing no-rights transition observable.
    fn connected_channel(
        cid: u32,
        priority: u8,
    ) -> (ChannelInner, broadcast::Receiver<ConnectionEvent>) {
        let (conn_tx, conn_rx) = broadcast::channel(16);
        let ch = ChannelInner {
            cid,
            pv_name: format!("TEST:PV{cid}"),
            priority,
            state: ChannelState::Connected,
            sid: 7,
            native_type: None,
            element_count: 1,
            server_addr: Some(addr()),
            access_rights: AccessRights {
                read: true,
                write: true,
            },
            connect_waiters: Vec::new(),
            conn_tx,
            reconnect_count: 0,
            last_connected_at: None,
        };
        (ch, conn_rx)
    }

    fn drain_events(rx: &mut broadcast::Receiver<ConnectionEvent>) -> Vec<ConnectionEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// R18-19: a circuit that dies with channels on it raises ECA_DISCONN on
    /// the global exception hook — C `cac::destroyIIU` (`cac.cpp:1236-1240`),
    /// gated on `iiu.channelCount()`. Pre-fix the circuit-gone path dispatched
    /// no exception at all, so a handler installed the way C library users do
    /// it (to log IOC loss) saw nothing. An empty circuit raises nothing, same
    /// gate as C.
    #[tokio::test(flavor = "current_thread")]
    async fn r18_19_circuit_death_raises_eca_disconn() {
        for (channels_on_circuit, want) in [(1usize, 1usize), (0, 0)] {
            let mut channels = HashMap::new();
            let mut server_channels: HashMap<types::CircuitKey, HashSet<u32>> = HashMap::new();
            if channels_on_circuit > 0 {
                let (ch, _rx) = connected_channel(42, 0);
                channels.insert(42u32, ch);
                server_channels.insert((addr(), 0), HashSet::from([42u32]));
            }
            let mut subscriptions = SubscriptionRegistry::new();
            let (search_tx, _search_rx) = mpsc::unbounded_channel();
            let diag = CaDiagnostics::default();
            let in_flight = types::InFlightOps::new();
            let snapshots: ChannelSnapshots = Arc::new(dashmap::DashMap::new());

            let raised: Arc<parking_lot::Mutex<Vec<(u32, String, types::ExceptionSite)>>> =
                Arc::new(parking_lot::Mutex::new(Vec::new()));
            let sink = raised.clone();
            let slot: types::CaExceptionSlot = Arc::new(parking_lot::RwLock::new(Some(Arc::new(
                move |exc: &types::CaException| {
                    sink.lock()
                        .push((exc.status.unwrap_or(0), exc.message.clone(), exc.source));
                },
            ))));

            handle_disconnect(
                &mut channels,
                &mut subscriptions,
                &mut server_channels,
                &search_tx,
                (addr(), 0),
                &diag,
                &in_flight,
                &snapshots,
                &slot,
            );

            let got = raised.lock().clone();
            assert_eq!(
                got.len(),
                want,
                "channels on circuit = {channels_on_circuit}: C gates the \
                 genLocalExcep on `iiu.channelCount()`"
            );
            if let Some((status, ctx, source)) = got.first() {
                assert_eq!(*status, crate::protocol::ECA_DISCONN);
                // Context is the resolved peer name, not a port-invented
                // sentence; `Source File:` is present because `genLocalExcep`
                // always passes `__FILE__`/`__LINE__`.
                assert!(
                    !ctx.is_empty() && !ctx.contains("circuit"),
                    "context: {ctx}"
                );
                assert_eq!(*source, types::LIBCA_CIRCUIT_DISCONNECT_SITE);
            }
        }
    }

    /// TCP circuit close (C `tcpiiu::disconnectAllChannels`,
    /// `tcpiiu.cpp:1848-1855`): the channel must drop to no-rights and the
    /// subscriber must see `AccessRightsChanged { false, false }` after
    /// `Disconnected`. Pre-fix the cached rights stayed read+write until
    /// reconnect and no rights event was emitted at all.
    #[tokio::test(flavor = "current_thread")]
    async fn r8_17_tcp_close_drops_the_channel_to_no_rights() {
        let mut channels = HashMap::new();
        let (ch, mut conn_rx) = connected_channel(42, 0);
        channels.insert(42u32, ch);
        let mut subscriptions = SubscriptionRegistry::new();
        let mut server_channels: HashMap<types::CircuitKey, HashSet<u32>> = HashMap::new();
        let (search_tx, _search_rx) = mpsc::unbounded_channel();
        let diag = CaDiagnostics::default();
        let in_flight = types::InFlightOps::new();
        let snapshots: ChannelSnapshots = Arc::new(dashmap::DashMap::new());

        handle_disconnect(
            &mut channels,
            &mut subscriptions,
            &mut server_channels,
            &search_tx,
            (addr(), 0),
            &diag,
            &in_flight,
            &snapshots,
            &types::CaExceptionSlot::default(),
        );

        let ch = channels.get(&42).expect("channel stays registered");
        assert_eq!(ch.state, ChannelState::Disconnected);
        assert!(
            !ch.access_rights.read && !ch.access_rights.write,
            "C `setServerAddressUnknown` clears both permits on circuit close"
        );
        assert_eq!(
            drain_events(&mut conn_rx),
            vec![
                ConnectionEvent::Disconnected,
                ConnectionEvent::AccessRightsChanged {
                    read: false,
                    write: false
                },
            ],
            "C fires disconnectNotify then accessRightsNotify(noRights)"
        );
    }

    /// `CA_PROTO_SERVER_DISCONN` (C `cac::disconnectChannel`,
    /// `cac.cpp:1185-1195`) runs the identical transition, scoped to one
    /// cid — and must not touch a sibling channel on the same circuit.
    #[tokio::test(flavor = "current_thread")]
    async fn r8_17_server_disconnect_drops_only_that_channel_to_no_rights() {
        let mut channels = HashMap::new();
        let (ch, mut conn_rx) = connected_channel(42, 0);
        let (other, mut other_rx) = connected_channel(43, 0);
        channels.insert(42u32, ch);
        channels.insert(43u32, other);
        let mut subscriptions = SubscriptionRegistry::new();
        let in_flight = types::InFlightOps::new();
        let snapshots: ChannelSnapshots = Arc::new(dashmap::DashMap::new());
        let (search_tx, mut search_rx) = mpsc::unbounded_channel();
        let exception_slot: types::CaExceptionSlot = Default::default();

        handle_server_disconnect(
            &mut channels,
            &mut subscriptions,
            &search_tx,
            42,
            addr(),
            &in_flight,
            &snapshots,
            &exception_slot,
        );
        assert!(
            matches!(
                search_rx.try_recv(),
                Ok(SearchRequest::Schedule { cid: 42, .. })
            ),
            "SERVER_DISCONN re-searches the retired channel"
        );

        let ch = channels.get(&42).unwrap();
        assert_eq!(ch.state, ChannelState::Disconnected);
        assert!(
            !ch.access_rights.read && !ch.access_rights.write,
            "SERVER_DISCONN must reset access rights (C disconnectChannel)"
        );
        assert_eq!(
            drain_events(&mut conn_rx),
            vec![
                ConnectionEvent::Disconnected,
                ConnectionEvent::AccessRightsChanged {
                    read: false,
                    write: false
                },
            ]
        );

        let other = channels.get(&43).unwrap();
        assert_eq!(other.state, ChannelState::Connected);
        assert!(
            other.access_rights.read && other.access_rights.write,
            "a sibling cid on the same circuit is untouched"
        );
        assert!(drain_events(&mut other_rx).is_empty());
    }

    /// R18-18: the echo-timeout path keeps its own *internal* terminal state
    /// (`Unresponsive` selects the re-subscribe-on-the-live-socket recovery),
    /// but the application sees the ordinary `Disconnected`: C's
    /// `nciu::unresponsiveCircuitNotify` (`nciu.cpp:170`) calls the same
    /// `disconnectNotify()` — `CA_OP_CONN_DOWN` — as a socket close.
    #[tokio::test(flavor = "current_thread")]
    async fn r18_18_unresponsive_is_a_disconnect_for_the_application() {
        let mut channels = HashMap::new();
        let (ch, mut conn_rx) = connected_channel(42, 0);
        channels.insert(42u32, ch);
        let mut subscriptions = SubscriptionRegistry::new();
        let in_flight = types::InFlightOps::new();
        let snapshots: ChannelSnapshots = Arc::new(dashmap::DashMap::new());

        disconnect_channels(
            &mut channels,
            &mut subscriptions,
            &in_flight,
            &snapshots,
            DisconnectKind::Unresponsive,
            |ch| ch.state == ChannelState::Connected,
            |_| {},
        );

        let ch = channels.get(&42).unwrap();
        assert_eq!(ch.state, ChannelState::Unresponsive);
        assert!(!ch.access_rights.read && !ch.access_rights.write);
        assert_eq!(
            drain_events(&mut conn_rx),
            vec![
                ConnectionEvent::Disconnected,
                ConnectionEvent::AccessRightsChanged {
                    read: false,
                    write: false
                },
            ]
        );
    }

    /// R8-20: the IO failures must be delivered BEFORE the connection
    /// event. C `nciu::unresponsiveCircuitNotify` (`nciu.cpp:168-170`) calls
    /// `disconnectAllIO` and only then `disconnectNotify`, so an application
    /// reacting to "channel down" never finds an IO of that channel still
    /// pending.
    ///
    /// Observing the order: both consumers park on their channels before the
    /// transition runs, and the transition runs on the `block_on` future of a
    /// `current_thread` runtime — so the two wakes land on the scheduler's
    /// injection queue in send order and the tasks run in that order. The
    /// recorded sequence is therefore the send sequence.
    #[tokio::test(flavor = "current_thread")]
    async fn r8_20_io_failures_are_delivered_before_the_disconnect_event() {
        let mut channels = HashMap::new();
        let (ch, mut conn_rx) = connected_channel(42, 0);
        channels.insert(42u32, ch);
        let mut subscriptions = SubscriptionRegistry::new();
        let in_flight = types::InFlightOps::new();
        let snapshots: ChannelSnapshots = Arc::new(dashmap::DashMap::new());

        let (rtx, rrx) = oneshot::channel();
        in_flight.reads.insert(
            1,
            types::ReadWaiter::OneShot {
                cid: 42,
                mode: types::ReadReplyMode::Raw,
                reply_tx: rtx,
            },
        );

        let order: Arc<std::sync::Mutex<Vec<&'static str>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let io_order = order.clone();
        let io_task = tokio::spawn(async move {
            let got = rrx.await.expect("read waiter must be woken");
            assert!(matches!(got, Err(CaError::Disconnected)));
            io_order.lock().unwrap().push("io");
        });
        let conn_order = order.clone();
        let conn_task = tokio::spawn(async move {
            let ev = conn_rx.recv().await.expect("connection event");
            assert_eq!(ev, ConnectionEvent::Disconnected);
            conn_order.lock().unwrap().push("conn");
        });

        // Let both consumers park on their awaits.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            order.lock().unwrap().is_empty(),
            "nothing may be delivered before the transition runs"
        );

        disconnect_channels(
            &mut channels,
            &mut subscriptions,
            &in_flight,
            &snapshots,
            DisconnectKind::CircuitGone,
            |ch| ch.cid == 42,
            |_| {},
        );

        io_task.await.unwrap();
        conn_task.await.unwrap();
        assert_eq!(
            *order.lock().unwrap(),
            vec!["io", "conn"],
            "C fails the channel's pending IOs (disconnectAllIO) before it \
             signals the disconnect (disconnectNotify)"
        );
    }

    /// Pending IOs still fail with `Disconnected` on every path through the
    /// owner — the fan-out must not be lost in the refactor.
    #[tokio::test(flavor = "current_thread")]
    async fn owner_fails_pending_ios_on_every_path() {
        for kind in [DisconnectKind::CircuitGone, DisconnectKind::Unresponsive] {
            let mut channels = HashMap::new();
            let (ch, _rx) = connected_channel(42, 0);
            channels.insert(42u32, ch);
            let mut subscriptions = SubscriptionRegistry::new();
            let in_flight = types::InFlightOps::new();
            let snapshots: ChannelSnapshots = Arc::new(dashmap::DashMap::new());

            let (rtx, rrx) = oneshot::channel();
            let (wtx, wrx) = oneshot::channel();
            in_flight.reads.insert(
                1,
                types::ReadWaiter::OneShot {
                    cid: 42,
                    mode: types::ReadReplyMode::Raw,
                    reply_tx: rtx,
                },
            );
            in_flight.writes.insert(2, (42, wtx));

            disconnect_channels(
                &mut channels,
                &mut subscriptions,
                &in_flight,
                &snapshots,
                kind,
                |ch| ch.cid == 42,
                |_| {},
            );

            assert!(
                matches!(rrx.await, Ok(Err(CaError::Disconnected))),
                "{kind:?}: pending read must fail with Disconnected"
            );
            assert!(
                matches!(wrx.await, Ok(Err(CaError::Disconnected))),
                "{kind:?}: pending write must fail with Disconnected"
            );
        }
    }
}
