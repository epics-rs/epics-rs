//! RSRV on the blocking driver, started: the listener set, the search
//! responders and their threads, with the `casr` source published.
//!
//! Its own file because `blocking.rs` pins zero thread creations — every
//! per-client thread there is a pool worker — and these are the fixed ones.

use std::fmt;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::task::StackSizeClass;
use epics_base_rs::runtime::worker_pool::ThreadCharge;
use epics_base_rs::server::access_security::AcfCell as SharedAcf;
use epics_base_rs::server::database::PvDatabase;

use crate::server::blocking::BlockingCaServer;
use crate::server::stats::ServerStats;
use crate::server::{addr_list, iocsh, tcp, udp};

/// Why [`BlockingCaServing::start`] did not come up — each its own
/// diagnosis, so a caller does not label a search-port clash "cannot bind".
/// The TCP and UDP wording is `realtime-ca-ioc`'s measured console.
#[derive(Debug)]
pub enum ServeError {
    /// The interface list (`EPICS_CAS_INTF_ADDR_LIST`).
    Config(CaError),
    /// The TCP listener set (`rsrv_grab_tcp`) on the configured `port`.
    Tcp { port: u16, cause: io::Error },
    /// A UDP name-search responder.
    Udp { port: u16, cause: io::Error },
    /// A serving thread could not be created.
    Thread(&'static str, io::Error),
    /// The beacon socket, which `rsrv_init` opens before any thread and
    /// cannot proceed without (`caservertask.c:297-299`).
    Beacon(io::Error),
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(e) => write!(f, "cannot start the CA server: {e}"),
            Self::Tcp { port, cause } => {
                write!(f, "cannot start the CA TCP server on port {port}: {cause}")
            }
            Self::Udp { port, cause } => write!(
                f,
                "cannot start the CA UDP search responder on port {port}: {cause}"
            ),
            Self::Thread(name, e) => write!(f, "cannot start the {name} thread: {e}"),
            Self::Beacon(e) => write!(f, "cannot open the CA beacon socket: {e}"),
        }
    }
}

impl std::error::Error for ServeError {}

/// RSRV on the blocking driver: C `rsrv_init` + `rsrv_run` for a build with
/// no reactor, and the one serving path every such build takes — the
/// `realtime-ca-ioc` entry point and the `exec_backend` IOC runner alike.
///
/// One TCP listener per `EPICS_CAS_INTF_ADDR_LIST` interface, bound by the
/// rule the async server binds by (`bind_tcp_set_with`), each with its
/// `CAS-TCP` accept thread, and the name-search responders the async server
/// binds too (`udp::bind_udp_responders_with`): a `CAS-UDP` socket per
/// interface, joined to every `casMCastAddrList` group, and a `CAS-UDP2` on
/// a specific interface's broadcast address; and one `CAS-beacon` thread for
/// the process. Every listener counts into one [`ServerStats`], and the
/// `casr` source is published here, the moment the sockets are bound, as
/// `CaServerBuilder::build` publishes it for the async server.
pub struct BlockingCaServing {
    servers: Vec<Arc<BlockingCaServer>>,
    tcp: Vec<thread::JoinHandle<()>>,
    udp: Vec<thread::JoinHandle<CaResult<()>>>,
    beacon: thread::JoinHandle<()>,
    beacon_stop: Arc<BeaconStop>,
    stats: Arc<ServerStats>,
    tcp_port: u16,
    udp_port: u16,
}

/// RSRV's sockets on the blocking driver, bound by `iocInit` at C's
/// `dbInitServers()` point (`rsrv_init`) and served from `iocRun`
/// ([`BlockingCaServing::serve`]).
pub struct BlockingRsrvSockets {
    listeners: Vec<TcpListener>,
    responders: Vec<udp::BoundResponder>,
    cfg: addr_list::CasUdpConfig,
    beacon: std::net::UdpSocket,
    tcp_port: u16,
    udp_port: u16,
}

impl BlockingRsrvSockets {
    /// Bind UDP search on `udp_port` and TCP on `tcp_port` (the same number
    /// unless `EPICS_CAS_SERVER_PORT` split them), with C's fallback to an
    /// OS-picked TCP port when it is taken.
    pub fn bind(udp_port: u16, tcp_port: u16) -> Result<Self, ServeError> {
        epics_base_rs::server::event_queue::configure_events_per_que();
        let requested_udp = udp_port;
        let intf = tcp::rsrv_intf_addrs().map_err(ServeError::Config)?;
        let (mut listeners, bound_tcp) =
            tcp::bind_tcp_set_with(intf, tcp_port, TcpListener::bind, |l| {
                Ok(l.local_addr()?.port())
            })
            .map_err(|cause| ServeError::Tcp {
                port: tcp_port,
                cause,
            })?;
        tcp::announce_tcp_port(tcp_port, bound_tcp);

        // The responders take the TCP set's interfaces, which have already
        // lost any address that is no interface of this host
        // (`caservertask.c:225-226`), and join every `EPICS_CAS_MCAST` group;
        // an interface whose UDP bind is held is dropped whole, TCP listener
        // included (`:744-749`).
        let cfg = addr_list::from_env().map_err(ServeError::Config)?;
        let bound_udp = udp::bind_udp_responders_with(
            udp_port,
            listeners.iter().map(|(_, ip)| *ip).collect(),
            &cfg.mcast_addrs,
            bound_tcp,
            &udp::responder_broadcast_for,
        )
        .map_err(|e| match e {
            CaError::Io(cause) => ServeError::Udp {
                port: udp_port,
                cause,
            },
            e => ServeError::Config(e),
        })?;
        let keep = bound_udp.interfaces();
        listeners.retain(|(_, ip)| keep.contains(ip));
        let udp_port = bound_udp.port();
        udp::announce_requested_ephemeral(
            (requested_udp, udp_port),
            (tcp_port, bound_tcp),
            cfg.beacon_addrs.len(),
        );
        let beacon = crate::server::beacon::beacon_socket().map_err(ServeError::Beacon)?;
        Ok(Self {
            listeners: listeners.into_iter().map(|(l, _)| l).collect(),
            responders: bound_udp.into_responders(),
            cfg,
            beacon,
            tcp_port: bound_tcp,
            udp_port,
        })
    }
}

impl BlockingCaServing {
    /// [`BlockingRsrvSockets::bind`], then [`Self::serve`].
    pub fn start(
        udp_port: u16,
        tcp_port: u16,
        db: Arc<PvDatabase>,
        acf: SharedAcf,
        stats: Arc<ServerStats>,
    ) -> Result<Self, ServeError> {
        Self::serve(
            BlockingRsrvSockets::bind(udp_port, tcp_port)?,
            db,
            acf,
            stats,
        )
    }

    /// Serve `db` on `sockets`. Every listener counts into `stats`, so a
    /// reader built before the serving starts (a status PV) reads the live
    /// numbers.
    pub fn serve(
        sockets: BlockingRsrvSockets,
        db: Arc<PvDatabase>,
        acf: SharedAcf,
        stats: Arc<ServerStats>,
    ) -> Result<Self, ServeError> {
        let BlockingRsrvSockets {
            listeners,
            responders,
            cfg,
            beacon: socket,
            tcp_port: bound_tcp,
            udp_port,
        } = sockets;
        let tcp_err = move |cause| ServeError::Tcp {
            port: bound_tcp,
            cause,
        };
        let _ = stats.started_at.set(std::time::Instant::now());
        let mut servers = Vec::with_capacity(listeners.len());
        let mut sockets = Vec::with_capacity(listeners.len());
        for (listener, responder) in listeners.into_iter().zip(responders) {
            let server =
                BlockingCaServer::from_listener(listener, db.clone(), acf.clone(), stats.clone())
                    .map_err(tcp_err)?;
            servers.push(Arc::new(server));
            sockets.push(responder);
        }

        iocsh::publish_casr_source(
            stats.clone(),
            iocsh::CasrAddrs {
                interfaces: sockets
                    .iter()
                    .map(|r| iocsh::CasrInterface {
                        tcp: SocketAddr::from((r.bind_ip, bound_tcp)),
                        udp: SocketAddr::from((r.bind_ip, udp_port)),
                        udp_bcast: r.bcast.as_ref().and_then(|b| b.local_addr().ok()),
                    })
                    .collect(),
                mcast: cfg
                    .mcast_addrs
                    .iter()
                    .map(|ip| SocketAddr::from((*ip, udp_port)))
                    .collect(),
                beacon: cfg.beacon_addrs.clone(),
                // C prints these with `sin_port = 0`.
                ignore: cfg
                    .ignore_addrs
                    .iter()
                    .map(|ip| SocketAddr::from((*ip, 0)))
                    .collect(),
            },
        );

        let mut tcp = Vec::with_capacity(servers.len());
        let mut udp = Vec::with_capacity(servers.len());
        for (server, responder) in servers.iter().zip(sockets) {
            let srv = server.clone();
            tcp.push(serving_thread("CAS-TCP", move || srv.serve())?);
            let (srv, reply) = (server.clone(), responder.socket.clone());
            udp.push(serving_thread("CAS-UDP", move || {
                srv.serve_udp_responder(&reply, &reply, "CAS-UDP")
            })?);
            if let Some(bcast) = responder.bcast {
                let (srv, reply) = (server.clone(), responder.socket);
                udp.push(serving_thread("CAS-UDP2", move || {
                    srv.serve_udp_responder(&bcast, &reply, "CAS-UDP2")
                })?);
            }
        }
        let beacon_stop = Arc::new(BeaconStop::default());
        let stop = beacon_stop.clone();
        let (beacon_addrs, period) = (cfg.beacon_addrs.clone(), cfg.beacon_period);
        let beacon = serving_thread("CAS-beacon", move || {
            crate::server::blocking::serve_beacons(socket, bound_tcp, &beacon_addrs, period, |d| {
                stop.sleep(d)
            })
        })?;
        Ok(Self {
            servers,
            tcp,
            udp,
            beacon,
            beacon_stop,
            stats,
            tcp_port: bound_tcp,
            udp_port,
        })
    }

    /// The TCP port every listener ended up on.
    pub fn tcp_port(&self) -> u16 {
        self.tcp_port
    }

    /// The UDP port the search responders are bound to.
    pub fn udp_port(&self) -> u16 {
        self.udp_port
    }

    /// Clients currently served, across every listener.
    pub fn active_connections(&self) -> usize {
        self.stats.active_clients() as usize
    }

    /// Stop accepting on every listener, and beaconing.
    pub fn shutdown(&self) {
        for server in &self.servers {
            server.shutdown();
        }
        self.beacon_stop.stop();
    }

    /// Wait for the serving threads; the first name-search failure is the
    /// result. Returns once every accept loop has ended.
    pub fn join(self) -> CaResult<()> {
        for t in self.tcp {
            let _ = t.join();
        }
        let mut result = Ok(());
        let beacon = self
            .beacon
            .join()
            .map_err(|_| CaError::InvalidValue("the CA beacon thread panicked".into()));
        for u in self.udp {
            let r = u.join().unwrap_or_else(|_| {
                Err(CaError::InvalidValue(
                    "the CA name-search thread panicked".into(),
                ))
            });
            if result.is_ok() {
                result = r;
            }
        }
        result.and(beacon)
    }
}

/// The `CAS-beacon` thread's sleep, cut short when serving ends.
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

/// One RSRV serving thread, named as C names it and charged to the
/// process's stack account.
fn serving_thread<T: Send + 'static>(
    name: &'static str,
    body: impl FnOnce() -> T + Send + 'static,
) -> Result<thread::JoinHandle<T>, ServeError> {
    let charge = ThreadCharge::fixed(StackSizeClass::Medium);
    thread::Builder::new()
        .name(name.to_string())
        .stack_size(StackSizeClass::Medium.bytes())
        .spawn(move || {
            let _charge = charge;
            body()
        })
        .map_err(|e| ServeError::Thread(name, e))
}

#[cfg(test)]
mod tests {
    use source_guard::{Comments, production};

    /// An ephemeral request is answered with the ports it got, as the async
    /// server answers it; a configured pair prints nothing, as C prints
    /// nothing.
    #[test]
    fn only_an_ephemeral_request_announces_its_ports() {
        use super::*;
        use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let _listener = errlog_add_listener(move |line| {
            if line.starts_with("CA server: UDP search on port ") {
                sink.lock().unwrap().push(line.trim_end().to_string());
            }
        });

        udp::announce_requested_ephemeral((5064, 5064), (5064, 5064), 1);
        errlog_flush();
        assert!(
            seen.lock().unwrap().is_empty(),
            "a configured port is silent"
        );

        let serving = BlockingCaServing::start(
            0,
            0,
            Arc::new(PvDatabase::new()),
            epics_base_rs::server::access_security::new_acf_cell(None),
            Arc::new(ServerStats::default()),
        )
        .expect("bind an ephemeral server");
        errlog_flush();
        let lines = seen.lock().unwrap().clone();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with(&format!(
                "CA server: UDP search on port {}, TCP on port {}, beacons → ",
                serving.udp_port(),
                serving.tcp_port()
            )),
            "{lines:?}"
        );
    }

    /// The `CAS-beacon` thread beacons the TCP port to
    /// `EPICS_CAS_BEACON_ADDR_LIST` and ends with the serving.
    #[test]
    fn serving_beacons_its_tcp_port_until_shutdown() {
        use super::*;
        use crate::protocol::{CA_PROTO_RSRV_IS_UP, CaHeader};

        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // SAFETY: nextest runs this test alone in its process, before the
        // memoized `addr_list::from_env` first reads these.
        unsafe {
            std::env::set_var(
                "EPICS_CAS_BEACON_ADDR_LIST",
                rx.local_addr().unwrap().to_string(),
            );
            std::env::set_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "NO");
            std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1");
        }
        let serving = BlockingCaServing::start(
            0,
            0,
            Arc::new(PvDatabase::new()),
            epics_base_rs::server::access_security::new_acf_cell(None),
            Arc::new(ServerStats::default()),
        )
        .expect("bind an ephemeral server");

        let mut buf = [0u8; 64];
        let n = rx.recv(&mut buf).expect("a beacon");
        let hdr = CaHeader::from_bytes(&buf[..n]).unwrap();
        assert_eq!(
            (hdr.cmmd, hdr.count),
            (CA_PROTO_RSRV_IS_UP, serving.tcp_port())
        );

        serving.shutdown();
        serving
            .join()
            .expect("every serving thread ends on shutdown");
    }

    /// Each fixed thread states its class and carries its charge inside the
    /// body, released by the thread ending — the rule
    /// `every_thread_here_is_charged_to_the_process_account` pins for the
    /// entry point, here for the threads it no longer starts itself.
    #[test]
    fn every_serving_thread_is_classed_and_charged() {
        let prod = production(include_str!("blocking_serving.rs"), Comments::Strip);
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
