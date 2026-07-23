//! Channel Access server — CaServer and CaServerBuilder.
//!
//! CaServerBuilder delegates all IOC-level bootstrap logic to
//! [`epics_base_rs::server::ioc_builder::IocBuilder`] and adds only
//! CA-specific configuration (port, access security).

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.

use std::sync::Arc;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::net::cas_server_port;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

use super::stats::ServerStats;
use super::{addr_list, beacon, tcp, udp};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::{access_security, autosave, device_support, ioc_builder, iocsh};

/// Builder for CaServer configuration.
///
/// IOC-level methods (`pv`, `record`, `db_file`, `register_device_support`,
/// etc.) delegate to the inner [`ioc_builder::IocBuilder`].  Only `port()`,
/// `acf()`, and `acf_file()` are CA-specific.
pub struct CaServerBuilder {
    ioc: ioc_builder::IocBuilder,
    /// UDP discovery port — clients send SEARCH packets here. Defaults
    /// to `EPICS_CA_SERVER_PORT` or 5064.
    port: u16,
    /// Explicit TCP listen port override. When `None`, resolved at
    /// `build()` time from `EPICS_CAS_SERVER_PORT`, falling back to
    /// `port`. Lets the canonical UDP port stay at 5064 while each
    /// IOC binds a unique TCP port (epics-base PR #69).
    tcp_port: Option<u16>,
    acf: Option<access_security::AccessSecurityConfig>,
    /// Captured by `acf_file(path)` so the built server can later
    /// `reload_acf()` from the same source. None when the ACF was
    /// supplied in-memory via `acf(config)`.
    acf_path: Option<String>,
    /// Optional CA-over-TLS configuration. When set, accepted TCP
    /// connections are wrapped in a `tokio_rustls::server::TlsStream`
    /// before the CA handshake runs.
    #[cfg(feature = "experimental-rust-tls")]
    tls: Option<crate::tls::TlsConfig>,
    /// Optional mDNS instance name for service discovery. When set
    /// (and `discovery` feature is enabled), the server announces
    /// itself as `<instance>._epics-ca._tcp.local.` on the link-local
    /// segment.
    mdns_instance: Option<String>,
    /// Extra TXT key=value pairs attached to the mDNS announce.
    mdns_txt: Vec<(String, String)>,
    /// Optional RFC 2136 Dynamic DNS UPDATE registration.
    #[cfg(feature = "discovery-dns-update")]
    dns_update: Option<crate::discovery::DnsRegistration>,
    /// Optional audit logger. When set, security-relevant events
    /// (connect, caput, ACF deny, ...) land in the configured sink.
    audit: Option<crate::audit::AuditLogger>,
    /// Optional bind address for the HTTP introspection listener.
    introspection_addr: Option<std::net::SocketAddr>,
    /// Grace period (seconds) for graceful drain on signal or admin
    /// request.
    drain_grace_secs: u64,
    /// Optional capability-token verifier. When set, the CLIENT_NAME
    /// payload is treated as a `cap:<token>` and verified before its
    /// resolved subject is used for ACF matching. Unset = legacy
    /// "trust the username string" behaviour.
    #[cfg(feature = "cap-tokens")]
    cap_token_verifier: Option<Arc<crate::cap_token::TokenVerifier>>,
}

impl CaServerBuilder {
    pub fn new() -> Self {
        Self {
            ioc: ioc_builder::IocBuilder::new(),
            // SERVER-side port reader honours EPICS_CAS_SERVER_PORT >
            // EPICS_CA_SERVER_PORT > 5064 (caservertask.c:491-498).
            port: cas_server_port(),
            tcp_port: None,
            acf: None,
            acf_path: None,
            #[cfg(feature = "experimental-rust-tls")]
            tls: None,
            mdns_instance: None,
            mdns_txt: Vec::new(),
            #[cfg(feature = "discovery-dns-update")]
            dns_update: None,
            audit: audit_from_env(),
            introspection_addr: introspection_from_env(),
            drain_grace_secs: drain_grace_from_env(),
            #[cfg(feature = "cap-tokens")]
            cap_token_verifier: None,
        }
    }

    /// Bind an HTTP introspection endpoint exposing
    /// `/healthz`, `/info`, `/clients`, `/queues`. Plain JSON, no
    /// authentication — bind to `127.0.0.1:<port>` for IOC-local
    /// probes or to a private interface for facility tooling.
    pub fn with_introspection(mut self, addr: std::net::SocketAddr) -> Self {
        self.introspection_addr = Some(addr);
        self
    }

    /// Wire a structured audit log. Every connection lifecycle event
    /// and every `caput` lands as one JSON line in the supplied sink.
    /// Useful for compliance and forensic review; cost is one
    /// `Option::is_some()` check per event when omitted.
    pub fn audit(mut self, logger: crate::audit::AuditLogger) -> Self {
        self.audit = Some(logger);
        self
    }

    /// Announce this IOC via mDNS as
    /// `<instance>._epics-ca._tcp.local.`. Requires the `discovery`
    /// cargo feature; without it the call still compiles but emits a
    /// warning at startup and announces nothing.
    pub fn announce_mdns(mut self, instance: impl Into<String>) -> Self {
        self.mdns_instance = Some(instance.into());
        self
    }

    /// Attach a key=value pair to the mDNS announce TXT record.
    /// Useful for site-wide metadata: `version`, `asg`, `owner`.
    pub fn announce_txt(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.mdns_txt.push((key.into(), value.into()));
        self
    }

    /// Self-register with a unicast DNS server via RFC 2136 Dynamic
    /// DNS UPDATE. The server adds SRV/PTR/TXT records on startup,
    /// refreshes them periodically (`reg.keepalive`), and removes
    /// them on graceful shutdown. Requires the
    /// `discovery-dns-update` cargo feature.
    #[cfg(feature = "discovery-dns-update")]
    pub fn register_dns_update(mut self, reg: crate::discovery::DnsRegistration) -> Self {
        self.dns_update = Some(reg);
        self
    }

    /// Enable CA over TLS using the supplied server-side configuration.
    /// Built with the `tls` cargo feature.
    #[cfg(feature = "experimental-rust-tls")]
    pub fn with_tls(mut self, tls: crate::tls::TlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    // ── CA-specific methods ──────────────────────────────────────────

    /// Set the UDP discovery port (default: `EPICS_CA_SERVER_PORT` or 5064).
    ///
    /// When no [`Self::tcp_port`] override is provided, the TCP listener
    /// inherits this port — preserving the historical
    /// "one port for both" behaviour for callers that don't need a
    /// split-port deployment.
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Set the TCP listen port independently from the UDP search port.
    ///
    /// Mirrors epics-base PR #69 (`EPICS_CAS_SERVER_PORT`): multiple
    /// IOCs on one host can each bind a unique TCP port while every
    /// IOC keeps the canonical UDP search port (5064). The UDP search
    /// responder advertises this TCP port back in SEARCH_REPLY so
    /// clients connect to the correct listener.
    ///
    /// When unset, the TCP port is resolved at `build()` time from
    /// `EPICS_CAS_SERVER_PORT`; if that env var is also unset, the TCP
    /// port falls back to [`Self::port`].
    pub fn tcp_port(mut self, port: u16) -> Self {
        self.tcp_port = Some(port);
        self
    }

    /// Load an access security configuration file. The path is
    /// retained so `CaServer::reload_acf()` can later re-read it.
    pub fn acf_file(mut self, path: &str) -> CaResult<Self> {
        let content = std::fs::read_to_string(path).map_err(CaError::Io)?;
        self.acf = Some(access_security::parse_acf(&content)?);
        self.acf_path = Some(path.to_string());
        Ok(self)
    }

    /// Set access security configuration directly.
    pub fn acf(mut self, config: access_security::AccessSecurityConfig) -> Self {
        self.acf = Some(config);
        self
    }

    // ── IOC-delegated methods ────────────────────────────────────────

    /// Add a simple PV to be created on server start.
    pub fn pv(mut self, name: &str, initial: EpicsValue) -> Self {
        self.ioc = self.ioc.pv(name, initial);
        self
    }

    /// Add a record to be created on server start.
    pub fn record(mut self, name: &str, record: impl Record) -> Self {
        self.ioc = self.ioc.record(name, record);
        self
    }

    /// Add a pre-boxed record to be created on server start.
    pub fn record_boxed(mut self, name: &str, record: Box<dyn Record>) -> Self {
        self.ioc = self.ioc.record_boxed(name, record);
        self
    }

    /// Load records from a .db file.
    pub fn db_file(
        mut self,
        path: &str,
        macros: &std::collections::HashMap<String, String>,
    ) -> CaResult<Self> {
        self.ioc = self.ioc.db_file(path, macros)?;
        Ok(self)
    }

    /// Load records from a .db string.
    pub fn db_string(
        mut self,
        content: &str,
        macros: &std::collections::HashMap<String, String>,
    ) -> CaResult<Self> {
        self.ioc = self.ioc.db_string(content, macros)?;
        Ok(self)
    }

    /// Register a device support factory by DTYP name.
    pub fn register_device_support<F>(mut self, dtyp: &str, factory: F) -> Self
    where
        F: Fn() -> Box<dyn device_support::DeviceSupport> + Send + Sync + 'static,
    {
        self.ioc = self.ioc.register_device_support(dtyp, factory);
        self
    }

    /// Register an external record type factory.
    pub fn register_record_type<F>(mut self, type_name: &str, factory: F) -> Self
    where
        F: Fn() -> Box<dyn Record> + Send + Sync + 'static,
    {
        self.ioc = self.ioc.register_record_type(type_name, factory);
        self
    }

    /// Register a subroutine function by name (for sub/aSub records).
    ///
    /// The closure returns the C `long` status: `Ok(0)` for the normal
    /// path, `Ok(n)` with `n < 0` to raise `SOFT_ALARM` at `BRSV` (and, for
    /// `aSub`, publish the status as `VAL`). See `SubroutineFn`.
    pub fn register_subroutine<F>(mut self, name: &str, func: F) -> Self
    where
        F: Fn(&mut dyn Record) -> CaResult<i64> + Send + Sync + 'static,
    {
        self.ioc = self.ioc.register_subroutine(name, func);
        self
    }

    /// Configure autosave with a save set configuration.
    pub fn autosave(mut self, config: autosave::SaveSetConfig) -> Self {
        self.ioc = self.ioc.autosave(config);
        self
    }

    /// Build the server.
    ///
    /// The TCP listeners and UDP search-responder sockets are bound
    /// here, so a returned `CaServer` is already listening: a client
    /// may connect (or SEARCH) the instant `build()` returns, before
    /// [`CaServer::run`] has been spawned or polled. Callers therefore
    /// never need to sleep for a startup window — see
    /// [`CaServer::tcp_port`] for the port actually bound.
    pub async fn build(self) -> CaResult<CaServer> {
        let (db, autosave_config) = self.ioc.build().await?;
        let acf = epics_base_rs::server::access_security::new_acf_cell(self.acf);
        #[cfg(feature = "experimental-rust-tls")]
        let tls = self.tls.and_then(|t| match t {
            crate::tls::TlsConfig::Server(arc) => Some(Arc::new(std::sync::RwLock::new(arc))),
            crate::tls::TlsConfig::Client(_) => {
                tracing::warn!("client-side TlsConfig passed to CaServer; ignoring");
                None
            }
        });
        let (conn_tx, _) = tokio::sync::broadcast::channel(64);
        let (acf_reload_tx, _) = tokio::sync::broadcast::channel(16);
        // C parity (caservertask.c:491-499): `ca_udp_port =
        // ca_server_port` — UDP and TCP bind the same value unless
        // the Rust-extension `.tcp_port(...)` was used to split. The
        // `self.port` field already incorporates the
        // `EPICS_CAS_SERVER_PORT > EPICS_CA_SERVER_PORT` precedence
        // via cas_server_port() at builder construction.
        let tcp_port = self.tcp_port.unwrap_or(self.port);
        let (tcp, udp) = bind_sockets(self.port, tcp_port).await?;
        Ok(CaServer {
            db,
            port: udp.port(),
            tcp_port: tcp.port(),
            tcp,
            udp,
            stats: Arc::new(ServerStats::default()),
            acf,
            acf_source_path: std::sync::Mutex::new(self.acf_path),
            acf_reload_tx,
            autosave_config,
            autosave_manager: None,
            conn_events: Some(conn_tx),
            #[cfg(feature = "experimental-rust-tls")]
            tls,
            #[cfg(feature = "experimental-rust-tls")]
            tls_paths: std::sync::Mutex::new(tls_paths_from_env()),
            mdns_instance: self.mdns_instance,
            mdns_txt: self.mdns_txt,
            #[cfg(feature = "discovery-dns-update")]
            dns_update: self.dns_update,
            audit: self.audit,
            introspection_addr: self.introspection_addr,
            drain_grace_secs: self.drain_grace_secs,
            #[cfg(feature = "cap-tokens")]
            cap_token_verifier: self.cap_token_verifier,
            beacon_reset: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Install a capability-token verifier. When set, CLIENT_NAME
    /// payloads beginning with `cap:` are passed to
    /// [`crate::cap_token::TokenVerifier::verify`]; the resolved
    /// `sub` claim becomes the ACF-matched username. Unprefixed
    /// CLIENT_NAME values still pass through unchanged.
    #[cfg(feature = "cap-tokens")]
    pub fn with_cap_token_verifier(
        mut self,
        verifier: Arc<crate::cap_token::TokenVerifier>,
    ) -> Self {
        self.cap_token_verifier = Some(verifier);
        self
    }
}

/// A cloneable, detachable handle that triggers `CA_PROTO_ACCESS_RIGHTS`
/// re-evaluation for every connected client, equivalent to
/// [`CaServer::notify_access_change`] but usable after [`CaServer::run`]
/// has taken ownership of the server value.
///
/// Obtain one via [`CaServer::access_rights_notifier`]. It wraps a clone of
/// the server's ACF-reload broadcast sender; firing [`Self::notify`] prompts
/// each client's TCP task to re-run its per-channel access computation
/// (including any installed `PvDatabase` access hook) and re-push
/// `CA_PROTO_ACCESS_RIGHTS` only for channels whose computed level changed
/// (libca `oldaccess != access` filter, asLibRoutines.c:1047-1051).
#[derive(Clone)]
pub struct AccessRightsNotifier {
    tx: tokio::sync::broadcast::Sender<()>,
}

impl AccessRightsNotifier {
    /// Prompt every connected client to re-evaluate and re-push
    /// `CA_PROTO_ACCESS_RIGHTS` for its open channels. A send error (no live
    /// subscribers) is a normal transient state and is ignored.
    pub fn notify(&self) {
        let _ = self.tx.send(());
    }
}

/// Bind every socket the server serves on: the TCP listeners and the
/// UDP search responders.
///
/// The single place a `CaServer`'s sockets come into existence — both
/// construction paths ([`CaServerBuilder::build`] and
/// [`CaServer::from_parts`]) go through it, so "a `CaServer` exists"
/// implies "its ports are bound and listening". Binding is what makes
/// a client's `connect()` / SEARCH succeed; `run()` only services what
/// the kernel has already accepted or queued. There is consequently no
/// window between construction and `run()` in which a client is
/// refused, and no readiness sleep for a caller to guess at.
async fn bind_sockets(
    udp_port: u16,
    tcp_port: u16,
) -> CaResult<(crate::server::tcp::BoundTcp, crate::server::udp::BoundUdp)> {
    let tcp = crate::server::tcp::bind_tcp_listeners(tcp_port).await?;
    let cfg = addr_list::from_env()?;
    let udp = crate::server::udp::bind_udp_responders(udp_port, cfg.intf_addrs, &cfg.mcast_addrs)?;
    Ok((tcp, udp))
}

pub struct CaServer {
    db: Arc<PvDatabase>,
    /// UDP discovery port actually bound by the search responders in
    /// `udp` — the port clients SEARCH on. Equal to the configured port,
    /// or the kernel-assigned one when 0 was requested.
    port: u16,
    /// TCP port actually bound by the listeners in `tcp` — the value
    /// SEARCH replies and beacons advertise. Equal to the configured
    /// port unless it was split via [`CaServerBuilder::tcp_port`] /
    /// `EPICS_CAS_SERVER_PORT` (epics-base PR #69), or the configured
    /// port was in use and the ephemeral fallback fired.
    tcp_port: u16,
    /// TCP listeners, bound at construction. See [`tcp::BoundTcp`].
    tcp: crate::server::tcp::BoundTcp,
    /// UDP search responders, bound at construction. See
    /// [`udp::BoundUdp`].
    udp: crate::server::udp::BoundUdp,
    /// Shared stats counter — incremented by a task spawned in
    /// `start()` that subscribes to `conn_events`. Surfaced via
    /// [`Self::stats`] and the `casr` iocsh command.
    stats: Arc<ServerStats>,
    /// Active access security configuration. A lock-free snapshot cell so
    /// `reload_acf` can swap it without restarting the server: an access
    /// check takes an `Arc` of the policy and never blocks, and a reload
    /// publishes a new one without waiting for in-flight checks.
    acf: epics_base_rs::server::access_security::AcfCell,
    /// Path the ACF was originally loaded from, retained so the no-arg
    /// `reload_acf()` knows which file to re-read. None when the server
    /// was built via the in-memory `acf(config)` setter.
    acf_source_path: std::sync::Mutex<Option<String>>,
    /// Fan-out for ACF reload notifications. Each accepted TCP client
    /// subscribes; on `reload_acf*()` we send `()` so every active
    /// connection re-evaluates and re-pushes `CA_PROTO_ACCESS_RIGHTS`
    /// for its open channels. Mirrors RSRV `sendAllUpdateAS`
    /// (caservertask.c:1224) — the broadcast that keeps already-open
    /// channels in sync with rule changes. Fired by `reload_acf*()`
    /// after a config swap and by [`Self::notify_access_change`] for
    /// programmatic access-state changes the server cannot detect.
    acf_reload_tx: tokio::sync::broadcast::Sender<()>,
    autosave_config: Option<autosave::SaveSetConfig>,
    autosave_manager: Option<Arc<autosave::AutosaveManager>>,
    /// Optional broadcast channel for connection lifecycle events.
    /// Subscribers (e.g. ca-gateway) get one event per accept/disconnect.
    conn_events: Option<tokio::sync::broadcast::Sender<crate::server::tcp::ServerConnectionEvent>>,
    /// Optional TLS configuration. When set, accepted TCP connections
    /// are wrapped in a `tokio_rustls::server::TlsStream` before the
    /// CA handshake runs. mTLS configurations additionally extract a
    /// verified peer identity for ACF rule matching.
    ///
    /// Wrapped in `RwLock<Arc<...>>` (rather than just `Arc<...>`) so
    /// `reload_tls()` can swap the active config in place — accepted
    /// connections see the new config without restarting the listener.
    #[cfg(feature = "experimental-rust-tls")]
    tls: Option<Arc<std::sync::RwLock<Arc<tokio_rustls::rustls::ServerConfig>>>>,
    /// Retained cert/key paths so `reload_tls()` knows what to re-read.
    /// None when TLS was supplied via `with_tls(config)` rather than
    /// path-based env config.
    #[cfg(feature = "experimental-rust-tls")]
    tls_paths: std::sync::Mutex<Option<TlsPaths>>,
    /// mDNS instance name to announce as. None disables announce.
    mdns_instance: Option<String>,
    /// Extra TXT key=value pairs for the mDNS announce.
    #[cfg_attr(not(feature = "discovery"), allow(dead_code))]
    mdns_txt: Vec<(String, String)>,
    /// RFC 2136 dynamic DNS UPDATE registration. None disables it.
    #[cfg(feature = "discovery-dns-update")]
    dns_update: Option<crate::discovery::DnsRegistration>,
    /// Optional structured audit logger.
    audit: Option<crate::audit::AuditLogger>,
    /// Optional HTTP introspection bind address.
    introspection_addr: Option<std::net::SocketAddr>,
    /// Grace period in seconds applied when drain is requested.
    /// Default 30 s; configurable via EPICS_CAS_DRAIN_GRACE_SECS.
    /// Only consumed by the Unix SIGTERM handler — kept on the
    /// struct for both cfgs so the constructor signature stays
    /// stable across platforms.
    #[cfg_attr(not(unix), allow(dead_code))]
    drain_grace_secs: u64,
    /// Optional capability-token verifier; threaded into per-client
    /// state at accept time so CLIENT_NAME `cap:<token>` payloads
    /// resolve to a verified subject before ACF lookup.
    #[cfg(feature = "cap-tokens")]
    cap_token_verifier: Option<Arc<crate::cap_token::TokenVerifier>>,
    /// Pulse-able beacon-reset signal. The beacon emitter task awaits
    /// this Notify alongside its periodic timer; firing it interrupts
    /// the next scheduled period and emits a beacon immediately
    /// (mirrors RSRV's `generateBeaconAnomaly`). Held on the struct
    /// (rather than constructed locally in `run()`) so external code
    /// — most importantly the bridge ca_gateway when it discovers a
    /// new upstream PV — can trigger an immediate beacon by calling
    /// [`Self::trigger_beacon_anomaly`].
    beacon_reset: Arc<tokio::sync::Notify>,
}

impl CaServer {
    /// Create a builder for configuring the server.
    pub fn builder() -> CaServerBuilder {
        CaServerBuilder::new()
    }

    /// Pulse the beacon emitter so it sends a beacon immediately
    /// (interrupting the periodic timer) — mirrors RSRV's
    /// `generateBeaconAnomaly`. Used by the bridge ca_gateway when a
    /// new upstream PV is registered so other gateway-aware clients
    /// re-search and pick the gateway as the source for that PV.
    pub fn trigger_beacon_anomaly(&self) {
        self.beacon_reset.notify_one();
    }

    /// Clone of the beacon-reset signal. Lets external coordinators
    /// (e.g. ca-gateway) hold a long-lived handle without a back-ref
    /// to the CaServer itself — important because `run()` consumes
    /// the server, after which there's no `&CaServer` to call
    /// `trigger_beacon_anomaly` on.
    pub fn beacon_anomaly_handle(&self) -> Arc<tokio::sync::Notify> {
        self.beacon_reset.clone()
    }

    /// Construct a CaServer from pre-populated parts.
    /// Used by [`epics_base_rs::server::ioc_app::IocApplication`] after st.cmd execution and
    /// device support wiring. `tcp_port` carries the optional split-port
    /// TCP override (`EPICS_CAS_SERVER_PORT`); pass `None` to share the
    /// UDP discovery port with the TCP listener.
    ///
    /// Binds the TCP and UDP sockets, exactly as
    /// [`CaServerBuilder::build`] does — the returned server is already
    /// listening, and a bind failure is reported here rather than from
    /// inside a spawned [`Self::run`] task.
    pub async fn from_parts(
        db: Arc<PvDatabase>,
        port: u16,
        tcp_port: Option<u16>,
        acf: Option<access_security::AccessSecurityConfig>,
        autosave_config: Option<autosave::SaveSetConfig>,
        autosave_manager: Option<Arc<autosave::AutosaveManager>>,
    ) -> CaResult<Self> {
        let stats = Arc::new(ServerStats::default());
        // Always-on connection broadcast so the stats counter task in
        // `run()` (and any external `connection_events()` subscriber)
        // can attach without requiring a `&mut self` mutation. Capacity
        // 64 matches the previous lazy default.
        let (conn_tx, _) = tokio::sync::broadcast::channel(64);
        let (acf_reload_tx, _) = tokio::sync::broadcast::channel(16);
        let tcp_port = tcp_port.unwrap_or(port);
        let (tcp, udp) = bind_sockets(port, tcp_port).await?;
        Ok(Self {
            db,
            port: udp.port(),
            tcp_port: tcp.port(),
            tcp,
            udp,
            stats,
            acf: epics_base_rs::server::access_security::new_acf_cell(acf),
            acf_source_path: std::sync::Mutex::new(None),
            acf_reload_tx,
            autosave_config,
            autosave_manager,
            conn_events: Some(conn_tx),
            #[cfg(feature = "experimental-rust-tls")]
            tls: None,
            #[cfg(feature = "experimental-rust-tls")]
            tls_paths: std::sync::Mutex::new(tls_paths_from_env()),
            mdns_instance: None,
            mdns_txt: Vec::new(),
            #[cfg(feature = "discovery-dns-update")]
            dns_update: None,
            audit: audit_from_env(),
            introspection_addr: introspection_from_env(),
            drain_grace_secs: drain_grace_from_env(),
            #[cfg(feature = "cap-tokens")]
            cap_token_verifier: None,
            beacon_reset: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// The TCP port the server is listening on.
    ///
    /// Known from construction (the listener is already bound), so a
    /// caller that passed port 0 — or whose configured port was taken
    /// and hit the ephemeral fallback — can read the real port without
    /// waiting for [`Self::run`].
    pub fn tcp_port(&self) -> u16 {
        self.tcp_port
    }

    /// The UDP port the search responders are bound to.
    pub fn udp_port(&self) -> u16 {
        self.port
    }

    /// Re-read the ACF file the server was originally configured with
    /// and atomically swap in the new configuration. The new rules take
    /// effect on the next access check (CREATE_CHAN, HOST_NAME, or
    /// CLIENT_NAME message); already-allocated channel access bits stay
    /// in place until re-evaluated.
    ///
    /// Errors when no source path is registered. Use `reload_acf_from`
    /// with an explicit path when the server was constructed via
    /// `acf(config)` rather than `acf_file(path)`.
    pub async fn reload_acf(&self) -> CaResult<()> {
        let path = self
            .acf_source_path
            .lock()
            .map_err(|_| CaError::InvalidValue("acf_source_path lock poisoned".into()))?
            .clone();
        match path {
            Some(p) => self.reload_acf_from(&p).await,
            None => Err(CaError::InvalidValue(
                "no ACF source path registered; use reload_acf_from() with an explicit path".into(),
            )),
        }
    }

    /// Re-read ACF from an arbitrary path. Use this when the source has
    /// moved or when the server was originally configured in-memory.
    pub async fn reload_acf_from(&self, path: &str) -> CaResult<()> {
        Self::reload_acf_inner(path, &self.acf, &self.acf_reload_tx).await?;
        if let Ok(mut p) = self.acf_source_path.lock() {
            *p = Some(path.to_string());
        }
        Ok(())
    }

    /// Shared implementation of `reload_acf_from` factored so the
    /// HAG-DNS-refresh task in `run()` can reload via cloned handles
    /// without holding the full `&self` borrow.
    pub(crate) async fn reload_acf_inner(
        path: &str,
        acf: &epics_base_rs::server::access_security::AcfCell,
        reload_tx: &tokio::sync::broadcast::Sender<()>,
    ) -> CaResult<()> {
        // std::fs::read_to_string blocks the worker thread on slow
        // NFS / FUSE / network FS. Run it on the blocking pool so
        // concurrent CA TCP traffic on the same worker doesn't stall
        // for the duration of the read.
        let path_owned = path.to_string();
        let content = tokio::task::spawn_blocking(move || std::fs::read_to_string(path_owned))
            .await
            .map_err(|e| CaError::Io(std::io::Error::other(e)))?
            .map_err(CaError::Io)?;
        let parsed = access_security::parse_acf(&content)?;
        acf.store(Some(Arc::new(parsed)));
        // Notify every active TCP client to recompute and push fresh
        // CA_PROTO_ACCESS_RIGHTS for its open channels. Send-error
        // (no live subscribers) is a normal transient state.
        let notified = reload_tx.send(()).unwrap_or(0);
        tracing::info!(
            path = %path,
            clients = notified,
            "ACF reloaded; pushed access-rights refresh"
        );
        metrics::counter!("ca_server_acf_reloads_total").increment(1);
        Ok(())
    }

    /// Returns the path the ACF was loaded from, if any.
    pub fn acf_source_path(&self) -> Option<String> {
        self.acf_source_path.lock().ok().and_then(|g| g.clone())
    }

    /// Trigger `CA_PROTO_ACCESS_RIGHTS` re-notification for all connected
    /// clients without touching the ACF configuration. Equivalent to C
    /// `asComputeAllAsg()` (asCa.c:205) — prompts every active TCP
    /// connection to run `reeval_access_rights`, which re-pushes
    /// `CA_PROTO_ACCESS_RIGHTS` only when the computed level changed
    /// (`oldaccess != access` filter, libca parity).
    ///
    /// Use this after programmatic access-security state changes the
    /// server cannot detect automatically — for example, when INP* link
    /// values used by CALC-gated ACF rules change. For ACF-file changes
    /// prefer [`Self::reload_acf_from`], which swaps the config and
    /// notifies in one step.
    pub fn notify_access_change(&self) {
        self.access_rights_notifier().notify();
    }

    /// Snapshot a cloneable, detachable [`AccessRightsNotifier`] that fires
    /// the same access-rights re-evaluation as [`Self::notify_access_change`].
    ///
    /// Unlike `&self`-based `notify_access_change`, the returned handle keeps
    /// working after [`Self::run`] has consumed the server: it holds a clone
    /// of the `acf_reload_tx` sender, and the matching receivers live inside
    /// each connected client's TCP task. A caller (e.g. the CA gateway, whose
    /// upstream manager outlives the `CaServer` value once `run` is spawned)
    /// snapshots this before `run` and fires it whenever a programmatic
    /// access-state change — such as an upstream IOC write-access flip or an
    /// `.acf`/`.pvlist` reload — should re-push `CA_PROTO_ACCESS_RIGHTS` to
    /// already-connected clients (RSRV `sendAllUpdateAS`, caservertask.c:1224).
    pub fn access_rights_notifier(&self) -> AccessRightsNotifier {
        AccessRightsNotifier {
            tx: self.acf_reload_tx.clone(),
        }
    }

    /// Install a TLS server config on a CaServer that was constructed
    /// via [`Self::from_parts`] (which can't accept a TLS config
    /// directly — `from_parts` is shared with non-TLS builds).
    /// Idempotent; replaces any previously set config.
    #[cfg(feature = "experimental-rust-tls")]
    pub fn set_tls(&mut self, tls: Arc<tokio_rustls::rustls::ServerConfig>) {
        self.tls = Some(Arc::new(std::sync::RwLock::new(tls)));
    }

    /// Record the cert/key/client-CA paths for later `reload_tls()`.
    /// Builders that load via env (`tls_paths_from_env`) populate this
    /// automatically; call this only when overriding programmatically.
    #[cfg(feature = "experimental-rust-tls")]
    pub fn set_tls_paths(&self, paths: TlsPaths) {
        if let Ok(mut g) = self.tls_paths.lock() {
            *g = Some(paths);
        }
    }

    /// Re-read the cert/key files registered via env or
    /// `set_tls_paths`, build a fresh `ServerConfig`, and atomically
    /// swap it in. New TCP accepts use the fresh config immediately;
    /// already-handshaked connections keep their negotiated session
    /// until they close. The most common use is rotating certs
    /// before expiry without restarting the IOC.
    ///
    /// Errors if no `tls_paths` is registered or the new files don't
    /// load. The active config is left untouched on error.
    #[cfg(feature = "experimental-rust-tls")]
    pub fn reload_tls(&self) -> Result<(), String> {
        let paths = {
            let g = self.tls_paths.lock().map_err(|e| e.to_string())?;
            g.clone()
        };
        let paths = paths.ok_or_else(|| "no TLS source paths registered".to_string())?;
        let chain = crate::tls::load_certs(&paths.cert)
            .map_err(|e| format!("loading {}: {e}", paths.cert))?;
        let key = crate::tls::load_private_key(&paths.key)
            .map_err(|e| format!("loading {}: {e}", paths.key))?;
        let cfg = match paths.client_ca.as_ref() {
            Some(ca) => {
                let roots = crate::tls::load_root_store(ca)
                    .map_err(|e| format!("loading client CA {ca}: {e}"))?;
                crate::tls::TlsConfig::server_mtls_from_pem(chain, key, roots)
                    .map_err(|e| format!("mTLS server build: {e}"))?
            }
            None => crate::tls::TlsConfig::server_from_pem(chain, key)
                .map_err(|e| format!("TLS server build: {e}"))?,
        };
        let new_arc = match cfg {
            crate::tls::TlsConfig::Server(arc) => arc,
            crate::tls::TlsConfig::Client(_) => {
                return Err("expected server TlsConfig".into());
            }
        };
        let slot = self
            .tls
            .as_ref()
            .ok_or_else(|| "TLS was never enabled on this server".to_string())?;
        match slot.write() {
            Ok(mut w) => {
                *w = new_arc;
                metrics::counter!("ca_server_tls_reload_total").increment(1);
                Ok(())
            }
            Err(e) => Err(format!("tls slot poisoned: {e}")),
        }
    }

    /// Subscribe to connection lifecycle events. Returns a broadcast
    /// receiver that receives [`crate::server::tcp::ServerConnectionEvent::Connected`] /
    /// `Disconnected` for each accepted client.
    ///
    /// Idempotent: calling multiple times shares the same broadcast sender.
    pub fn connection_events(
        &mut self,
    ) -> tokio::sync::broadcast::Receiver<crate::server::tcp::ServerConnectionEvent> {
        match &self.conn_events {
            Some(tx) => tx.subscribe(),
            None => {
                let (tx, rx) = tokio::sync::broadcast::channel(64);
                self.conn_events = Some(tx);
                rx
            }
        }
    }

    /// Expose PV database for shell/external use.
    pub fn database(&self) -> &Arc<PvDatabase> {
        &self.db
    }

    /// Live connect/disconnect counters + uptime. Backs the `casr`
    /// iocsh command. The counters update once `start()` has spawned
    /// the connection-event subscriber; before that they read zero.
    pub fn stats(&self) -> Arc<ServerStats> {
        self.stats.clone()
    }

    /// Run server + interactive shell. Shell exit stops server.
    pub async fn run_with_shell<F>(self, register_fn: F) -> CaResult<()>
    where
        F: FnOnce(&iocsh::IocShell) + Send + 'static,
    {
        let db = self.db.clone();
        let handle = tokio::runtime::Handle::current();

        let autosave_cmds = self
            .autosave_manager
            .as_ref()
            .map(|mgr| autosave::iocsh::autosave_commands(mgr.clone()));

        let server = Arc::new(self);

        let server_clone = server.clone();
        let server_handle =
            epics_base_rs::runtime::task::spawn(async move { server_clone.run().await });

        let (tx, rx) = epics_base_rs::runtime::sync::oneshot::channel();
        std::thread::spawn(move || {
            let shell = iocsh::IocShell::new(db, handle);
            register_fn(&shell);
            if let Some(cmds) = autosave_cmds {
                for cmd in cmds {
                    shell.register(cmd);
                }
            }
            let result = shell.run_repl();
            let _ = tx.send(result);
        });

        let shell_result = rx.await;

        server_handle.abort();
        let _ = server_handle.await;

        match shell_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                eprintln!("shell error: {e}");
                Err(CaError::InvalidValue(e))
            }
            Err(_) => {
                eprintln!("shell thread dropped unexpectedly");
                Err(CaError::InvalidValue("shell thread dropped".to_string()))
            }
        }
    }

    /// Add a simple PV at runtime. Returns `Err` when the name is
    /// already registered as a simple PV, record, or alias.
    pub async fn add_pv(&self, name: &str, initial: EpicsValue) -> CaResult<()> {
        self.db.add_pv(name, initial).await
    }

    /// Add a record at runtime. Returns `Err` on duplicate name.
    pub async fn add_record(&self, name: &str, record: impl Record) -> CaResult<()> {
        self.db.add_record(name, Box::new(record)).await
    }

    /// Set a PV value (notifies subscribers).
    pub async fn put(&self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.db.put_pv(name, value).await
    }

    /// Get a PV value.
    pub async fn get(&self, name: &str) -> CaResult<EpicsValue> {
        self.db.get_pv(name)
    }

    /// Run the server (UDP + TCP + beacon + scan scheduler).
    /// This function runs indefinitely.
    pub async fn run(&self) -> CaResult<()> {
        // Pin the started_at timestamp on first run() so subsequent
        // re-entries don't reset uptime accounting.
        let _ = self.stats.started_at.set(std::time::Instant::now());

        // Spawn the connect/disconnect counter that backs `casr`.
        // Subscribes to the always-on `conn_events` broadcast set up in
        // the constructors. The receiver lives for as long as the
        // server runs; on shutdown the broadcast sender drops and the
        // task exits naturally.
        if let Some(tx) = &self.conn_events {
            let mut rx = tx.subscribe();
            let stats_for_task = self.stats.clone();
            tokio::spawn(async move {
                while let Ok(evt) = rx.recv().await {
                    use std::sync::atomic::Ordering::Relaxed;
                    // ServerConnectionEvent is `#[non_exhaustive]`, so
                    // the `_` arm guards against future variants — even
                    // though every present variant is matched today,
                    // we don't want a new event in some future minor
                    // release to break this crate. Clippy sees today's
                    // exhaustive cover and warns; allow it explicitly.
                    #[allow(unreachable_patterns)]
                    match evt {
                        crate::server::tcp::ServerConnectionEvent::Connected(_) => {
                            stats_for_task.connects_total.fetch_add(1, Relaxed);
                        }
                        crate::server::tcp::ServerConnectionEvent::Disconnected(_) => {
                            stats_for_task.disconnects_total.fetch_add(1, Relaxed);
                        }
                        crate::server::tcp::ServerConnectionEvent::ChannelCreated { .. } => {
                            stats_for_task.channels_opened_total.fetch_add(1, Relaxed);
                        }
                        crate::server::tcp::ServerConnectionEvent::ChannelCleared { .. } => {
                            stats_for_task.channels_closed_total.fetch_add(1, Relaxed);
                        }
                        crate::server::tcp::ServerConnectionEvent::SubscriptionOpened {
                            ..
                        } => {
                            stats_for_task
                                .subscriptions_opened_total
                                .fetch_add(1, Relaxed);
                        }
                        crate::server::tcp::ServerConnectionEvent::SubscriptionClosed {
                            ..
                        } => {
                            stats_for_task
                                .subscriptions_closed_total
                                .fetch_add(1, Relaxed);
                        }
                        _ => {}
                    }
                }
            });
        }

        let db_udp = self.db.clone();
        let db_tcp = self.db.clone();
        let acf = self.acf.clone();
        let port = self.port;
        // Sockets were bound at construction (`bind_sockets`), so the
        // TCP port is already final — no start-up handshake back from
        // the listener task, and nothing downstream (beacon, SEARCH
        // replies, mDNS, introspection) has to wait to learn it.
        let tcp_port = self.tcp_port;
        let bound_tcp = self.tcp.clone();
        let bound_udp = self.udp.clone();

        // NOTE: no scan scheduler here. Scanning (and the PINI=YES pass)
        // is owned by the IOC core — `epics_base_rs::server::scan::
        // ScanOwner`, started by `IocApplication::run` at the C `scanRun`
        // point or by the IOC entry binary — never by a protocol server.
        // The "PINI before after-init hooks" ordering the scheduler arm
        // used to provide lives in `IocApplication::run` (Phase 2b.6 runs
        // PINI, H3 drains the hooks after it, both before this server can
        // accept a client).

        // Spawn autosave: prefer existing manager, otherwise build one from SaveSetConfig
        let autosave_handle = if let Some(ref mgr) = self.autosave_manager {
            let mgr = mgr.clone();
            let db_save = self.db.clone();
            Some(mgr.start(db_save))
        } else if let Some(ref cfg) = self.autosave_config {
            let builder = autosave::AutosaveBuilder::new().add_set(cfg.clone());
            match builder.build().await {
                Ok(mgr) => {
                    let mgr = Arc::new(mgr);
                    let db_save = self.db.clone();
                    Some(mgr.start(db_save))
                }
                Err(e) => {
                    eprintln!("autosave: failed to start: {e}");
                    None
                }
            }
        } else {
            None
        };

        // Use the externally-pulse-able handle held on the struct.
        // Bridge ca_gateway captures `beacon_anomaly_handle()` BEFORE
        // calling run() (which consumes self) and pulses it on
        // upstream PV discovery to fire a beacon immediately.
        let beacon_reset = self.beacon_reset.clone();

        let conn_events = self.conn_events.clone();
        let acf_reload_tx = self.acf_reload_tx.clone();
        #[cfg(feature = "experimental-rust-tls")]
        let tls = match self.tls.clone() {
            Some(slot) => Some(slot),
            None => match crate::tls::server_from_env() {
                Ok(Some(crate::tls::TlsConfig::Server(arc))) => {
                    Some(Arc::new(std::sync::RwLock::new(arc)))
                }
                Ok(Some(crate::tls::TlsConfig::Client(_))) => {
                    tracing::warn!("client-side TlsConfig produced by server_from_env; ignoring");
                    None
                }
                Ok(None) => None,
                Err(e) => {
                    tracing::error!(error = %e,
                        "EPICS_CAS_TLS_* configuration is invalid; starting in plaintext mode");
                    None
                }
            },
        };
        #[cfg(feature = "experimental-rust-tls")]
        if tls.is_some() {
            tracing::warn!(
                "═══════════════════════════════════════════════════════════════════════\n  \
                 CA-over-TLS ENABLED — non-standard, Rust-only extension.\n  \
                 C tools (caget/caput/camonitor/EDM/MEDM/CSS) and pyepics CANNOT connect.\n  \
                 For interoperable encryption use network-layer (IPSec/WireGuard/VPN).\n  \
                 See doc/11-tls-design.md for rationale.\n  \
                 ═══════════════════════════════════════════════════════════════════════"
            );
            metrics::counter!("ca_server_tls_enabled_total").increment(1);
        }
        let audit_for_tcp = self.audit.clone();
        // Drain coordination — shared between the TCP listener
        // (checks before accept) and the introspection /drain admin
        // route (sets when triggered).
        let drain = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let drain_for_tcp = drain.clone();
        #[cfg(feature = "cap-tokens")]
        let cap_token_verifier_for_tcp = self.cap_token_verifier.clone();
        let stats_for_tcp = Some(self.stats.clone());
        let tcp_handle = epics_base_rs::runtime::task::spawn(async move {
            #[cfg(feature = "experimental-rust-tls")]
            {
                tcp::run_tcp_listener(
                    db_tcp,
                    bound_tcp,
                    acf,
                    acf_reload_tx,
                    conn_events,
                    audit_for_tcp,
                    drain_for_tcp,
                    stats_for_tcp,
                    tls,
                    #[cfg(feature = "cap-tokens")]
                    cap_token_verifier_for_tcp,
                )
                .await
            }
            #[cfg(not(feature = "experimental-rust-tls"))]
            {
                tcp::run_tcp_listener(
                    db_tcp,
                    bound_tcp,
                    acf,
                    acf_reload_tx,
                    conn_events,
                    audit_for_tcp,
                    drain_for_tcp,
                    stats_for_tcp,
                    #[cfg(feature = "cap-tokens")]
                    cap_token_verifier_for_tcp,
                )
                .await
            }
        });

        // epics-base PR #862/#863 (DNS TTL refresh of HAG): when the
        // operator sets `EPICS_RS_HAG_DNS_REFRESH_SECS=N`, periodically
        // re-read the registered ACF source path and re-resolve every
        // HAG hostname → IP set. This catches cases where a hostname's
        // DNS A record changed (cluster failover, DHCP host renewal)
        // without an operator-driven `/reload-acf`. N=0 (default) keeps
        // the historic on-demand-only behaviour. The task is silently
        // skipped when no ACF source path is registered (in-memory
        // config has no file to re-read).
        //
        // We clone the small set of fields the task needs (acf,
        // acf_reload_tx, path string) instead of cloning the whole
        // `&self` borrow — `run` takes `&self` so no Arc<Self> is
        // available, but the inner Arc-shared state already implements
        // the necessary handle semantics.
        let _hag_refresh_handle = {
            let secs = epics_base_rs::runtime::env::get("EPICS_RS_HAG_DNS_REFRESH_SECS")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let path = self.acf_source_path();
            if let (true, Some(path)) = (secs > 0, path) {
                let acf = self.acf.clone();
                let reload_tx = self.acf_reload_tx.clone();
                Some(epics_base_rs::runtime::task::spawn(async move {
                    let mut tick = tokio::time::interval(std::time::Duration::from_secs(secs));
                    tick.tick().await; // skip immediate fire
                    loop {
                        tick.tick().await;
                        match Self::reload_acf_inner(&path, &acf, &reload_tx).await {
                            Ok(()) => tracing::trace!(
                                target: "epics_ca_rs::server",
                                "HAG DNS refresh tick: ACF re-read + re-resolved"
                            ),
                            Err(e) => tracing::debug!(
                                target: "epics_ca_rs::server",
                                error = %e,
                                "HAG DNS refresh: ACF reload failed (non-fatal)"
                            ),
                        }
                    }
                }))
            } else {
                None
            }
        };

        // Signal-driven drain: SIGTERM (and SIGINT on unix) flips the
        // drain flag. The accept loop will exit; existing connections
        // continue until the grace period elapses, after which run()
        // returns and the rest of the spawned tasks are aborted.
        #[cfg(unix)]
        let signal_handle = {
            let drain = drain.clone();
            let grace = self.drain_grace_secs;
            Some(epics_base_rs::runtime::task::spawn(async move {
                let mut sigterm =
                    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(error = %e, "drain: cannot install SIGTERM handler");
                            return;
                        }
                    };
                if sigterm.recv().await.is_some() {
                    tracing::info!(grace_secs = grace, "SIGTERM received; entering drain mode");
                    drain.store(true, std::sync::atomic::Ordering::Release);
                    metrics::counter!("ca_server_drain_total").increment(1);
                    tokio::time::sleep(std::time::Duration::from_secs(grace)).await;
                    tracing::info!("drain grace expired; exiting");
                    std::process::exit(0);
                }
            }))
        };
        #[cfg(not(unix))]
        let signal_handle: Option<tokio::task::JoinHandle<()>> = None;
        let tcp_abort = tcp_handle.abort_handle();

        let udp_cfg = addr_list::from_env()?;
        eprintln!(
            "CA server: UDP search on port {port}, TCP on port {tcp_port}, beacons → {} address(es)",
            udp_cfg.beacon_addrs.len()
        );

        // mDNS announce: held for the lifetime of run(). Drops when
        // the function returns, deregistering us from the network.
        #[cfg(feature = "discovery")]
        let _mdns = if let Some(ref instance) = self.mdns_instance {
            match crate::discovery::MdnsBackend::announce_helper(
                instance,
                tcp_port,
                self.mdns_txt.clone(),
            ) {
                Ok(announcer) => {
                    tracing::info!(instance = %instance, port = tcp_port,
                        "mDNS announce active");
                    metrics::counter!("ca_server_mdns_announces_total").increment(1);
                    Some(announcer)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "mDNS announce failed; continuing without it");
                    None
                }
            }
        } else {
            None
        };
        #[cfg(not(feature = "discovery"))]
        if self.mdns_instance.is_some() {
            tracing::warn!(
                "mDNS announce requested via .announce_mdns() but built without `discovery` \
                 cargo feature; ignoring"
            );
        }

        // RFC 2136 dynamic DNS UPDATE — held for run() lifetime.
        // Drop sends DELETE updates to clean up the records.
        #[cfg(feature = "discovery-dns-update")]
        let _dns_updater = if let Some(ref reg) = self.dns_update {
            // The configured port may differ from the actual listening
            // port (e.g. when binding 0 for ephemeral). Patch the
            // registration with `tcp_port` before sending.
            let mut reg = reg.clone();
            reg.port = tcp_port;
            match crate::discovery::DnsUpdater::register(reg).await {
                Ok(updater) => {
                    tracing::info!("RFC 2136 dynamic DNS registration active");
                    Some(updater)
                }
                Err(e) => {
                    tracing::warn!(error = %e,
                        "RFC 2136 dynamic DNS registration failed; continuing");
                    None
                }
            }
        } else {
            None
        };
        #[cfg(not(feature = "discovery-dns-update"))]
        {
            // No-op when feature is off.
        }

        // Optional HTTP introspection endpoint. Bound on the address
        // configured via `with_introspection()` or
        // EPICS_CAS_INTROSPECTION_ADDR. Failures are logged and the CA
        // server keeps running — introspection is non-essential.
        let introspection_handle = if let Some(addr) = self.introspection_addr {
            let state = crate::server::introspection::IntrospectionState::new(tcp_port);
            // Share the drain flag so POST /drain triggers the same
            // graceful-shutdown path as SIGTERM.
            let state = state.with_drain(drain.clone());
            // Wire POST /reload-acf to the same machinery the
            // built-in reload uses.
            let acf_clone = self.acf.clone();
            let acf_path_clone = self.acf_source_path.lock().ok().and_then(|g| g.clone());
            let acf_reload_tx_clone = self.acf_reload_tx.clone();
            let reload_fn: Arc<dyn Fn() -> Result<(), String> + Send + Sync> =
                Arc::new(move || -> Result<(), String> {
                    let path = acf_path_clone
                        .as_ref()
                        .ok_or("no ACF source path registered")?;
                    let content =
                        std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
                    let cfg = access_security::parse_acf(&content)
                        .map_err(|e| format!("parse {path}: {e}"))?;
                    // Avoid awaiting inside the closure — spawn a one-shot
                    // task to swap the RwLock contents and notify clients.
                    let acf = acf_clone.clone();
                    let reload_tx = acf_reload_tx_clone.clone();
                    tokio::spawn(async move {
                        acf.store(Some(Arc::new(cfg)));
                        let _ = reload_tx.send(());
                    });
                    Ok(())
                });
            let state = state.with_reload_acf(reload_fn);

            // POST /reload-tls hook: re-read the cert/key paths and
            // swap the inner ServerConfig Arc atomically. Available
            // only when the server has TLS enabled and source paths.
            #[cfg(feature = "experimental-rust-tls")]
            let state = if let (Some(slot), Some(paths)) = (
                self.tls.clone(),
                self.tls_paths.lock().ok().and_then(|g| g.clone()),
            ) {
                let paths = std::sync::Arc::new(paths);
                let reload_tls_fn: Arc<dyn Fn() -> Result<(), String> + Send + Sync> =
                    Arc::new(move || -> Result<(), String> {
                        let chain = crate::tls::load_certs(&paths.cert)
                            .map_err(|e| format!("loading {}: {e}", paths.cert))?;
                        let key = crate::tls::load_private_key(&paths.key)
                            .map_err(|e| format!("loading {}: {e}", paths.key))?;
                        let cfg = match paths.client_ca.as_ref() {
                            Some(ca) => {
                                let roots = crate::tls::load_root_store(ca)
                                    .map_err(|e| format!("loading {ca}: {e}"))?;
                                crate::tls::TlsConfig::server_mtls_from_pem(chain, key, roots)
                                    .map_err(|e| format!("mTLS build: {e}"))?
                            }
                            None => crate::tls::TlsConfig::server_from_pem(chain, key)
                                .map_err(|e| format!("TLS build: {e}"))?,
                        };
                        let new_arc = match cfg {
                            crate::tls::TlsConfig::Server(arc) => arc,
                            crate::tls::TlsConfig::Client(_) => {
                                return Err("expected server TlsConfig".into());
                            }
                        };
                        let mut w = slot
                            .write()
                            .map_err(|e| format!("tls slot poisoned: {e}"))?;
                        *w = new_arc;
                        metrics::counter!("ca_server_tls_reload_total").increment(1);
                        Ok(())
                    });
                state.with_reload_tls(reload_tls_fn)
            } else {
                state
            };

            let st = state.clone();
            Some(epics_base_rs::runtime::task::spawn(async move {
                if let Err(e) = crate::server::introspection::run_introspection(addr, st).await {
                    tracing::warn!(error = %e, "introspection HTTP exited");
                }
            }))
        } else {
            None
        };

        // Spawn UDP responder as its own task so its waker isn't multiplexed
        // through a select! branch (which can drop/replace wakers between polls
        // and miss edge-triggered epoll events).
        let ignore_addrs = udp_cfg.ignore_addrs.clone();
        let udp_handle = epics_base_rs::runtime::task::spawn(async move {
            udp::run_udp_search_responder(db_udp, bound_udp, tcp_port, ignore_addrs).await
        });
        let udp_abort = udp_handle.abort_handle();

        let result = tokio::select! {
            r = udp_handle => {
                eprintln!("UDP responder exited: {r:?}");
                match r {
                    Ok(inner) => inner,
                    Err(e) => Err(CaError::Io(
                        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                    )),
                }
            }
            r = tcp_handle => {
                eprintln!("TCP listener exited: {r:?}");
                match r {
                    Ok(inner) => inner,
                    Err(e) => Err(CaError::Io(
                        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                    )),
                }
            }
            r = beacon::run_beacon_emitter(
                tcp_port,
                udp_cfg.beacon_addrs.clone(),
                udp_cfg.beacon_period,
                beacon_reset,
                #[cfg(feature = "cap-tokens")]
                None,
            ) => {
                eprintln!("Beacon emitter exited: {r:?}");
                r
            }
        };

        // Tear down spawned tasks whose JoinHandles were moved into the
        // select!. Calling abort() on a handle whose task already finished
        // is a no-op, so it's safe to call unconditionally.
        udp_abort.abort();
        tcp_abort.abort();
        if let Some(h) = autosave_handle {
            h.abort();
        }
        if let Some(h) = introspection_handle {
            h.abort();
        }
        if let Some(h) = signal_handle {
            h.abort();
        }
        result
    }
}

/// Cert / key / optional-client-CA paths retained on the server so
/// `reload_tls()` can re-read them. Used internally; populated from
/// the env-var path or via the (currently unused) builder hook.
#[cfg(feature = "experimental-rust-tls")]
#[derive(Debug, Clone)]
pub struct TlsPaths {
    pub cert: String,
    pub key: String,
    pub client_ca: Option<String>,
}

#[cfg(feature = "experimental-rust-tls")]
fn tls_paths_from_env() -> Option<TlsPaths> {
    let cert = epics_base_rs::runtime::env::get("EPICS_CAS_TLS_CERT_FILE")?;
    let key = epics_base_rs::runtime::env::get("EPICS_CAS_TLS_KEY_FILE")?;
    let client_ca = epics_base_rs::runtime::env::get("EPICS_CAS_TLS_CLIENT_CA_FILE");
    Some(TlsPaths {
        cert,
        key,
        client_ca,
    })
}

/// Resolve an audit logger from environment variables. The default
/// builders call this so every CaServer picks up site-wide audit
/// configuration without code changes.
///
/// - `EPICS_CAS_AUDIT_FILE=<path>` writes JSON-Lines to the path
/// - `EPICS_CAS_AUDIT=stderr`      writes to stderr
/// - unset / empty                 disables audit
fn audit_from_env() -> Option<crate::audit::AuditLogger> {
    if let Some(path) = epics_base_rs::runtime::env::get("EPICS_CAS_AUDIT_FILE") {
        if !path.is_empty() {
            // Opened synchronously because this runs during server
            // construction, before any runtime is guaranteed. The handle stays
            // a `std::fs::File`: `AuditSink` writes it through the filesystem
            // seam, so no runtime is needed here or at any later write.
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                Ok(f) => {
                    let sink = crate::audit::AuditSink::File(crate::audit::AuditFile::from_std(f));
                    return Some(crate::audit::AuditLogger::new(sink));
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = %path,
                        "EPICS_CAS_AUDIT_FILE: failed to open; audit disabled");
                }
            }
        }
    }
    if let Some(val) = epics_base_rs::runtime::env::get("EPICS_CAS_AUDIT") {
        if val.eq_ignore_ascii_case("stderr") {
            return Some(crate::audit::AuditLogger::new(
                crate::audit::AuditSink::Stderr,
            ));
        }
    }
    None
}

/// Resolve the HTTP introspection bind address from the environment.
/// `EPICS_CAS_INTROSPECTION_ADDR=<host>:<port>` enables it; defaults
/// off.
fn introspection_from_env() -> Option<std::net::SocketAddr> {
    epics_base_rs::runtime::env::get("EPICS_CAS_INTROSPECTION_ADDR").and_then(|s| s.parse().ok())
}

/// Drain grace seconds from the env. Default 30 — long enough for a
/// rolling restart to finish active monitor batches, short enough
/// that a Kubernetes terminationGracePeriodSeconds of 60 still leaves
/// headroom for SIGKILL.
fn drain_grace_from_env() -> u64 {
    epics_base_rs::runtime::env::get("EPICS_CAS_DRAIN_GRACE_SECS")
        .and_then(|s| s.parse().ok())
        .unwrap_or(30)
}

#[cfg(test)]
mod access_notifier_tests {
    use super::*;

    async fn empty_server() -> CaServer {
        let db = Arc::new(PvDatabase::new());
        CaServer::from_parts(db, 0, None, None, None, None)
            .await
            .expect("bind ephemeral server")
    }

    // The detachable handle must keep firing after the CaServer value is
    // gone — that is the whole point: the gateway's upstream manager calls
    // it long after `run()` has consumed the server.
    #[tokio::test]
    async fn access_rights_notifier_fires_after_server_dropped() {
        let server = empty_server().await;
        let mut rx = server.acf_reload_tx.subscribe();
        let notifier = server.access_rights_notifier();
        drop(server);
        notifier.notify();
        assert!(
            rx.try_recv().is_ok(),
            "detached notifier must deliver to a live receiver after server drop"
        );
    }

    // notify_access_change and the detachable handle must drive the SAME
    // broadcast, so both re-push CA_PROTO_ACCESS_RIGHTS identically.
    #[tokio::test]
    async fn notify_access_change_and_handle_share_one_channel() {
        let server = empty_server().await;
        let mut rx = server.acf_reload_tx.subscribe();
        server.notify_access_change();
        assert!(rx.try_recv().is_ok(), "notify_access_change must send");
        server.access_rights_notifier().notify();
        assert!(
            rx.try_recv().is_ok(),
            "handle must send on the same channel"
        );
    }
}
