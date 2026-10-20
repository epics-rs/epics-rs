//! The native PVA server, started: a driver bound by `EPICS_PVA_RS_DRIVER`,
//! its UDP search responder, and their `PVAS-TCP` / `PVAS-UDP` /
//! `PVAS-beacon` threads.
//!
//! Two steps, because an IOC registers its status PVs against the bound
//! server before the accept thread lets the first client in.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use epics_base_rs::runtime::task::{Reactor, StackSizeClass};
use epics_base_rs::runtime::worker_pool::ThreadCharge;

use super::blocking::{SearchSocket, bind_udp_search, bind_udp_search_v6};
use super::config::PvaServerConfig;
use super::driver::{self, BeaconSender, DriverKind, PvaServerDriver};
use super::report::{Liveness, LivenessGuard, Running, ServerReportHandle, pvxs_config_text};
use super::search_engine::ServerInterfaces;
use super::serv_iface::ServListeners;
use super::shared_pv::SharedSource;
use super::source::DynSource;
use crate::error::{PvaError, PvaResult};
use tokio::sync::mpsc;

/// Why [`BoundPva::bind`] or [`BoundPva::serve`] did not come up. A search
/// port that cannot be bound is not here: the server stays reachable over
/// TCP through `EPICS_PVA_NAME_SERVERS`, so it is [`BoundPva::search`].
#[derive(Debug)]
pub enum ServeError {
    /// `EPICS_PVA_RS_DRIVER` names no driver.
    Driver(String),
    /// `EPICS_PVAS_INTF_ADDR_LIST` expands to an address no socket can take,
    /// which pvxs's `Server` constructor throws on.
    Interfaces(io::Error),
    /// The TCP listener on the configured `port`. Not "cannot bind": the
    /// listener set's bind also calls `local_addr`, which is what fails on
    /// RTEMS.
    Tcp { port: u16, cause: io::Error },
    /// The driver on the bound listeners: its GUID, poller or executors.
    Server(io::Error),
    /// A serving thread could not be created.
    Thread(&'static str, io::Error),
    /// The beacon senders or their destinations, which pvxs sets up in the
    /// `Server` constructor (`server.cpp:480-496`).
    Beacon(io::Error),
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Driver(e) => f.write_str(e),
            Self::Interfaces(e) => write!(f, "{e}"),
            Self::Tcp { port, cause } => {
                write!(f, "cannot start the PVA TCP server on port {port}: {cause}")
            }
            Self::Server(e) => write!(f, "cannot start the PVA server: {e}"),
            Self::Thread(name, e) => write!(f, "cannot start the {name} thread: {e}"),
            Self::Beacon(e) => write!(f, "cannot set up the PVA beacons: {e}"),
        }
    }
}

impl std::error::Error for ServeError {}

/// pvxs's v4 and v6 search collectors, each opened only when
/// `EPICS_PVAS_INTF_ADDR_LIST` has an entry of its family, both on one port
/// (`server.cpp:411-426`). At least one is bound.
pub struct SearchSockets {
    pub v4: Option<SearchSocket>,
    pub v6: Option<SearchSocket>,
}

impl SearchSockets {
    /// The search port, the first bound collector's.
    pub fn local_port(&self) -> io::Result<u16> {
        match (&self.v4, &self.v6) {
            (Some(s), _) | (None, Some(s)) => Ok(s.local_addr()?.port()),
            (None, None) => Err(io::Error::other("no search socket bound")),
        }
    }
}

/// The search responder's bind outcome: the socket, or the port it was
/// asked for and why it could not be had.
pub type SearchBind = Result<SearchSockets, (u16, io::Error)>;

/// A bound server whose threads have not started.
pub struct BoundPva {
    server: Arc<dyn PvaServerDriver>,
    kind: DriverKind,
    search: SearchBind,
    beacons: BeaconSender,
    udp_port: u16,
    ignore_addrs: usize,
    beacon_period_secs: u64,
    /// [`pvxs_config_text`] of the bound server.
    pvxs_config: Arc<str>,
}

/// The sockets a [`BoundPva`] serves on, bound before it has a source:
/// pvxs's `Server` constructor (`server.cpp:385-500`), which
/// `pvxsBaseRegistrar` runs before the startup script.
pub struct PvaSockets {
    kind: DriverKind,
    config: PvaServerConfig,
    listeners: ServListeners,
    search: SearchBind,
    udp_port: u16,
    /// [`pvxs_config_text`] of the bound server.
    pvxs_config: Arc<str>,
    /// The `__builtin` store, created with the sockets as pvxs's constructor
    /// creates it (`server.cpp:542`).
    builtin: Arc<SharedSource>,
}

impl PvaSockets {
    /// The TCP listeners on `config`'s address, and the search responder
    /// wildcard on its UDP port — wildcard so it receives broadcast and
    /// multicast SEARCHes, as pvxs binds it (`udp_collector.cpp:140-151`).
    pub fn bind(config: PvaServerConfig) -> Result<Self, ServeError> {
        let kind = DriverKind::from_env().map_err(ServeError::Driver)?;
        let (tcp_port, config_udp_port) = (config.tcp_port, config.udp_port);
        let interfaces = ServerInterfaces::resolve(
            &config.interfaces,
            &config.beacon_destinations,
            config.auto_beacon,
        )
        .map_err(ServeError::Interfaces)?;
        let search =
            bind_search(config_udp_port, &config, &interfaces).map_err(|e| (config_udp_port, e));
        let listeners = ServListeners::bind(SocketAddr::new(config.bind_ip, tcp_port), &config)
            .map_err(|cause| ServeError::Tcp {
                port: tcp_port,
                cause,
            })?;
        let udp_port = match &search {
            Ok(s) => s.local_port().unwrap_or(config_udp_port),
            Err(_) => config_udp_port,
        };
        let mut reported = config.clone();
        (reported.tcp_port, reported.udp_port) = (listeners.port(), udp_port);
        let pvxs_config = pvxs_config_text(&reported, &interfaces, config_udp_port).into();
        Ok(Self {
            kind,
            config,
            listeners,
            search,
            udp_port,
            pvxs_config,
            builtin: Arc::new(SharedSource::new()),
        })
    }

    /// [`bind`](Self::bind) as an IOC binds it, the tokio `PvaServer::bind`
    /// twin: a search port that cannot be had fails it, and the error is the
    /// `PvaError::Io` `pvxsBaseRegistrar`'s line prints pvxs's `what()` of.
    pub fn bind_ioc(config: PvaServerConfig) -> PvaResult<Self> {
        let sockets = Self::bind(config).map_err(|e| match e {
            ServeError::Tcp { cause, .. } | ServeError::Interfaces(cause) => PvaError::Io(cause),
            e => PvaError::Protocol(e.to_string()),
        })?;
        if let Err((_, e)) = &sockets.search {
            return Err(PvaError::Io(e.raw_os_error().map_or_else(
                || io::Error::new(e.kind(), e.to_string()),
                io::Error::from_raw_os_error,
            )));
        }
        Ok(sockets)
    }

    /// The TCP port the listeners hold — the requested one, or the fallback.
    pub fn bound_tcp_port(&self) -> u16 {
        self.listeners.port()
    }

    /// The `__builtin` store: pvxs's `Server::addPV` / `removePV`, which
    /// take PVs before `start` too.
    pub fn builtin(&self) -> &Arc<SharedSource> {
        &self.builtin
    }

    /// The report of a server that has not started: pvxs's `Stopped`, with
    /// only the internal sources and no peers.
    pub fn report_handle(&self) -> ServerReportHandle {
        ServerReportHandle {
            bound_tcp_port: self.bound_tcp_port(),
            udp_port: self.udp_port,
            bound_tls_port: 0,
            tls_enabled: false,
            ignore_addrs: self.config.ignore_addrs.len(),
            beacon_period_secs: self.config.beacon_period.as_secs(),
            pvxs_config: self.pvxs_config.clone(),
            peers: super::peers::PeerRegistry::new(),
            builtin: self.builtin.clone(),
            running: None,
        }
    }
}

impl BoundPva {
    /// [`PvaSockets::bind`] then [`attach`](Self::attach).
    pub fn bind(source: DynSource, config: PvaServerConfig) -> Result<Self, ServeError> {
        Self::attach(PvaSockets::bind(config)?, source)
    }

    /// `source` on `sockets`: the driver and the beacon senders.
    pub fn attach(sockets: PvaSockets, source: DynSource) -> Result<Self, ServeError> {
        let PvaSockets {
            kind,
            config,
            listeners,
            search,
            udp_port,
            pvxs_config,
            builtin,
        } = sockets;
        let ignore_addrs = config.ignore_addrs.len();
        let beacon_period_secs = config.beacon_period.as_secs();
        let server = driver::from_listeners(kind, listeners, source, builtin, config)
            .map_err(ServeError::Server)?;
        let beacons = server.beacon_sender(udp_port).map_err(ServeError::Beacon)?;
        Ok(Self {
            server,
            kind,
            search,
            beacons,
            udp_port,
            ignore_addrs,
            beacon_period_secs,
            pvxs_config,
        })
    }

    pub fn server(&self) -> &Arc<dyn PvaServerDriver> {
        &self.server
    }

    pub fn kind(&self) -> DriverKind {
        self.kind
    }

    pub fn search(&self) -> &SearchBind {
        &self.search
    }

    /// Start the accept loop on `reactor` and the search responder.
    pub fn serve(self, reactor: &Reactor) -> Result<PvaServing, ServeError> {
        let udp_port = self.udp_port;
        let (ended_tx, ended) = mpsc::unbounded_channel();
        let (tcp_alive, tcp_guard) = Liveness::guarded();
        let srv = self.server.clone();
        let reactor = reactor.clone();
        serving_thread("PVAS-TCP", tcp_guard, ended_tx.clone(), move || {
            srv.serve(&reactor);
            Ok(())
        })?;
        let (udp_alive, udp_v6_alive) = match self.search {
            Ok(SearchSockets { v4, v6 }) => {
                let v4 = match v4 {
                    Some(v4) => search_thread("PVAS-UDP", &self.server, v4, &ended_tx)?,
                    None => Liveness::new(|| false),
                };
                let v6 = v6
                    .map(|v6| search_thread("PVAS-UDP6", &self.server, v6, &ended_tx))
                    .transpose()?;
                (v4, v6)
            }
            Err(_) => (Liveness::new(|| false), None),
        };
        let beacon_stop = Arc::new(BeaconStop::default());
        let (beacons, stop) = (self.beacons, beacon_stop.clone());
        let (_, beacon_guard) = Liveness::guarded();
        serving_thread("PVAS-beacon", beacon_guard, ended_tx, move || {
            beacons.run(&mut |d| stop.sleep(d));
            Ok(())
        })?;
        let report = ServerReportHandle {
            bound_tcp_port: self.server.tcp_port(),
            udp_port,
            bound_tls_port: 0,
            tls_enabled: false,
            ignore_addrs: self.ignore_addrs,
            beacon_period_secs: self.beacon_period_secs,
            pvxs_config: self.pvxs_config,
            peers: self.server.peers().clone(),
            builtin: self.server.builtin().clone(),
            running: Some(Running {
                source: self.server.source().clone(),
                tcp_alive,
                udp_alive,
                udp_v6_alive,
            }),
        };
        Ok(PvaServing {
            server: self.server.clone(),
            beacon_stop,
            kind: self.kind,
            ended,
            report,
        })
    }
}

/// A native server serving on its own threads. Dropping it shuts the server
/// down, so an aborted [`wait`](Self::wait) stops it as the tokio server's
/// dropped task does.
pub struct PvaServing {
    server: Arc<dyn PvaServerDriver>,
    beacon_stop: Arc<BeaconStop>,
    kind: DriverKind,
    ended: mpsc::UnboundedReceiver<PvaResult<()>>,
    report: ServerReportHandle,
}

impl PvaServing {
    pub fn server(&self) -> &Arc<dyn PvaServerDriver> {
        &self.server
    }

    pub fn kind(&self) -> DriverKind {
        self.kind
    }

    /// What `pvxsr` reads; publish it with `iocsh::publish_pvxs_report`.
    pub fn report_handle(&self) -> ServerReportHandle {
        self.report.clone()
    }

    /// Until either loop ends, which ends the server — the tokio server's
    /// `wait` rule. The first loop's result is the result.
    pub async fn wait(mut self) -> PvaResult<()> {
        self.ended.recv().await.unwrap_or(Ok(()))
    }
}

impl Drop for PvaServing {
    fn drop(&mut self) {
        self.server.shutdown();
        self.beacon_stop.stop();
    }
}

/// The `PVAS-beacon` thread's sleep, cut short when serving ends.
#[derive(Default)]
struct BeaconStop {
    stopped: Mutex<bool>,
    wake: Condvar,
}

impl BeaconStop {
    /// Sleep `period` or until [`stop`](Self::stop); `true` once stopped.
    fn sleep(&self, period: Duration) -> bool {
        let stopped = self.stopped.lock().unwrap_or_else(|e| e.into_inner());
        let (stopped, _) = self
            .wake
            .wait_timeout_while(stopped, period, |s| !*s)
            .unwrap_or_else(|e| e.into_inner());
        *stopped
    }

    fn stop(&self) {
        *self.stopped.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake.notify_all();
    }
}

/// [`BoundPva`] then [`PvaServing::wait`] on the current reactor, with
/// `on_started` handed the report the moment it serves — the exec twin of
/// `runtime::run_pva_server_reporting`, which the IOC runners call through.
/// A search port it cannot bind fails the start, as the tokio server's does.
pub async fn run_native_server_reporting(
    source: DynSource,
    sockets: PvaSockets,
    on_started: impl FnOnce(ServerReportHandle),
) -> PvaResult<()> {
    let bound = BoundPva::attach(sockets, source).map_err(|e| PvaError::Protocol(e.to_string()))?;
    let reactor = Reactor::current()
        .ok_or_else(|| PvaError::Protocol("no reactor to serve PVA on".into()))?;
    let serving = bound
        .serve(&reactor)
        .map_err(|e| PvaError::Protocol(e.to_string()))?;
    on_started(serving.report_handle());
    serving.wait().await
}

/// One serving thread: its stack class stated, charged to the process
/// account, reported down by `alive` the moment its body ends, and its
/// result — a panic included — sent on `ended`.
/// The collectors `config`'s interfaces ask for, or none: as the tokio
/// backend's `bind_udp`, a start that cannot hold the v6 socket on the v4
/// number fails, and on a kernel-chosen number tries another.
fn bind_search(
    port: u16,
    config: &PvaServerConfig,
    interfaces: &ServerInterfaces,
) -> io::Result<SearchSockets> {
    const EPHEMERAL_TRIES: usize = 32;
    let v6 = interfaces.v6();
    if !interfaces.has_v4() {
        let listen = v6.ok_or_else(|| io::Error::other("no search interface"))?;
        let v6 = bind_udp_search_v6(port, interfaces, listen)?;
        return Ok(SearchSockets {
            v4: None,
            v6: Some(v6),
        });
    }
    let mut tries = 1;
    loop {
        let v4 = bind_udp_search(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port), config)?;
        let Some(listen) = v6.clone() else {
            return Ok(SearchSockets {
                v4: Some(v4),
                v6: None,
            });
        };
        match bind_udp_search_v6(v4.local_addr()?.port(), interfaces, listen) {
            Ok(v6) => {
                return Ok(SearchSockets {
                    v4: Some(v4),
                    v6: Some(v6),
                });
            }
            Err(e) if port == 0 && epics_base_rs::net::port_held(&e) && tries < EPHEMERAL_TRIES => {
                tries += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// A `name` thread answering SEARCHes on `socket`; its liveness.
fn search_thread(
    name: &'static str,
    server: &Arc<dyn PvaServerDriver>,
    socket: SearchSocket,
    ended: &mpsc::UnboundedSender<PvaResult<()>>,
) -> Result<Liveness, ServeError> {
    let (alive, guard) = Liveness::guarded();
    let srv = server.clone();
    serving_thread(name, guard, ended.clone(), move || {
        srv.serve_udp_search(socket)
    })?;
    Ok(alive)
}

fn serving_thread(
    name: &'static str,
    alive: LivenessGuard,
    ended: mpsc::UnboundedSender<PvaResult<()>>,
    body: impl FnOnce() -> PvaResult<()> + Send + 'static,
) -> Result<(), ServeError> {
    let charge = ThreadCharge::fixed(StackSizeClass::Medium);
    thread::Builder::new()
        .name(name.to_string())
        // Accepts and hands off; the per-connection threads are where the
        // depth is.
        .stack_size(StackSizeClass::Medium.bytes())
        .spawn(move || {
            let _charge = charge;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body))
                .unwrap_or_else(|_| Err(PvaError::Protocol(format!("{name} panicked"))));
            drop(alive);
            let _ = ended.send(result);
        })
        .map(drop)
        .map_err(|e| ServeError::Thread(name, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_native::blocking::tests::{harness_reactor, isolated_config, test_source};
    use source_guard::{Comments, production};
    use std::time::{Duration, Instant};

    /// The native drivers beacon too: the first at once, advertising the
    /// bound TCP port, to `beacon_destinations` (pvxs `doBeacons`).
    #[test]
    fn serving_beacons_its_tcp_port() {
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let config = PvaServerConfig {
            beacon_destinations: vec![rx.local_addr().unwrap().into()],
            ..isolated_config()
        };
        let serving = BoundPva::bind(test_source(), config)
            .expect("bind ephemeral")
            .serve(&harness_reactor())
            .expect("serve");

        let mut buf = [0u8; 256];
        let n = rx.recv(&mut buf).expect("a beacon");
        let beacon = &buf[..n];
        assert_eq!(beacon[3], crate::proto::Command::Beacon.code());
        // header (8), GUID (12), flags/seq/change (4), address (16), port.
        assert_eq!(beacon[2], 0xc0, "big-endian server message");
        let port = u16::from_be_bytes([beacon[40], beacon[41]]);
        assert_eq!(port, serving.server().tcp_port());
        assert_eq!(&beacon[8..20], &serving.server().guid());
    }

    /// `pvxsr` on an exec IOC reads the server it is running: its port, its
    /// two loops up, and the client that just connected.
    #[test]
    fn the_report_reads_the_served_server() {
        let serving = BoundPva::bind(test_source(), isolated_config())
            .expect("bind ephemeral")
            .serve(&harness_reactor())
            .expect("serve");
        let handle = serving.report_handle();
        let port = serving.server().tcp_port();
        assert_eq!(handle.bound_tcp_port(), port);

        let _client = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle.report().peer_count == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let report = handle.report();
        assert_eq!(report.tcp_port, port);
        assert_ne!(report.udp_port, 0, "the bound search port, not the request");
        assert!(report.tcp_alive && report.udp_alive);
        assert_eq!(report.peer_count, 1);

        // Dropping the serving shuts it down: both loops report down.
        drop(serving);
        let deadline = Instant::now() + Duration::from_secs(5);
        while (handle.report().tcp_alive || handle.report().udp_alive) && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!handle.report().tcp_alive && !handle.report().udp_alive);
    }

    /// `config` listening on `interfaces`.
    fn on_interfaces(interfaces: &[&str]) -> PvaServerConfig {
        PvaServerConfig {
            interfaces: interfaces
                .iter()
                .map(|t| crate::config::Endpoint::parse(t, 0).unwrap())
                .collect(),
            ..isolated_config()
        }
    }

    /// `pvxsr`'s `config()` lines name the ports the bind landed on and its
    /// source lines the served composition, as the tokio server's do.
    #[test]
    fn the_report_carries_the_bound_config_and_sources() {
        let serving = BoundPva::bind(test_source(), on_interfaces(&["127.0.0.1"]))
            .expect("bind ephemeral")
            .serve(&harness_reactor())
            .expect("serve");
        let handle = serving.report_handle();
        let (tcp, udp) = (handle.bound_tcp_port(), handle.report().udp_port);
        assert_ne!((tcp, udp), (0, 0));
        assert_eq!(
            handle.pvxs_config(),
            format!(
                "EPICS_PVAS_AUTO_BEACON_ADDR_LIST=NO\n\
                 EPICS_PVAS_BEACON_ADDR_LIST=\n\
                 EPICS_PVAS_BROADCAST_PORT={udp}\n\
                 EPICS_PVAS_IGNORE_ADDR_LIST=\n\
                 EPICS_PVAS_INTF_ADDR_LIST=127.0.0.1\n\
                 EPICS_PVAS_SERVER_PORT={tcp}\n"
            )
        );
        // The composition the driver serves, not the source it was handed.
        assert_eq!(
            handle.source_list(),
            [
                ("__builtin".to_owned(), -1),
                ("__server".to_owned(), -1),
                ("__user".to_owned(), 0)
            ]
        );
    }

    /// A v4-only list opens no v6 collector, a v6-only one no v4 collector
    /// (pvxs `onSearch` per entry, `server.cpp:411-426`).
    #[test]
    fn each_family_is_opened_only_for_its_entries() {
        for (intf, v4, v6) in [
            (&[][..], true, false),
            (&["127.0.0.1"][..], true, false),
            (&["[::1]"][..], false, true),
            (&["0.0.0.0", "[::]"][..], true, true),
        ] {
            let bound = BoundPva::bind(test_source(), on_interfaces(intf)).expect("bind");
            let Ok(s) = bound.search() else {
                panic!("{intf:?}: search bind failed");
            };
            assert_eq!((s.v4.is_some(), s.v6.is_some()), (v4, v6), "{intf:?}");
        }
    }

    /// The `[::]` collector answers a v6 SEARCH on the v4 search number,
    /// from its own thread.
    #[test]
    fn the_v6_search_socket_answers_on_the_search_port() {
        let serving = BoundPva::bind(test_source(), on_interfaces(&["0.0.0.0", "[::]"]))
            .expect("bind ephemeral")
            .serve(&harness_reactor())
            .expect("serve");
        let report = serving.report_handle().report();
        assert!(report.udp_alive && report.udp_v6_alive);

        let client = std::net::UdpSocket::bind("[::1]:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let codec = crate::codec::PvaCodec { big_endian: false };
        let search = codec.build_search(
            7,
            9,
            "dut",
            [0; 4],
            client.local_addr().unwrap().port(),
            false,
        );
        client
            .send_to(&search, (std::net::Ipv6Addr::LOCALHOST, report.udp_port))
            .unwrap();
        let mut buf = [0u8; 512];
        let n = client.recv(&mut buf).expect("a SEARCH_RESPONSE over v6");
        let golden = crate::server_native::search::build_search_response_proto(
            serving.server().guid(),
            7,
            serving.server().tcp_port(),
            &[9],
            crate::proto::ByteOrder::Little,
            "tcp",
        );
        assert_eq!(&buf[..n], &golden[..]);
    }

    /// Each fixed thread states its class and carries its charge inside the
    /// body, as the entry points' own guards require of theirs.
    #[test]
    fn every_serving_thread_is_classed_and_charged() {
        let prod = production(include_str!("serving.rs"), Comments::Strip);
        let sites: Vec<&str> = prod
            .split(concat!("thread", "::Builder::new()"))
            .skip(1)
            .collect();
        assert_eq!(sites.len(), 1, "one spawn site, `serving_thread`");
        for after in sites {
            let (chain, body) = after.split_once(".spawn(").unwrap_or((after, ""));
            assert!(chain.contains(".stack_size("), "unclassed: {chain}");
            assert!(
                body[..body.len().min(120)].contains("let _charge = charge;"),
                "uncharged: {body}"
            );
        }
    }
}
