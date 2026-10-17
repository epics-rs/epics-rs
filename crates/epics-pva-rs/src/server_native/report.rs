//! The `pvxsr` report of a running PVA server, whichever backend serves it.
//!
//! Ungated: the tokio `PvaServer` and the exec backend's native drivers both
//! hand `pvxsr` one [`ServerReportHandle`], so the command reads one shape on
//! every build and is registered on every build.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether one serving loop (TCP accept, UDP responder) is still running.
///
/// The tokio server reads it from the task's `AbortHandle`; a native-driver
/// thread holds a [`LivenessGuard`] that clears it when the thread ends,
/// however it ends.
#[derive(Clone)]
pub struct Liveness(Arc<dyn Fn() -> bool + Send + Sync>);

impl Liveness {
    pub fn new(alive: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(alive))
    }

    /// A liveness that reads true until the returned guard drops.
    pub fn guarded() -> (Self, LivenessGuard) {
        let flag = Arc::new(AtomicBool::new(true));
        let read = flag.clone();
        (
            Self::new(move || read.load(Ordering::Acquire)),
            LivenessGuard(flag),
        )
    }

    pub fn alive(&self) -> bool {
        (self.0)()
    }
}

/// Held by the loop a [`Liveness::guarded`] reports on; dropping it — a
/// return, a panic unwinding — marks the loop down.
pub struct LivenessGuard(Arc<AtomicBool>);

impl Drop for LivenessGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Snapshot returned by `PvaServer::report`.
#[derive(Debug, Clone)]
pub struct ServerReport {
    pub tcp_port: u16,
    pub udp_port: u16,
    /// Bound dedicated-TLS port advertised as the `"tls"` SEARCH endpoint.
    /// Only meaningful when [`Self::tls_enabled`]; equals [`Self::tcp_port`]
    /// when TLS shares the plaintext port.
    pub tls_port: u16,
    pub tls_enabled: bool,
    pub ignore_addrs: usize,
    pub beacon_period_secs: u64,
    pub udp_alive: bool,
    /// `true` iff the v6 search responder is running. `false` also when
    /// `EPICS_PVAS_INTF_ADDR_LIST` has no v6 entry and pvxs opens none (the
    /// default) — distinct from "task crashed".
    pub udp_v6_alive: bool,
    pub tcp_alive: bool,
    /// Live per-connection counters captured under the registry's
    /// read lock. pvxs `Server::report()` parity at the
    /// "live peers + per-peer channel/op/byte counters" level.
    pub peers: Vec<(SocketAddr, super::peers::PeerSnapshot)>,
    /// Total currently-active connections.
    pub peer_count: usize,
}

/// Resolved inputs to [`assemble_report`]. Liveness booleans are already
/// computed by the caller (from JoinHandles in `PvaServer::report_zeroed`,
/// from AbortHandles in [`ServerReportHandle::report`]); everything else is
/// read straight through.
pub(crate) struct ReportFields<'a> {
    pub(crate) tcp_port: u16,
    pub(crate) udp_port: u16,
    pub(crate) tls_port: u16,
    pub(crate) tls_enabled: bool,
    pub(crate) ignore_addrs: usize,
    pub(crate) beacon_period_secs: u64,
    pub(crate) udp_alive: bool,
    pub(crate) udp_v6_alive: bool,
    pub(crate) tcp_alive: bool,
    pub(crate) peers: &'a super::peers::PeerRegistry,
    pub(crate) zero: bool,
}

/// The single owner of [`ServerReport`] construction. Both
/// `PvaServer::report_zeroed` and [`ServerReportHandle::report`] route
/// through here, so the snapshot/count pair and the struct's field set
/// cannot drift between the two paths — a new `ServerReport` field is added
/// in exactly one place.
pub(crate) fn assemble_report(f: ReportFields<'_>) -> ServerReport {
    ServerReport {
        tcp_port: f.tcp_port,
        udp_port: f.udp_port,
        tls_port: f.tls_port,
        tls_enabled: f.tls_enabled,
        ignore_addrs: f.ignore_addrs,
        beacon_period_secs: f.beacon_period_secs,
        udp_alive: f.udp_alive,
        udp_v6_alive: f.udp_v6_alive,
        tcp_alive: f.tcp_alive,
        peers: f.peers.snapshot_zeroed(f.zero),
        peer_count: f.peers.len(),
    }
}

/// Cheap, cloneable, `Send + Sync` handle to a running `PvaServer`'s
/// diagnostics, produced by `PvaServer::report_handle`.
///
/// Unlike a `&PvaServer` it survives the `run`/`wait` consumption of the
/// server, so an in-process iocsh command (`pvxsr`) running inside the
/// live server can snapshot the report even though the owning `PvaServer`
/// value lives — and is `wait()`-consumed — inside `run_pva_server`.
#[derive(Clone)]
pub struct ServerReportHandle {
    pub(crate) bound_tcp_port: u16,
    pub(crate) udp_port: u16,
    pub(crate) bound_tls_port: u16,
    pub(crate) tls_enabled: bool,
    pub(crate) ignore_addrs: usize,
    pub(crate) beacon_period_secs: u64,
    /// pvxs `operator<<(Server::config())` text of the bound server.
    pub(crate) pvxs_config: Arc<str>,
    pub(crate) peers: Arc<super::peers::PeerRegistry>,
    /// `None` for a bound server that serves nothing yet.
    pub(crate) running: Option<Running>,
}

/// What a started server adds to its [`ServerReportHandle`].
#[derive(Clone)]
pub(crate) struct Running {
    pub(crate) source: super::source::DynSource,
    pub(crate) tcp_alive: Liveness,
    pub(crate) udp_alive: Liveness,
    pub(crate) udp_v6_alive: Option<Liveness>,
}

impl ServerReportHandle {
    /// The TCP port the listeners actually bound, which is the only value
    /// that answers an ephemeral (`0`) or fallen-back request. Separate from
    /// [`Self::report`] because a caller that wants the port should not have
    /// to snapshot the peer registry to read it.
    pub fn bound_tcp_port(&self) -> u16 {
        self.bound_tcp_port
    }

    /// Snapshot the same [`ServerReport`] the server's own `report` gives,
    /// through the shared registry and each serving loop's [`Liveness`].
    /// Counters are never zeroed through this handle (`zero = false`); the
    /// resetting variant stays on `PvaServer::report_zeroed`, which owns the
    /// tasks.
    pub fn report(&self) -> ServerReport {
        let running = self.running.as_ref();
        assemble_report(ReportFields {
            tcp_port: self.bound_tcp_port,
            udp_port: self.udp_port,
            tls_port: self.bound_tls_port,
            tls_enabled: self.tls_enabled,
            ignore_addrs: self.ignore_addrs,
            beacon_period_secs: self.beacon_period_secs,
            udp_alive: running.is_some_and(|r| r.udp_alive.alive()),
            udp_v6_alive: running
                .and_then(|r| r.udp_v6_alive.as_ref())
                .is_some_and(Liveness::alive),
            tcp_alive: running.is_some_and(|r| r.tcp_alive.alive()),
            peers: &self.peers,
            zero: false,
        })
    }

    /// Whether the server was started: pvxs's `Running` against `Stopped`.
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// pvxs `operator<<(Server::config())`: `KEY=VALUE` lines.
    pub fn pvxs_config(&self) -> &str {
        &self.pvxs_config
    }

    /// pvxs's `Server` source map as `(name, order)`, in consultation
    /// order. Before the start only the server's own source, which pvxs's
    /// constructor adds (`server.cpp:542-547`) and `start` always composes.
    pub fn source_list(&self) -> Vec<(String, i32)> {
        match &self.running {
            Some(r) => r.source.registry_entries().unwrap_or_default(),
            None => vec![(super::server_info::SERVER_SOURCE_NAME.to_owned(), -1)],
        }
    }
}

/// pvxs `operator<<(server::Config)` (`config.cpp:526-541`) of
/// `Server::config()`: the `EPICS_PVAS_*` keys of `updateDefs`
/// (`config.cpp:474-483`) in key order, as the constructor leaves them —
/// expanded, so the auto beacon list reads `NO` with its broadcasts in the
/// explicit list, and with the bound ports (`server.cpp:397-486`).
///
/// pvxs keeps a list entry's text, so a port shows only where the entry
/// named one; the lists here are parsed with the search port filled in, so
/// a beacon entry on the search port prints without it, also when its text
/// named that port. A multicast interface entry's derived interfaces
/// (`config.cpp:497-509`, `addGroups`) are not printed.
pub(crate) fn pvxs_config_text(
    config: &super::PvaServerConfig,
    interfaces: &super::search_engine::ServerInterfaces,
    requested_udp_port: u16,
) -> String {
    use crate::config::env::sock_addr_text;
    // pvxs `operator<<(SockEndpoint)` (`config.cpp:125-135`); its `ttl`
    // defaults to -1, which prints.
    let endpoint = |ep: &crate::config::Endpoint| {
        let mut text = sock_addr_text(ep.addr);
        if ep.addr.ip().is_multicast() {
            if ep.ttl != 0 {
                text.push_str(&format!(",{}", ep.ttl));
            }
            if let Some(iface) = &ep.iface {
                text.push_str(&format!("@{iface}"));
            }
        }
        text
    };
    let join = |texts: Vec<String>| texts.join(" ");
    let beacon = crate::server_native::beacon::beacon_destinations(
        config.beacon_destinations.clone(),
        config.auto_beacon,
        &interfaces.bcast_ifaces(),
        0,
    );
    let beacon = join(
        beacon
            .iter()
            .map(|ep| {
                let mut ep = ep.clone();
                if ep.addr.port() == requested_udp_port {
                    ep.addr.set_port(0);
                }
                endpoint(&ep)
            })
            .collect(),
    );
    let ignore = join(
        config
            .ignore_addrs
            .iter()
            .map(|&(ip, port)| sock_addr_text(SocketAddr::new(ip, port)))
            .collect(),
    );
    // An empty list binds `bind_ip`, pvxs's `0.0.0.0` unless a caller set it.
    let intf = if config.interfaces.is_empty() {
        sock_addr_text(SocketAddr::new(config.bind_ip, 0))
    } else {
        join(config.interfaces.iter().map(endpoint).collect())
    };
    format!(
        "EPICS_PVAS_AUTO_BEACON_ADDR_LIST=NO\n\
         EPICS_PVAS_BEACON_ADDR_LIST={beacon}\n\
         EPICS_PVAS_BROADCAST_PORT={}\n\
         EPICS_PVAS_IGNORE_ADDR_LIST={ignore}\n\
         EPICS_PVAS_INTF_ADDR_LIST={intf}\n\
         EPICS_PVAS_SERVER_PORT={}\n",
        config.udp_port, config.tcp_port,
    )
}
