mod beacon_monitor;
mod circuit_breaker;
mod search;
mod state;
mod subscription;
mod sync_group;
mod transport;
mod types;

pub use sync_group::{SyncGroup, SyncGroupResults};

pub use circuit_breaker::{BreakerConfig, BreakerState};

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use std::time::Duration;

use epics_base_rs::runtime::sync::{broadcast, mpsc, oneshot};
use parking_lot::Mutex;

use crate::channel::{AccessRights, ChannelInfo, alloc_cid, alloc_ioid, alloc_subid};
use crate::protocol::*;
use crate::repeater;
use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::snapshot::{DbrClass, Snapshot};
use epics_base_rs::types::{DbFieldType, EpicsValue, decode_dbr};

pub use state::{ChannelState, ConnectionEvent};

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

        let channel = client.create_channel_expanded(pv_name.clone());
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
pub use types::{CaException, CaExceptionHandler, CaExceptionKind};

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
    _coordinator: tokio::task::JoinHandle<()>,
    _search_task: tokio::task::JoinHandle<()>,
    _transport_task: tokio::task::JoinHandle<()>,
    _beacon_task: tokio::task::JoinHandle<()>,
}

/// Internal coordinator requests from CaChannel / public API
#[allow(dead_code)]
enum CoordRequest {
    RegisterChannel {
        cid: u32,
        pv_name: String,
        conn_tx: broadcast::Sender<ConnectionEvent>,
    },
    WaitConnected {
        cid: u32,
        reply: oneshot::Sender<()>,
    },
    Subscribe {
        cid: u32,
        subid: u32,
        mask: u16,
        deadband: f64,
        callback_tx: mpsc::Sender<CaResult<Snapshot>>,
        reply: oneshot::Sender<CaResult<()>>,
    },
    Unsubscribe {
        subid: u32,
    },
    MonitorConsumed {
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
    ForceRescanServer {
        server_addr: SocketAddr,
        kind: beacon_monitor::BeaconAnomalyKind,
    },
    /// Beacon arrival notification for the per-circuit receive
    /// watchdog. `anomaly = false` for healthy beacons (libca
    /// `beaconArrivalNotify` — refresh watchdog deadline);
    /// `anomaly = true` for `IdMismatch` / `PeriodCollapse` (libca
    /// `beaconAnomalyNotify` — set sticky flag, no immediate echo).
    /// `FirstSighting` deliberately does NOT generate this message:
    /// either we don't yet have a virtual circuit to the server, in
    /// which case the watchdog is irrelevant, or we do (we just
    /// pruned the BHE) and the next healthy beacon will refresh it
    /// naturally.
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
    /// Number of distinct CA servers we hold a virtual circuit to.
    /// Mirrors libca `ca_get_ioc_connection_count`.
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
    pub discovery: Option<crate::discovery::DiscoveryConfig>,

    /// Pre-built discovery backends to consult in addition to whatever
    /// `discovery` resolves to. Lets applications plug in custom
    /// `Backend` implementations (HTTP API, Consul, etcd, site CMDB,
    /// ...) without having to extend `DiscoveryConfig`. Each backend's
    /// `discover()` is called once at startup; addresses are deduped
    /// against `EPICS_CA_ADDR_LIST` and `discovery`-provided sources.
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
        // Run repeater registration in background — don't block client startup.
        epics_base_rs::runtime::task::spawn(async { repeater::ensure_repeater().await });

        // Round 50 (R50-G2): build the address list with hostname
        // info preserved so the search-engine refresh task can
        // re-resolve entries whose DNS name maps to a different IP
        // after IOC migration. Pre-fix `parse_addr_list()` returned
        // bare `Vec<SocketAddr>` resolved exactly once at startup,
        // permanently pinning the client to the first-resolved IPs.
        let mut addr_list = parse_addr_list_with_hostnames()?;

        // Service discovery: explicit config wins; otherwise honour
        // EPICS_CA_DISCOVERY env var. Custom `extra_backends` are then
        // appended. Results are merged with addr_list (deduped by
        // SocketAddr).
        let discovery_cfg = config.discovery.clone().or_else(crate::discovery::from_env);
        let mut backends: Vec<Box<dyn crate::discovery::Backend>> = match discovery_cfg {
            Some(cfg) => crate::discovery::build_backends(cfg),
            None => Vec::new(),
        };
        backends.extend(config.extra_backends);
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
        // Build the per-address SNI map. Two sources:
        // 1. EPICS_CA_NAME_SERVERS hostnames (added in F-G0).
        // 2. EPICS_CA_TLS_SNI_MAP for IPs reached via UDP search
        //    (F-G6). The CA SEARCH wire protocol carries no
        //    hostname, so a UDP-discovered TLS IOC otherwise has to
        //    fall back to the IP literal. Operators populate this map
        //    with `EPICS_CA_TLS_SNI_MAP="10.0.0.1=ioc1.lab.example.com 10.0.0.2:5064=ioc2.lab.example.com"`
        //    — the addr token may include or omit `:port`; the
        //    matching `connect_server(addr)` looks up the addr first,
        //    then the addr-with-port-zero (wildcard port) form.
        // Per-address overrides win over the global
        // EPICS_CA_TLS_SERVER_NAME (config.tls_server_name).
        #[cfg(feature = "experimental-rust-tls")]
        let sni_overrides: std::collections::HashMap<SocketAddr, String> = {
            let mut map: std::collections::HashMap<SocketAddr, String> = nameserver_entries
                .iter()
                .filter_map(|(addr, host)| host.clone().map(|h| (*addr, h)))
                .collect();
            for (addr, host) in parse_tls_sni_map() {
                map.insert(addr, host);
            }
            map
        };
        let nameserver_addrs: Vec<SocketAddr> =
            nameserver_entries.iter().map(|(a, _)| *a).collect();

        let (search_tx, search_rx) = mpsc::unbounded_channel();
        let (search_resp_tx, search_resp_rx) = mpsc::unbounded_channel();

        let (transport_tx, transport_rx) = mpsc::unbounded_channel();
        let (transport_evt_tx, transport_evt_rx) = mpsc::unbounded_channel();

        let (coord_tx, coord_rx) = mpsc::unbounded_channel();

        let search_attempts: types::SearchAttempts = Arc::new(dashmap::DashMap::new());
        let search_task = epics_base_rs::runtime::task::spawn(search::run_search_engine(
            addr_list,
            nameserver_addrs,
            search_rx,
            search_resp_tx,
            search_attempts.clone(),
        ));

        #[cfg(feature = "experimental-rust-tls")]
        let tls_arc = config.tls.as_ref().and_then(|t| match t {
            crate::tls::TlsConfig::Client(arc) => Some(arc.clone()),
            crate::tls::TlsConfig::Server(_) => {
                tracing::warn!("server-side TlsConfig passed to CaClient; ignoring");
                None
            }
        });

        let in_flight = InFlightOps::new();
        let snapshots: ChannelSnapshots = Arc::new(dashmap::DashMap::new());
        let server_writers: DirectServerWriters = Arc::new(dashmap::DashMap::new());
        let last_rx_at: ServerLastRxAt = Arc::new(dashmap::DashMap::new());

        let transport_task = {
            #[cfg(feature = "experimental-rust-tls")]
            {
                epics_base_rs::runtime::task::spawn(transport::run_transport_manager(
                    transport_rx,
                    transport_evt_tx,
                    in_flight.clone(),
                    server_writers.clone(),
                    last_rx_at.clone(),
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
                    server_writers.clone(),
                    last_rx_at.clone(),
                ))
            }
        };

        let diagnostics = Arc::new(CaDiagnostics::default());
        let exception_slot: types::CaExceptionSlot = Arc::new(parking_lot::RwLock::new(None));

        let (beacon_ctrl_tx, beacon_ctrl_rx) =
            mpsc::unbounded_channel::<beacon_monitor::BeaconControl>();

        let coordinator = epics_base_rs::runtime::task::spawn(run_coordinator(
            coord_rx,
            search_resp_rx,
            transport_evt_rx,
            search_tx.clone(),
            transport_tx.clone(),
            in_flight.clone(),
            snapshots.clone(),
            last_rx_at,
            diagnostics.clone(),
            exception_slot.clone(),
            search_attempts.clone(),
            beacon_ctrl_tx,
        ));

        let beacon_task = epics_base_rs::runtime::task::spawn(beacon_monitor::run_beacon_monitor(
            coord_tx.clone(),
            beacon_ctrl_rx,
        ));

        Ok(Self {
            search_tx,
            transport_tx,
            coord_tx,
            one_shot_channels: Mutex::new(OneShotChannelCache::default()),
            in_flight,
            snapshots,
            server_writers,
            diagnostics,
            search_attempts,
            exception_slot,
            _coordinator: coordinator,
            _search_task: search_task,
            _transport_task: transport_task,
            _beacon_task: beacon_task,
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

    /// Number of distinct CA servers this client currently holds an
    /// operational virtual circuit to. Mirrors libca
    /// `ca_get_ioc_connection_count()` (oldChannelNotify.cpp:891) —
    /// a circuit-level (not channel-level) count, useful for sizing
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
        // Wait briefly for the clear commands to be sent
        let _ = tokio::time::timeout(Duration::from_secs(2), rx).await;
    }

    /// Create a persistent channel. Returns immediately (starts searching in background).
    pub fn create_channel(&self, name: &str) -> CaChannel {
        self.create_channel_expanded(expand_pv_name(name))
    }

    fn create_channel_expanded(&self, pv_name: String) -> CaChannel {
        let cid = alloc_cid();
        let (conn_tx, _) = broadcast::channel(16);

        let _ = self.coord_tx.send(CoordRequest::RegisterChannel {
            cid,
            pv_name: pv_name.clone(),
            conn_tx: conn_tx.clone(),
        });

        let _ = self.search_tx.send(SearchRequest::Schedule {
            cid,
            pv_name: pv_name.clone(),
            reason: SearchReason::Initial,
        });

        let lifecycle = Arc::new(ChannelLifecycle {
            cid,
            coord_tx: self.coord_tx.clone(),
        });
        let channel_pv_name: Arc<str> = Arc::from(pv_name.as_str());
        CaChannel {
            cid,
            pv_name: channel_pv_name,
            coord_tx: self.coord_tx.clone(),
            transport_tx: self.transport_tx.clone(),
            in_flight: self.in_flight.clone(),
            snapshots: self.snapshots.clone(),
            server_writers: self.server_writers.clone(),
            conn_tx,
            cached_read: Arc::new(Mutex::new(None)),
            search_attempts: self.search_attempts.clone(),
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
        let timeout = Duration::from_secs_f64(timeout_secs);
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
/// We approximate the same outcome despite tokio's sync `Drop`:
///   * If a runtime is reachable, spawn a detached cleanup task that
///     sends `CoordRequest::Shutdown` (the coordinator's handler at
///     line ~2160 emits `ClearChannel` for every operational channel
///     before returning) and awaits the reply with a 2-s ceiling.
///     Once the reply lands, abort the four top-level tasks. Aborts
///     cascade through the `connections` HashMap → `ServerConnection`
///     Drop → per-circuit read/write tasks.
///   * If no runtime is reachable (Drop on a non-tokio thread, or
///     after the runtime has begun shutting down), abort the four
///     handles directly. No graceful drain — same fallback as before
///     this elaboration.
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
        let beacon_abort = self._beacon_task.abort_handle();

        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(async move {
                let (tx, rx) = oneshot::channel();
                if coord_tx.send(CoordRequest::Shutdown { reply: tx }).is_ok() {
                    // Bounded so a wedged coordinator doesn't keep
                    // the cleanup task alive indefinitely.
                    let _ = tokio::time::timeout(Duration::from_secs(2), rx).await;
                }
                coord_abort.abort();
                transport_abort.abort();
                search_abort.abort();
                beacon_abort.abort();
            });
        } else {
            // No runtime to drive the graceful sequence — fall back
            // to immediate abort to at least guarantee no task leak.
            self._coordinator.abort();
            self._transport_task.abort();
            self._search_task.abort();
            self._beacon_task.abort();
        }
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

#[derive(Clone)]
pub struct CaChannel {
    cid: u32,
    pv_name: Arc<str>,
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
    /// Refcounted lifecycle guard — see [`ChannelLifecycle`].
    _lifecycle: Arc<ChannelLifecycle>,
}

impl CaChannel {
    pub async fn wait_connected(&self, timeout: Duration) -> CaResult<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::WaitConnected {
            cid: self.cid,
            reply: reply_tx,
        });
        tokio::time::timeout(timeout, reply_rx)
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
        self.server_writers.get(&server_addr).map(|w| w.clone())
    }

    fn build_read_notify_frame(sid: u32, data_type: u16, count: u32, ioid: u32) -> Vec<u8> {
        let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
        hdr.data_type = data_type;
        hdr.cid = sid;
        hdr.available = ioid;
        if count > 0xFFFF {
            hdr.set_payload_size(0, count);
        } else {
            hdr.count = count as u16;
        }
        hdr.to_bytes_extended()
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

    fn build_write_frame(
        cmd: u16,
        sid: u32,
        data_type: u16,
        count: u32,
        ioid: Option<u32>,
        payload: Vec<u8>,
    ) -> Vec<u8> {
        let padded_len = align8(payload.len());
        let mut padded = payload;
        padded.resize(padded_len, 0);

        let mut hdr = CaHeader::new(cmd);
        hdr.data_type = data_type;
        hdr.cid = sid;
        if let Some(ioid) = ioid {
            hdr.available = ioid;
        }
        hdr.set_payload_size(padded.len(), count);

        let mut frame = hdr.to_bytes_extended();
        frame.extend_from_slice(&padded);
        frame
    }

    fn send_read_notify_fast(
        &self,
        snap: &ChannelSnapshotPublic,
        data_type: u16,
        count: u32,
        ioid: u32,
    ) -> CaResult<()> {
        if let Some(writer) = self.direct_writer(snap.server_addr) {
            return writer.send_frame(Self::build_read_notify_frame(
                snap.sid, data_type, count, ioid,
            ));
        }

        self.transport_tx
            .send(TransportCommand::ReadNotify {
                sid: snap.sid,
                data_type,
                count,
                ioid,
                server_addr: snap.server_addr,
            })
            .map_err(|_| CaError::Shutdown)
    }

    fn send_write_notify_fast(
        &self,
        snap: &ChannelSnapshotPublic,
        count: u32,
        ioid: u32,
        payload: Vec<u8>,
    ) -> CaResult<()> {
        if let Some(writer) = self.direct_writer(snap.server_addr) {
            return writer.send_frame(Self::build_write_frame(
                CA_PROTO_WRITE_NOTIFY,
                snap.sid,
                snap.native_type as u16,
                count,
                Some(ioid),
                payload,
            ));
        }

        self.transport_tx
            .send(TransportCommand::WriteNotify {
                sid: snap.sid,
                data_type: snap.native_type as u16,
                count,
                ioid,
                payload,
                server_addr: snap.server_addr,
            })
            .map_err(|_| CaError::Shutdown)
    }

    fn send_write_nowait_fast(
        &self,
        snap: &ChannelSnapshotPublic,
        count: u32,
        payload: Vec<u8>,
    ) -> CaResult<()> {
        if let Some(writer) = self.direct_writer(snap.server_addr) {
            return writer.send_frame(Self::build_write_frame(
                CA_PROTO_WRITE,
                snap.sid,
                snap.native_type as u16,
                count,
                None,
                payload,
            ));
        }

        self.transport_tx
            .send(TransportCommand::Write {
                sid: snap.sid,
                data_type: snap.native_type as u16,
                count,
                payload,
                server_addr: snap.server_addr,
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
        let mut groups: HashMap<SocketAddr, BulkReadGroup> = HashMap::new();
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
                let frame = Self::build_read_notify_frame(
                    cached.sid,
                    cached.data_type,
                    cached.element_count,
                    cached.ioid,
                );
                let kind = PendingKind::Warm {
                    ioid: cached.ioid,
                    in_flight: ch.in_flight.clone(),
                    cached_read_slot: ch.cached_read.clone(),
                    cached,
                };
                (frame, kind)
            } else {
                let ioid = alloc_ioid();
                ch.in_flight.reads.insert(
                    ioid,
                    ReadWaiter::OneShot {
                        cid: ch.cid,
                        mode: ReadReplyMode::Plain,
                        reply_tx,
                    },
                );
                let frame = Self::build_read_notify_frame(
                    snap.sid,
                    snap.native_type as u16,
                    snap.element_count,
                    ioid,
                );
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
                    .entry(snap.server_addr)
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

        let deadline = tokio::time::Instant::now() + timeout;
        for p in pending {
            let Pending {
                index,
                reply_rx,
                kind,
            } = p;
            let result = tokio::time::timeout_at(deadline, reply_rx).await;
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
                        let warm_ioid = alloc_ioid();
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

    pub async fn get_with_timeout(&self, timeout: Duration) -> CaResult<(DbFieldType, EpicsValue)> {
        let snap = self.snapshot()?;

        let ioid = alloc_ioid();
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
            self.send_read_notify_fast(&snap, snap.native_type as u16, snap.element_count, ioid)
        {
            self.in_flight.reads.remove(&ioid);
            return Err(e);
        }

        let result = tokio::time::timeout(timeout, reply_rx).await;
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
    pub async fn get_with_metadata_count(&self, class: DbrClass, count: u32) -> CaResult<Snapshot> {
        let snap = self.snapshot()?;

        let request_count = if count > 0 {
            count.min(snap.element_count)
        } else {
            snap.element_count
        };

        let native = DbFieldType::from_u16(snap.native_type as u16)?;
        let request_type = match class {
            DbrClass::Time => native.time_dbr_type(),
            DbrClass::Ctrl => native.ctrl_dbr_type(),
            DbrClass::Sts => native as u16 + 7,
            DbrClass::Gr => native as u16 + 21,
            DbrClass::Plain => native as u16,
        };

        let ioid = alloc_ioid();
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

        let result = tokio::time::timeout(Duration::from_secs(30), reply_rx).await;
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

        let ioid = alloc_ioid();
        let (reply_tx, reply_rx) = oneshot::channel();
        // Direct registry insert (Option C Phase A).
        self.in_flight.writes.insert(ioid, (self.cid, reply_tx));

        let payload = value.to_bytes();
        let count = value.count() as u32;
        if let Err(e) = self.send_write_notify_fast(&snap, count, ioid, payload) {
            self.in_flight.writes.remove(&ioid);
            return Err(e);
        }

        // Default put timeout configurable via EPICS_CA_PUT_TIMEOUT (seconds).
        let default_secs = epics_base_rs::runtime::env::get("EPICS_CA_PUT_TIMEOUT")
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(30.0);
        let result = tokio::time::timeout(Duration::from_secs_f64(default_secs), reply_rx).await;
        self.in_flight.writes.remove(&ioid);
        result
            .map_err(|_| CaError::Timeout)?
            .map_err(|_| CaError::Shutdown)?
    }

    /// Write with completion callback and configurable timeout.
    pub async fn put_with_timeout(&self, value: &EpicsValue, timeout: Duration) -> CaResult<()> {
        let snap = self.snapshot()?;

        let ioid = alloc_ioid();
        let (reply_tx, reply_rx) = oneshot::channel();
        // Direct registry insert (Option C Phase A).
        self.in_flight.writes.insert(ioid, (self.cid, reply_tx));

        let payload = value.to_bytes();
        let count = value.count() as u32;
        if let Err(e) = self.send_write_notify_fast(&snap, count, ioid, payload) {
            self.in_flight.writes.remove(&ioid);
            return Err(e);
        }

        let result = tokio::time::timeout(timeout, reply_rx).await;
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

        let payload = value.to_bytes();
        let count = value.count() as u32;
        self.send_write_nowait_fast(&snap, count, payload)
    }

    pub async fn subscribe(&self) -> CaResult<MonitorHandle> {
        self.subscribe_with_deadband(0.0).await
    }

    /// Subscribe with client-side deadband filtering.
    /// Events where |new - old| < deadband are suppressed (scalar values only).
    pub async fn subscribe_with_deadband(&self, deadband: f64) -> CaResult<MonitorHandle> {
        let subid = alloc_subid();
        // Bounded queue prevents unbounded memory growth on slow consumers.
        // EVENTS_OFF will fire when outstanding hits FLOW_CONTROL_OFF_THRESHOLD,
        // but the queue gives the application a buffer before drops kick in.
        let queue_size = epics_base_rs::runtime::env::get("EPICS_CA_MONITOR_QUEUE")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(256)
            .max(8);
        let (callback_tx, callback_rx) = mpsc::channel(queue_size);

        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::Subscribe {
            cid: self.cid,
            subid,
            mask: DBE_VALUE | DBE_LOG | DBE_ALARM,
            deadband,
            callback_tx,
            reply: reply_tx,
        });

        reply_rx.await.map_err(|_| CaError::Shutdown)??;

        Ok(MonitorHandle {
            subid,
            callback_rx,
            coord_tx: self.coord_tx.clone(),
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
    /// the current state via [`Self::access_rights`] /
    /// [`Self::is_connected`].
    pub fn connection_events(&self) -> broadcast::Receiver<ConnectionEvent> {
        self.conn_tx.subscribe()
    }

    /// Convenience wrapper around [`Self::connection_events`] that
    /// invokes `cb` on every `AccessRightsChanged`. Returns a
    /// [`tokio::task::JoinHandle`] you can drop to stop watching.
    /// Mirrors libca `ca_replace_access_rights_event` at the
    /// callback-registration shape.
    pub fn on_access_rights_change<F>(&self, mut cb: F) -> tokio::task::JoinHandle<()>
    where
        F: FnMut(AccessRights) + Send + 'static,
    {
        let mut rx = self.conn_tx.subscribe();
        epics_base_rs::runtime::task::spawn(async move {
            while let Ok(evt) = rx.recv().await {
                if let ConnectionEvent::AccessRightsChanged { read, write } = evt {
                    cb(AccessRights { read, write });
                }
            }
        })
    }

    /// Convenience wrapper around [`Self::connection_events`] that
    /// invokes `cb(true)` on `Connected` and `cb(false)` on
    /// `Disconnected`. Mirrors libca
    /// `ca_change_connection_event(chid, callback)` (oldChannelNotify.cpp:229) —
    /// drop the returned handle to stop watching.
    pub fn on_connection_change<F>(&self, mut cb: F) -> tokio::task::JoinHandle<()>
    where
        F: FnMut(bool) + Send + 'static,
    {
        let mut rx = self.conn_tx.subscribe();
        epics_base_rs::runtime::task::spawn(async move {
            while let Ok(evt) = rx.recv().await {
                match evt {
                    ConnectionEvent::Connected => cb(true),
                    ConnectionEvent::Disconnected => cb(false),
                    _ => {}
                }
            }
        })
    }

    /// Server's IP address as a string (e.g. `"10.0.0.5:5064"`).
    /// Mirrors libca `ca_host_name(chid)` (oldChannelNotify.cpp:189).
    /// Returns `Err` if the channel hasn't connected yet — pvxs
    /// returns `"<disconnected>"` for the same case; we surface
    /// the typed error instead so callers can decide.
    pub async fn host_name(&self) -> CaResult<String> {
        let info = self.info().await?;
        Ok(info.server_addr.to_string())
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

/// Handle for a monitor subscription. Dropping it cancels the subscription.
pub struct MonitorHandle {
    subid: u32,
    callback_rx: mpsc::Receiver<CaResult<Snapshot>>,
    coord_tx: mpsc::UnboundedSender<CoordRequest>,
}

impl MonitorHandle {
    pub async fn recv(&mut self) -> Option<CaResult<Snapshot>> {
        let result = self.callback_rx.recv().await;
        if result.is_some() {
            let _ = self
                .coord_tx
                .send(CoordRequest::MonitorConsumed { subid: self.subid });
        }
        result
    }
}

impl Drop for MonitorHandle {
    fn drop(&mut self) {
        let _ = self
            .coord_tx
            .send(CoordRequest::Unsubscribe { subid: self.subid });
    }
}

// --- Coordinator ---

const FLOW_CONTROL_OFF_THRESHOLD: usize = 10;
const FLOW_CONTROL_ON_THRESHOLD: usize = 5;

#[derive(Default)]
struct FlowControlState {
    outstanding: usize,
    active: bool,
}

fn flow_control_note_queued(
    flow_control: &mut HashMap<SocketAddr, FlowControlState>,
    server_addr: SocketAddr,
    transport_tx: &mpsc::UnboundedSender<TransportCommand>,
) {
    let state = flow_control.entry(server_addr).or_default();
    state.outstanding = state.outstanding.saturating_add(1);
    if !state.active && state.outstanding >= FLOW_CONTROL_OFF_THRESHOLD {
        let _ = transport_tx.send(TransportCommand::EventsOff { server_addr });
        state.active = true;
    }
}

fn flow_control_note_consumed(
    flow_control: &mut HashMap<SocketAddr, FlowControlState>,
    server_addr: SocketAddr,
    count: usize,
    transport_tx: &mpsc::UnboundedSender<TransportCommand>,
) {
    if count == 0 {
        return;
    }
    let Some(state) = flow_control.get_mut(&server_addr) else {
        return;
    };
    state.outstanding = state.outstanding.saturating_sub(count);
    if state.active && state.outstanding <= FLOW_CONTROL_ON_THRESHOLD {
        let _ = transport_tx.send(TransportCommand::EventsOn { server_addr });
        state.active = false;
    }
    if !state.active && state.outstanding == 0 {
        flow_control.remove(&server_addr);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_coordinator(
    mut coord_rx: mpsc::UnboundedReceiver<CoordRequest>,
    mut search_rx: mpsc::UnboundedReceiver<SearchResponse>,
    mut transport_rx: mpsc::UnboundedReceiver<TransportEvent>,
    search_tx: mpsc::UnboundedSender<SearchRequest>,
    transport_tx: mpsc::UnboundedSender<TransportCommand>,
    in_flight: types::InFlightOps,
    snapshots: ChannelSnapshots,
    last_rx_at: ServerLastRxAt,
    diag: Arc<CaDiagnostics>,
    exception_slot: types::CaExceptionSlot,
    search_attempts: types::SearchAttempts,
    beacon_ctrl_tx: mpsc::UnboundedSender<beacon_monitor::BeaconControl>,
) {
    let mut channels: HashMap<u32, ChannelInner> = HashMap::new();
    let mut pending_wait_connected: HashMap<u32, Vec<oneshot::Sender<()>>> = HashMap::new();
    let mut pending_found: HashMap<u32, SocketAddr> = HashMap::new();
    let mut subscriptions = SubscriptionRegistry::new();
    // Reverse index: server_addr -> set of cids last seen on that server.
    // Keep disconnected channels indexed so beacon anomalies can trigger
    // immediate re-search for the affected IOC.
    let mut server_channels: HashMap<SocketAddr, HashSet<u32>> = HashMap::new();
    let mut flow_control: HashMap<SocketAddr, FlowControlState> = HashMap::new();
    // Per-server CA minor protocol version, populated from
    // CA_PROTO_VERSION on TCP handshake. Powers `host_minor_protocol`.
    let mut server_minor_version: HashMap<SocketAddr, u16> = HashMap::new();

    loop {
        tokio::select! {
            req = coord_rx.recv() => {
                let Some(req) = req else { return };
                match req {
                    CoordRequest::RegisterChannel { cid, pv_name, conn_tx } => {
                        // Drain any waiters that arrived before registration.
                        let early_waiters = pending_wait_connected
                            .remove(&cid)
                            .unwrap_or_default();
                        channels.insert(cid, ChannelInner {
                            cid,
                            pv_name: pv_name.clone(),
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
                            server_channels.entry(server_addr).or_default().insert(cid);
                            let _ = transport_tx.send(TransportCommand::CreateChannel {
                                cid,
                                pv_name,
                                server_addr,
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
                    CoordRequest::Subscribe { cid, subid, mask, deadband, callback_tx, reply } => {
                        if let Some(ch) = channels.get(&cid) {
                            let server_addr = ch.server_addr.unwrap_or_else(|| {
                                SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
                            });
                            let connected = ch.state == ChannelState::Connected;
                            let data_type = ch.native_type.map(|t| t as u16 + 14);
                            let count = ch.native_type.map(|_| ch.element_count);

                            subscriptions.add(subscription::SubscriptionRecord {
                                subid,
                                cid,
                                data_type,
                                count,
                                mask,
                                server_addr,
                                deadband,
                                callback_tx,
                                needs_restore: !connected,
                                last_value: None,
                                pending_deliveries: 0,
                            });

                            if connected {
                                let _ = transport_tx.send(TransportCommand::Subscribe {
                                    sid: ch.sid,
                                    data_type: data_type.expect("connected channel has native type"),
                                    count: count.expect("connected channel has element count"),
                                    subid,
                                    mask,
                                    server_addr,
                                });
                            }
                            let _ = reply.send(Ok(()));
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
                                        let _ = transport_tx.send(TransportCommand::Unsubscribe {
                                            sid: ch.sid,
                                            subid,
                                            data_type,
                                            server_addr: ch.server_addr.unwrap(),
                                        });
                                    }
                                }
                            }
                        }
                        if let Some(rec) = subscriptions.remove(subid) {
                            flow_control_note_consumed(
                                &mut flow_control,
                                rec.server_addr,
                                rec.pending_deliveries,
                                &transport_tx,
                            );
                        }
                    }
                    CoordRequest::MonitorConsumed { subid } => {
                        if let Some(server_addr) = subscriptions.mark_consumed(subid) {
                            flow_control_note_consumed(
                                &mut flow_control,
                                server_addr,
                                1,
                                &transport_tx,
                            );
                        }
                    }
                    CoordRequest::DropChannel { cid } => {
                        // Cancel all subscriptions for this channel
                        let sub_ids = subscriptions.for_cid(cid);
                        for subid in sub_ids {
                            if let Some(rec) = subscriptions.get(subid) {
                                if let Some(ch) = channels.get(&cid) {
                                    if ch.state == ChannelState::Connected {
                                        if let Some(data_type) = rec.data_type {
                                            let _ = transport_tx.send(TransportCommand::Unsubscribe {
                                                sid: ch.sid,
                                                subid,
                                                data_type,
                                                server_addr: ch.server_addr.unwrap(),
                                            });
                                        }
                                    }
                                }
                            }
                            if let Some(rec) = subscriptions.remove(subid) {
                                flow_control_note_consumed(
                                    &mut flow_control,
                                    rec.server_addr,
                                    rec.pending_deliveries,
                                    &transport_tx,
                                );
                            }
                        }

                        // Clear channel on server + clean reverse index
                        if let Some(ch) = channels.get(&cid) {
                            if ch.state.is_operational() {
                                let _ = transport_tx.send(TransportCommand::ClearChannel {
                                    cid,
                                    sid: ch.sid,
                                    server_addr: ch.server_addr.unwrap(),
                                });
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
                                remove_server_channel(&mut server_channels, addr, cid);
                            }
                        }
                        channels.remove(&cid);
                        snapshots.remove(&cid);
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
                            last_rx_at.get(&addr).map(|e| e.value().elapsed())
                        });
                        let _ = reply.send(delay);
                    }
                    CoordRequest::GetHostMinorProtocol { cid, reply } => {
                        let v = channels.get(&cid).and_then(|ch| {
                            if !ch.state.is_operational() {
                                return None;
                            }
                            let addr = ch.server_addr?;
                            server_minor_version.get(&addr).copied()
                        });
                        let _ = reply.send(v);
                    }
                    CoordRequest::GetIocConnectionCount { reply } => {
                        // Count distinct servers with at least one
                        // operational channel — mirrors libca which
                        // counts virtual circuits, not channels.
                        let mut servers = HashSet::<SocketAddr>::new();
                        for ch in channels.values() {
                            if let Some(addr) = ch.server_addr {
                                if ch.state.is_operational() {
                                    servers.insert(addr);
                                }
                            }
                        }
                        let _ = reply.send(servers.len());
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
                                    });
                                }
                            }
                        }
                        let _ = reply.send(());
                        return; // Exit coordinator loop
                    }
                    CoordRequest::ForceRescanServer { server_addr, kind } => {
                        // FirstSighting is a per-client bookkeeping
                        // event (our beacon map was empty for this
                        // server), not a server-side anomaly. Logging
                        // it as a warning every time a fresh CaClient
                        // hears its first beacon was misleading and
                        // would over-promote a benign condition.
                        // Reserve the warn-level "IOC may have
                        // restarted" message for real restart signals.
                        let is_real_restart = matches!(
                            kind,
                            beacon_monitor::BeaconAnomalyKind::IdMismatch
                                | beacon_monitor::BeaconAnomalyKind::PeriodCollapse
                        );
                        if is_real_restart {
                            diag.beacon_anomalies.fetch_add(1, Ordering::Relaxed);
                            diag.record(DiagEvent::BeaconAnomaly { server: server_addr });
                            tracing::warn!(
                                server = %server_addr,
                                ?kind,
                                "beacon anomaly detected — IOC may have restarted"
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
                                if let Some(old_addr) = ch.server_addr {
                                    remove_server_channel(&mut server_channels, old_addr, cid);
                                }
                                ch.state = ChannelState::Connecting;
                                ch.server_addr = Some(server_addr);
                                server_channels.entry(server_addr).or_default().insert(cid);
                                let _ = transport_tx.send(TransportCommand::CreateChannel {
                                    cid,
                                    pv_name: ch.pv_name.to_string(),
                                    server_addr,
                                });
                            }
                        } else {
                            // Channel not registered yet — stash the Found
                            // response so RegisterChannel can process it.
                            pending_found.insert(cid, server_addr);
                        }
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
                    TransportEvent::ChannelCreated { cid, sid, data_type, element_count, access, server_addr } => {
                        if let Some(ch) = channels.get_mut(&cid) {
                            let was_disconnected = matches!(ch.state, ChannelState::Disconnected);
                            let dbr_type = DbFieldType::from_u16(data_type).ok();
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

                            // Restore subscriptions
                            let (restored, stale) = subscriptions.restore_for_channel(
                                cid,
                                sid,
                                data_type,
                                element_count,
                                server_addr,
                                &transport_tx,
                            );
                            diag.connections.fetch_add(1, Ordering::Relaxed);
                            diag.record(DiagEvent::Connected { pv: ch.pv_name.to_string(), server: server_addr });
                            if restored > 0 || stale > 0 {
                                diag.reconnections.fetch_add(1, Ordering::Relaxed);
                                diag.subscriptions_restored.fetch_add(restored as u64, Ordering::Relaxed);
                                diag.subscriptions_stale.fetch_add(stale as u64, Ordering::Relaxed);
                                diag.record(DiagEvent::Reconnected { pv: ch.pv_name.to_string(), restored, stale });
                                eprintln!("CA: {}: restored {restored} subscriptions ({stale} stale removed)", ch.pv_name);
                            }

                            // Notify search engine of successful connect (clears penalty).
                            let _ = search_tx.send(SearchRequest::ConnectResult {
                                cid,
                                success: true,
                                server_addr,
                            });
                        }
                    }
                    TransportEvent::MonitorData { subid, data_type, count, data } => {
                        use subscription::MonitorDeliveryOutcome;
                        match subscriptions.on_monitor_data(subid, data_type, count, &data) {
                            MonitorDeliveryOutcome::Queued(server_addr) => {
                                flow_control_note_queued(
                                    &mut flow_control,
                                    server_addr,
                                    &transport_tx,
                                );
                            }
                            MonitorDeliveryOutcome::Dropped(_server_addr) => {
                                diag.dropped_monitors.fetch_add(1, Ordering::Relaxed);
                                tracing::warn!(subid, "monitor dropped (consumer queue full)");
                                metrics::counter!("ca_client_dropped_monitors_total").increment(1);
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
                    TransportEvent::ServerError {
                        eca_status,
                        original_request,
                        message,
                        server_addr,
                    } => {
                        // Already logged in transport layer; surface
                        // through the exception handler if registered.
                        // CaException.status is the ECA code (libca
                        // parity); the original request cmd goes into
                        // the message text as diagnostic context.
                        let annotated = match original_request {
                            Some(cmd) => {
                                if message.is_empty() {
                                    format!("(while processing cmd={cmd})")
                                } else {
                                    format!("{message} (while processing cmd={cmd})")
                                }
                            }
                            None => message,
                        };
                        types::dispatch_exception(
                            &exception_slot,
                            types::CaException {
                                kind: types::CaExceptionKind::ServerError,
                                message: annotated,
                                server_addr: Some(server_addr),
                                pv_name: None,
                                status: Some(eca_status),
                            },
                        );
                    }
                    TransportEvent::TcpClosed { server_addr } => {
                        let n_affected = server_channels
                            .get(&server_addr)
                            .map(|s| s.len())
                            .unwrap_or(0);
                        tracing::warn!(server = %server_addr, channels = n_affected, "TCP circuit closed");
                        metrics::counter!("ca_client_tcp_closed_total", "server" => server_addr.to_string()).increment(1);
                        flow_control.remove(&server_addr);
                        last_rx_at.remove(&server_addr);
                        server_minor_version.remove(&server_addr);
                        handle_disconnect(&mut channels, &mut subscriptions, &mut server_channels, &search_tx, server_addr, &diag, &in_flight, &snapshots);
                    }
                    TransportEvent::ServerDisconnect { cid, server_addr } => {
                        // Single channel disconnect (CA_PROTO_SERVER_DISCONN).
                        // Server is telling us this specific cid is gone —
                        // wake any in-flight read/write waiters tied to it
                        // so blocked `caget`/`caput` futures fail with
                        // `Disconnected` instead of stalling until their
                        // own outer timeout fires. Mirrors the bulk
                        // `handle_disconnect` wake path used for
                        // `TcpClosed` (mod.rs ~1877). Without this,
                        // SERVER_DISCONN was structurally dead-letter:
                        // we re-searched but never released callers who
                        // were waiting on a response that the server
                        // had just told us would never come.
                        if let Some(ch) = channels.get_mut(&cid) {
                            if ch.server_addr == Some(server_addr) {
                                ch.state = ChannelState::Disconnected;
                                snapshots.remove(&cid);
                                let _ = ch.conn_tx.send(ConnectionEvent::Disconnected);

                                let pv_name = ch.pv_name.to_string();
                                let cids = vec![cid];
                                let cleared = subscriptions.mark_disconnected(&cids);
                                for (addr, count) in cleared {
                                    flow_control_note_consumed(
                                        &mut flow_control,
                                        addr,
                                        count,
                                        &transport_tx,
                                    );
                                }

                                // Drain blocked read/write waiters for this cid.
                                let mut affected = HashSet::with_capacity(1);
                                affected.insert(cid);
                                drain_waiters_for_cids(&affected, &in_flight);

                                // Re-search
                                let _ = search_tx.send(SearchRequest::Schedule {
                                    cid,
                                    pv_name: pv_name.clone(),
                                    reason: SearchReason::Reconnect,
                                });

                                // CA-130: surface to per-client handler.
                                types::dispatch_exception(
                                    &exception_slot,
                                    types::CaException {
                                        kind: types::CaExceptionKind::ServerDisconnect,
                                        message: "server-initiated channel close".to_string(),
                                        server_addr: Some(server_addr),
                                        pv_name: Some(pv_name),
                                        status: None,
                                    },
                                );
                            }
                        }
                    }
                    TransportEvent::CircuitUnresponsive { server_addr } => {
                        diag.unresponsive_events.fetch_add(1, Ordering::Relaxed);
                        diag.record(DiagEvent::Unresponsive { server: server_addr });
                        tracing::warn!(server = %server_addr, "circuit unresponsive (echo timeout)");
                        metrics::counter!("ca_client_unresponsive_total", "server" => server_addr.to_string()).increment(1);
                        for ch in channels.values_mut() {
                            if ch.server_addr == Some(server_addr)
                                && ch.state == ChannelState::Connected
                            {
                                ch.state = ChannelState::Unresponsive;
                                if let Some(mut snap) = snapshots.get_mut(&ch.cid) {
                                    snap.state = ChannelState::Unresponsive;
                                }
                                let _ = ch.conn_tx.send(ConnectionEvent::Unresponsive);
                            }
                        }
                    }
                    TransportEvent::CircuitResponsive { server_addr } => {
                        diag.record(DiagEvent::Responsive { server: server_addr });
                        tracing::info!(server = %server_addr, "circuit responsive again");
                        for ch in channels.values_mut() {
                            if ch.server_addr == Some(server_addr)
                                && ch.state == ChannelState::Unresponsive
                            {
                                ch.state = ChannelState::Connected;
                                if let Some(mut snap) = snapshots.get_mut(&ch.cid) {
                                    snap.state = ChannelState::Connected;
                                }
                                let _ = ch.conn_tx.send(ConnectionEvent::Connected);
                            }
                        }
                    }
                    TransportEvent::ServerVersion { server_addr, minor_version } => {
                        // libca exposes this via `ca_host_minor_protocol`
                        // (BUG_ARCHAEOLOGY d763541): used by gateways /
                        // nameservers to report the connected server's
                        // CA wire version. Read from CA_PROTO_VERSION
                        // during TCP handshake; cleared on TcpClosed.
                        server_minor_version.insert(server_addr, minor_version);
                    }
                    TransportEvent::ServerConnected { server_addr } => {
                        // libca bhe-on-connect parity: tell the beacon
                        // monitor to drop its per-server EMA so the
                        // next beacon reseeds `period_estimate` from
                        // the live cadence. Without this, an archiver
                        // that reconnects to a server in the middle of
                        // its `online_notify_task` ramp-up would log a
                        // PeriodCollapse cascade against its stale
                        // steady-state estimate.
                        let _ = beacon_ctrl_tx.send(
                            beacon_monitor::BeaconControl::ResetServer { server_addr },
                        );
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_disconnect(
    channels: &mut HashMap<u32, ChannelInner>,
    subscriptions: &mut SubscriptionRegistry,
    server_channels: &mut HashMap<SocketAddr, HashSet<u32>>,
    search_tx: &mpsc::UnboundedSender<SearchRequest>,
    server_addr: SocketAddr,
    diag: &CaDiagnostics,
    in_flight: &types::InFlightOps,
    snapshots: &ChannelSnapshots,
) {
    let mut affected_cids = Vec::new();
    let now = std::time::Instant::now();

    for ch in channels.values_mut() {
        if ch.server_addr == Some(server_addr)
            && (ch.state.is_operational() || ch.state == ChannelState::Connecting)
        {
            ch.state = ChannelState::Disconnected;
            snapshots.remove(&ch.cid);
            affected_cids.push(ch.cid);
            let _ = ch.conn_tx.send(ConnectionEvent::Disconnected);

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
        }
    }
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
    server_channels.remove(&server_addr);
    let _ = subscriptions.mark_disconnected(&affected_cids);

    // Fail pending read/write waiters for affected channels so callers
    // don't hang forever waiting for a response that will never arrive.
    let affected: HashSet<u32> = affected_cids.into_iter().collect();
    drain_waiters_for_cids(&affected, in_flight);
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

fn remove_server_channel(
    server_channels: &mut HashMap<SocketAddr, HashSet<u32>>,
    server_addr: SocketAddr,
    cid: u32,
) {
    if let Some(set) = server_channels.get_mut(&server_addr) {
        set.remove(&cid);
        if set.is_empty() {
            server_channels.remove(&server_addr);
        }
    }
}

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
}

impl AddrEntry {
    pub fn new(sock: SocketAddr, hostname: Option<String>, port: u16) -> Self {
        Self {
            sock,
            hostname,
            port,
        }
    }

    /// Re-resolve the hostname (if any) and return the freshened
    /// SocketAddr. Returns `Ok(self.sock)` unchanged when there's
    /// nothing to refresh (literal IP entry).
    ///
    /// Round 50 (R50-G2): wired into the search engine's periodic
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
        Ok(new_sock)
    }
}

/// `parse_addr_list` variant that retains hostname per entry.
/// Round 50 (R50-G2): this is the live caller from
/// `new_with_config()`; the search engine periodically calls
/// `AddrEntry::refresh_dns` on each entry so that an
/// `EPICS_CA_ADDR_LIST` hostname whose DNS resolution changes
/// (e.g. an IOC migrates between hosts) is picked up at runtime
/// instead of permanently pinning the client to the first-resolved
/// IP. Closes the long-standing upstream-tracking item for
/// epics-base#488.
pub(crate) fn parse_addr_list_with_hostnames() -> CaResult<Vec<AddrEntry>> {
    let mut addrs: Vec<AddrEntry> = Vec::new();
    let default_port = epics_base_rs::runtime::env::get("EPICS_CA_SERVER_PORT")
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(CA_SERVER_PORT);
    if let Some(list) = epics_base_rs::runtime::env::get("EPICS_CA_ADDR_LIST") {
        for entry in list.split_whitespace() {
            let (host_raw, port) = if entry.contains(':') {
                if let Some((h, p)) = entry.rsplit_once(':') {
                    let port: u16 = match p.parse() {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    (h.to_string(), port)
                } else {
                    (entry.to_string(), default_port)
                }
            } else {
                (entry.to_string(), default_port)
            };
            // Pure-IP entry has no DNS to refresh.
            let hostname = if host_raw.parse::<Ipv4Addr>().is_ok() {
                None
            } else {
                Some(host_raw.clone())
            };
            match resolve_host(&host_raw, port) {
                Ok(sock) => addrs.push(AddrEntry::new(sock, hostname, port)),
                Err(e) => tracing::debug!(token = %entry, error = %e,
                    "EPICS_CA_ADDR_LIST: dropped unresolvable entry"),
            }
        }
    }

    // Round 50 (R50-G2): mirror the legacy `parse_addr_list`'s
    // AUTO_ADDR_LIST + broadcast-fallback behaviour. Without these
    // the new live caller would silently drop UDP broadcast
    // discovery for multi-NIC clients and the limited-broadcast
    // last-resort fallback. The added entries are IP literals
    // (`hostname = None`) so the periodic refresh task short-
    // circuits them.
    let auto_addr = epics_base_rs::runtime::env::get_or("EPICS_CA_AUTO_ADDR_LIST", "YES");
    if auto_addr.eq_ignore_ascii_case("YES") {
        let server_port = default_port;
        for bcast in crate::server::addr_list::discover_broadcast_addrs() {
            let sock = SocketAddr::V4(SocketAddrV4::new(bcast, server_port));
            if !addrs.iter().any(|e| e.sock == sock) {
                addrs.push(AddrEntry::new(sock, None, server_port));
            }
        }
        // Limited broadcast as a last-resort fallback (multi-NIC
        // enumeration may have returned nothing useful — e.g. on
        // point-to-point links).
        let fallback = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, server_port));
        if !addrs.iter().any(|e| e.sock == fallback) {
            addrs.push(AddrEntry::new(fallback, None, server_port));
        }
    }
    Ok(addrs)
}

#[cfg(test)]
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
    if epics_base_rs::runtime::env::get_or("EPICS_CA_USE_SHELL_VARS", "NO")
        .eq_ignore_ascii_case("YES")
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

/// Parse `EPICS_CA_TLS_SNI_MAP` — whitespace-separated `IP[:port]=hostname`
/// entries. Returns a vec of `(SocketAddr, hostname)` pairs ready to
/// merge into the per-server SNI override map.
///
/// The CA SEARCH wire protocol carries no hostname information, so a
/// UDP-discovered TLS IOC at e.g. `10.0.0.1:5064` cannot be reached
/// with a hostname-bound cert unless operators provide an explicit
/// IP→hostname mapping. F-G6 (April 2026): adds this env so multi-IOC
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

/// Parse `EPICS_CA_NAME_SERVERS` — whitespace-separated host[:port] entries
/// reachable over TCP. Returns each entry's resolved [`SocketAddr`] alongside
/// the operator-supplied hostname when one was given (None for raw-IP
/// entries). The hostname is later threaded into the TLS handshake as the
/// SNI / cert-verification name for that specific server, so multi-IOC TLS
/// deployments with hostname-bound certs work without a single global
/// `EPICS_CA_TLS_SERVER_NAME` override.
pub(crate) fn parse_nameserver_list() -> Vec<(SocketAddr, Option<String>)> {
    let Some(list) = epics_base_rs::runtime::env::get("EPICS_CA_NAME_SERVERS") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in list.split_whitespace() {
        if entry.contains(':') {
            // Try as raw `IP:port` first — if it parses, no hostname.
            if let Ok(addr) = entry.parse::<SocketAddr>() {
                out.push((addr, None));
                continue;
            }
            // Otherwise treat it as `host:port` and remember the host.
            let Some((host, port_str)) = entry.rsplit_once(':') else {
                continue;
            };
            let Ok(port) = port_str.parse::<u16>() else {
                continue;
            };
            if let Ok(addr) = resolve_host(host, port) {
                let hostname = if host.parse::<std::net::IpAddr>().is_ok() {
                    None
                } else {
                    Some(host.to_string())
                };
                out.push((addr, hostname));
            }
        } else {
            // Bare hostname (no port) — treat as DNS name even if it
            // happens to look like an IP literal (caller intent is
            // unambiguous when no port is specified).
            if let Ok(addr) = resolve_host(entry, CA_SERVER_PORT) {
                let hostname = if entry.parse::<std::net::IpAddr>().is_ok() {
                    None
                } else {
                    Some(entry.to_string())
                };
                out.push((addr, hostname));
            }
        }
    }
    out
}

// Round 50 (R50-G2): legacy `parse_addr_list() -> Vec<SocketAddr>`
// removed. The hostname-preserving `parse_addr_list_with_hostnames()`
// (line ~2944) is the only live caller — it carries DNS context
// for the search engine's periodic refresh loop and matches the
// legacy function's AUTO_ADDR_LIST + per-NIC broadcast + limited-
// broadcast fallback semantics.

#[cfg(test)]
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
