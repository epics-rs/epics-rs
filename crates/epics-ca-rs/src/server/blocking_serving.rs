//! RSRV on the blocking driver, started: the listener set, the search
//! responders and their threads, with the `casr` source published.
//!
//! Its own file because `blocking.rs` pins zero thread creations — every
//! per-client thread there is a pool worker — and these are the fixed ones.

use std::fmt;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::task::StackSizeClass;
use epics_base_rs::runtime::worker_pool::ThreadCharge;
use epics_base_rs::server::access_security::AcfCell as SharedAcf;
use epics_base_rs::server::database::PvDatabase;

use crate::server::blocking::{BlockingCaServer, RunControl};
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
/// `casr` source is published once the serving runs.
pub struct BlockingCaServing {
    servers: Vec<Arc<BlockingCaServer>>,
    tcp: Vec<thread::JoinHandle<()>>,
    udp: Vec<thread::JoinHandle<CaResult<()>>>,
    /// `None` only inside [`Self::serve`], before the beacon has started.
    beacon: Option<thread::JoinHandle<()>>,
    /// Held until every thread above has started.
    run: Arc<RunControl>,
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
    ///
    /// `Ok` only once every serving thread runs. A start shows clients
    /// nothing until then (no accept, search reply or beacon, no `casr`
    /// source, no uptime), and a thread that cannot be created stops and
    /// joins those already started, so `Err` leaves nothing behind and every
    /// socket closed. C cannot fail here (`epicsThreadMustCreate`,
    /// `caservertask.c:716-758`) and beacons and fills `casr` from `rsrv_init`.
    pub fn serve(
        sockets: BlockingRsrvSockets,
        db: Arc<PvDatabase>,
        acf: SharedAcf,
        stats: Arc<ServerStats>,
    ) -> Result<Self, ServeError> {
        Self::serve_with(sockets, db, acf, stats, &mut |_| Ok(()))
    }

    /// [`Self::serve`], asking `admit` before each thread is created, where a
    /// test refuses one as a target out of thread resources does.
    fn serve_with(
        sockets: BlockingRsrvSockets,
        db: Arc<PvDatabase>,
        acf: SharedAcf,
        stats: Arc<ServerStats>,
        admit: &mut dyn FnMut(&'static str) -> io::Result<()>,
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
        let run = Arc::new(RunControl::held());
        let mut servers = Vec::with_capacity(listeners.len());
        let mut sockets = Vec::with_capacity(listeners.len());
        for (listener, responder) in listeners.into_iter().zip(responders) {
            let server = BlockingCaServer::with_run(
                listener,
                db.clone(),
                acf.clone(),
                stats.clone(),
                run.clone(),
            )
            .map_err(tcp_err)?;
            servers.push(Arc::new(server));
            sockets.push(responder);
        }

        let casr = iocsh::CasrAddrs {
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
        };

        let mut serving = Self {
            servers,
            tcp: Vec::with_capacity(sockets.len()),
            udp: Vec::with_capacity(sockets.len()),
            beacon: None,
            run,
            stats,
            tcp_port: bound_tcp,
            udp_port,
        };
        if let Err(e) = serving.start_threads(sockets, socket, &cfg, admit) {
            serving.shutdown();
            let _ = serving.join();
            return Err(e);
        }
        let _ = serving.stats.started_at.set(std::time::Instant::now());
        iocsh::publish_casr_source(serving.stats.clone(), casr);
        serving.run.run();
        Ok(serving)
    }

    /// Start the serving threads, each kept as it starts so that
    /// [`shutdown`](Self::shutdown) and [`join`](Self::join) reach it.
    fn start_threads(
        &mut self,
        responders: Vec<udp::BoundResponder>,
        beacon: std::net::UdpSocket,
        cfg: &addr_list::CasUdpConfig,
        admit: &mut dyn FnMut(&'static str) -> io::Result<()>,
    ) -> Result<(), ServeError> {
        for (server, responder) in self.servers.iter().zip(responders) {
            let srv = server.clone();
            self.tcp
                .push(serving_thread(admit, "CAS-TCP", move || srv.serve())?);
            let (srv, reply) = (server.clone(), responder.socket.clone());
            self.udp.push(serving_thread(admit, "CAS-UDP", move || {
                srv.serve_udp_responder(&reply, &reply, "CAS-UDP")
            })?);
            if let Some(bcast) = responder.bcast {
                let (srv, reply) = (server.clone(), responder.socket);
                self.udp.push(serving_thread(admit, "CAS-UDP2", move || {
                    srv.serve_udp_responder(&bcast, &reply, "CAS-UDP2")
                })?);
            }
        }
        let run = self.run.clone();
        let tcp_port = self.tcp_port;
        let (beacon_addrs, period) = (cfg.beacon_addrs.clone(), cfg.beacon_period);
        self.beacon = Some(serving_thread(admit, "CAS-beacon", move || {
            crate::server::blocking::serve_beacons(beacon, tcp_port, &beacon_addrs, period, &run)
        })?);
        Ok(())
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
        self.run.stop();
    }

    /// Wait for the serving threads; the first name-search failure is the
    /// result. Returns once every accept loop has ended.
    pub fn join(self) -> CaResult<()> {
        for t in self.tcp {
            let _ = t.join();
        }
        let mut result = Ok(());
        let beacon = self.beacon.map_or(Ok(()), |b| {
            b.join()
                .map_err(|_| CaError::InvalidValue("the CA beacon thread panicked".into()))
        });
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

/// One RSRV serving thread, named as C names it and charged to the
/// process's stack account.
fn serving_thread<T: Send + 'static>(
    admit: &mut dyn FnMut(&'static str) -> io::Result<()>,
    name: &'static str,
    body: impl FnOnce() -> T + Send + 'static,
) -> Result<thread::JoinHandle<T>, ServeError> {
    admit(name).map_err(|e| ServeError::Thread(name, e))?;
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
        use epics_base_rs::runtime::log::ErrlogCapture;

        let mut errlog = ErrlogCapture::start();
        let mut seen = || -> Vec<String> {
            errlog
                .take()
                .iter()
                .filter(|l| l.starts_with("CA server: UDP search on port "))
                .map(|l| l.trim_end().to_string())
                .collect()
        };

        udp::announce_requested_ephemeral((5064, 5064), (5064, 5064), 1);
        assert!(seen().is_empty(), "a configured port is silent");

        let serving = BlockingCaServing::start(
            0,
            0,
            Arc::new(PvDatabase::new()),
            epics_base_rs::server::access_security::new_acf_cell(None),
            Arc::new(ServerStats::default()),
        )
        .expect("bind an ephemeral server");
        let lines = seen();
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
        use std::time::Duration;

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

    /// A serving thread the target will not create, at each place one can be
    /// refused — a later interface's included — fails the start with nothing
    /// left serving: C's `epicsThreadMustCreate` never leaves part of the set
    /// running (`caservertask.c:716-758`).
    #[test]
    fn a_refused_serving_thread_leaves_no_socket_open() {
        use super::*;
        use std::net::{TcpStream, UdpSocket};

        let beacon_sink = UdpSocket::bind("127.0.0.1:0").unwrap();
        // SAFETY: nextest runs this test alone in its process, before the
        // memoized `addr_list::from_env` first reads these.
        unsafe {
            std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1 127.0.0.2");
            std::env::set_var(
                "EPICS_CAS_BEACON_ADDR_LIST",
                beacon_sink.local_addr().unwrap().to_string(),
            );
            std::env::set_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "NO");
        }
        let db = Arc::new(PvDatabase::new());
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);

        let mut refused = Vec::new();
        let (serving, threads) = loop {
            let sockets = BlockingRsrvSockets::bind(0, 0).expect("bind both interfaces");
            assert_eq!(sockets.listeners.len(), 2, "two interfaces");
            let tcp: Vec<SocketAddr> = sockets
                .listeners
                .iter()
                .map(|l| l.local_addr().unwrap())
                .collect();
            let udp: Vec<SocketAddr> = sockets
                .responders
                .iter()
                .flat_map(|r| std::iter::once(&r.socket).chain(&r.bcast))
                .map(|s| s.local_addr().unwrap())
                .collect();
            // TCP and UDP per interface, UDP2 per broadcast socket, beacon.
            let threads = tcp.len() + udp.len() + 1;

            let refuse_at = refused.len();
            let mut created = 0;
            let started = BlockingCaServing::serve_with(
                sockets,
                db.clone(),
                acf.clone(),
                Arc::new(ServerStats::default()),
                &mut |_| {
                    if created == refuse_at {
                        return Err(io::ErrorKind::OutOfMemory.into());
                    }
                    created += 1;
                    Ok(())
                },
            );
            let name = match started {
                Ok(serving) => break (serving, threads),
                Err(ServeError::Thread(name, _)) => name,
                Err(e) => panic!("not a thread refusal: {e}"),
            };
            for addr in &tcp {
                assert_eq!(
                    TcpStream::connect(addr).map_err(|e| e.kind()).err(),
                    Some(io::ErrorKind::ConnectionRefused),
                    "{name} #{refuse_at} refused: {addr} still accepts"
                );
            }
            for addr in &udp {
                if let Err(e) = UdpSocket::bind(addr) {
                    panic!("{name} #{refuse_at} refused: {addr} still held: {e}");
                }
            }
            refused.push(name);
        };
        assert_eq!(
            refused.len(),
            threads,
            "each thread refused once: {refused:?}"
        );

        serving.shutdown();
        serving.join().expect("the unrefused start serves");
    }

    /// A client that connects or searches while the serving starts is served
    /// once every thread runs, and never by a start that fails.
    #[test]
    fn a_client_during_the_start_is_served_only_by_a_start_that_succeeds() {
        use super::*;
        use crate::protocol::{
            CA_MINOR_VERSION, CA_PROTO_ECHO, CA_PROTO_SEARCH, CA_PROTO_VERSION, CaHeader,
            pad_string,
        };
        use epics_base_rs::types::EpicsValue;
        use fact_budget::FACT_BUDGET;
        use std::io::{Read, Write};
        use std::net::{TcpStream, UdpSocket};
        use std::time::Duration;

        const PV: &str = "SERVING:DURING:START";

        /// Whether the server answers an ECHO on `c` before its read timeout.
        fn echoes(c: &mut TcpStream) -> bool {
            if c.write_all(&CaHeader::new(CA_PROTO_ECHO).to_bytes())
                .is_err()
            {
                return false;
            }
            let mut frame = [0u8; CaHeader::SIZE];
            loop {
                if c.read_exact(&mut frame).is_err() {
                    return false;
                }
                let hdr = CaHeader::from_bytes(&frame).unwrap();
                if hdr.cmmd == CA_PROTO_ECHO {
                    return true;
                }
                let mut payload = vec![0u8; hdr.postsize.into()];
                if c.read_exact(&mut payload).is_err() {
                    return false;
                }
            }
        }

        /// Whether a search for `PV` sent to `responder` draws a reply within
        /// `wait`.
        fn answers(responder: SocketAddr, wait: Duration) -> bool {
            let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
            probe.set_read_timeout(Some(wait)).unwrap();
            let mut version = CaHeader::new(CA_PROTO_VERSION);
            version.count = CA_MINOR_VERSION;
            let name = pad_string(PV);
            let mut search = CaHeader::new(CA_PROTO_SEARCH);
            search.data_type = 5;
            search
                .set_payload_size(name.len(), CA_MINOR_VERSION as u32, CA_MINOR_VERSION)
                .unwrap();
            let dg = [&version.to_bytes()[..], &search.to_bytes(), &name].concat();
            probe.send_to(&dg, responder).unwrap();
            probe.recv_from(&mut [0u8; 1024]).is_ok()
        }

        let beacon_sink = UdpSocket::bind("127.0.0.1:0").unwrap();
        // SAFETY: nextest runs this test alone in its process, before the
        // memoized `addr_list::from_env` first reads these.
        unsafe {
            std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1");
            std::env::set_var(
                "EPICS_CAS_BEACON_ADDR_LIST",
                beacon_sink.local_addr().unwrap().to_string(),
            );
            std::env::set_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "NO");
        }
        let db = Arc::new(PvDatabase::new());
        epics_base_rs::runtime::task::block_on_sync(db.add_pv(PV, EpicsValue::Double(1.0)))
            .expect("no runtime on the test thread")
            .expect("add_pv");
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);

        // The client comes as the beacon thread is about to be created, every
        // other thread started; `refuse` decides whether it is.
        let at = "CAS-beacon";
        for refuse in [true, false] {
            let sockets = BlockingRsrvSockets::bind(0, 0).expect("bind");
            let tcp = sockets.listeners[0].local_addr().unwrap();
            let udp = sockets.responders[0].socket.local_addr().unwrap();
            let mut client = None;
            let started = BlockingCaServing::serve_with(
                sockets,
                db.clone(),
                acf.clone(),
                Arc::new(ServerStats::default()),
                &mut |name| {
                    if name != at {
                        return Ok(());
                    }
                    let mut c = TcpStream::connect(tcp).unwrap();
                    // absence-window: a held server sends nothing, so no reply
                    // can mark the end of the hold.
                    c.set_read_timeout(Some(Duration::from_millis(500)))
                        .unwrap();
                    let mut version = CaHeader::new(CA_PROTO_VERSION);
                    version.count = CA_MINOR_VERSION;
                    c.write_all(&version.to_bytes()).unwrap();
                    assert!(!echoes(&mut c), "served while {at} was not yet started");
                    // absence-window: as for the circuit above.
                    assert!(
                        !answers(udp, Duration::from_millis(500)),
                        "searched while {at} was not yet started"
                    );
                    c.set_read_timeout(Some(FACT_BUDGET)).unwrap();
                    client = Some(c);
                    if refuse {
                        Err(io::ErrorKind::OutOfMemory.into())
                    } else {
                        Ok(())
                    }
                },
            );
            let mut c = client.expect("the client connected");
            match started {
                Err(ServeError::Thread(name, _)) if refuse => {
                    assert_eq!(name, at);
                    // Closing the listener resets the client still waiting
                    // in it; a read that times out found it held open.
                    let read = c.read(&mut [0u8; 1]);
                    assert!(
                        matches!(&read, Ok(0))
                            || matches!(&read, Err(e) if e.kind() == io::ErrorKind::ConnectionReset),
                        "not closed after the start failed at {at}: {read:?}"
                    );
                    if let Err(e) = UdpSocket::bind(udp) {
                        panic!("{udp} still held after the start failed at {at}: {e}");
                    }
                }
                Ok(serving) if !refuse => {
                    assert!(echoes(&mut c), "not served once every thread runs");
                    assert!(
                        answers(udp, FACT_BUDGET),
                        "not searched once every thread runs"
                    );
                    serving.shutdown();
                    serving.join().expect("the serving ends");
                }
                Ok(_) => panic!("{at} was refused, yet the start succeeded"),
                Err(e) => panic!("{at}: {e}"),
            }
        }
    }

    /// A stop wakes the accept loop of every interface, each back in
    /// `accept()` after serving a client.
    #[test]
    fn a_stop_wakes_the_accept_loop_of_every_interface() {
        use super::*;
        use crate::protocol::CaHeader;
        use fact_budget::FACT_BUDGET;
        use std::io::Read;
        use std::net::{TcpStream, UdpSocket};

        let beacon_sink = UdpSocket::bind("127.0.0.1:0").unwrap();
        // SAFETY: nextest runs this test alone in its process, before the
        // memoized `addr_list::from_env` first reads these.
        unsafe {
            std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1 127.0.0.2");
            std::env::set_var(
                "EPICS_CAS_BEACON_ADDR_LIST",
                beacon_sink.local_addr().unwrap().to_string(),
            );
            std::env::set_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "NO");
        }
        let sockets = BlockingRsrvSockets::bind(0, 0).expect("bind both interfaces");
        let tcp: Vec<SocketAddr> = sockets
            .listeners
            .iter()
            .map(|l| l.local_addr().unwrap())
            .collect();
        assert_eq!(tcp.len(), 2, "two interfaces");
        let serving = BlockingCaServing::serve_with(
            sockets,
            Arc::new(PvDatabase::new()),
            epics_base_rs::server::access_security::new_acf_cell(None),
            Arc::new(ServerStats::default()),
            &mut |_| Ok(()),
        )
        .expect("serve");
        // A greeting is sent by a client thread its listener's loop accepted.
        let _clients: Vec<TcpStream> = tcp
            .iter()
            .map(|addr| {
                let mut c = TcpStream::connect(addr).unwrap();
                c.set_read_timeout(Some(FACT_BUDGET)).unwrap();
                c.read_exact(&mut [0u8; CaHeader::SIZE])
                    .unwrap_or_else(|e| panic!("{addr} not served: {e}"));
                c
            })
            .collect();

        let (done, ended) = std::sync::mpsc::channel();
        thread::spawn(move || {
            serving.shutdown();
            let _ = done.send(serving.join().is_ok());
        });
        assert_eq!(
            ended.recv_timeout(FACT_BUDGET),
            Ok(true),
            "an accept loop was left parked"
        );
    }

    /// A start that fails returns while clients fill the backlog of the
    /// listener it never accepted on.
    #[test]
    fn a_failed_start_returns_with_its_backlog_full() {
        use super::*;
        use std::net::{TcpStream, UdpSocket};
        use std::time::Duration;

        let beacon_sink = UdpSocket::bind("127.0.0.1:0").unwrap();
        // SAFETY: nextest runs this test alone in its process, before the
        // memoized `addr_list::from_env` first reads these.
        unsafe {
            std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1");
            std::env::set_var(
                "EPICS_CAS_BEACON_ADDR_LIST",
                beacon_sink.local_addr().unwrap().to_string(),
            );
            std::env::set_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "NO");
        }
        let sockets = BlockingRsrvSockets::bind(0, 0).expect("bind");
        let tcp = sockets.listeners[0].local_addr().unwrap();
        // Up to the first connect the full backlog leaves unanswered.
        let waiting: Vec<TcpStream> = std::iter::from_fn(|| {
            // absence-window: a full backlog drops the SYN, so no reply can
            // mark it full.
            TcpStream::connect_timeout(&tcp, Duration::from_millis(200)).ok()
        })
        .take(4096)
        .collect();
        assert!(waiting.len() < 4096, "the backlog never filled");

        let (done, ended) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let started = BlockingCaServing::serve_with(
                sockets,
                Arc::new(PvDatabase::new()),
                epics_base_rs::server::access_security::new_acf_cell(None),
                Arc::new(ServerStats::default()),
                &mut |name| match name {
                    "CAS-beacon" => Err(io::ErrorKind::OutOfMemory.into()),
                    _ => Ok(()),
                },
            );
            let _ = done.send(matches!(started, Err(ServeError::Thread("CAS-beacon", _))));
        });
        assert_eq!(
            ended.recv_timeout(fact_budget::FACT_BUDGET),
            Ok(true),
            "the failed start did not return ({} clients waiting)",
            waiting.len()
        );
    }

    /// A start that fails leaves `casr` with no server to report and the
    /// uptime unstarted, so a start that then succeeds counts from itself.
    #[test]
    fn a_failed_start_leaves_casr_and_the_uptime_untouched() {
        use super::*;
        use std::net::UdpSocket;

        let beacon_sink = UdpSocket::bind("127.0.0.1:0").unwrap();
        // SAFETY: nextest runs this test alone in its process, before the
        // memoized `addr_list::from_env` first reads these.
        unsafe {
            std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1");
            std::env::set_var(
                "EPICS_CAS_BEACON_ADDR_LIST",
                beacon_sink.local_addr().unwrap().to_string(),
            );
            std::env::set_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "NO");
        }
        let db = Arc::new(PvDatabase::new());
        let acf = epics_base_rs::server::access_security::new_acf_cell(None);
        let stats = Arc::new(ServerStats::default());

        let refused = BlockingCaServing::serve_with(
            BlockingRsrvSockets::bind(0, 0).expect("bind"),
            db.clone(),
            acf.clone(),
            stats.clone(),
            &mut |name| match name {
                "CAS-beacon" => Err(io::ErrorKind::OutOfMemory.into()),
                _ => Ok(()),
            },
        );
        assert!(matches!(refused, Err(ServeError::Thread("CAS-beacon", _))));
        assert_eq!(
            iocsh::casr_report_lines(1),
            Vec::<String>::new(),
            "casr reports a start that failed"
        );

        let failed_at = std::time::Instant::now();
        let serving = BlockingCaServing::serve(
            BlockingRsrvSockets::bind(0, 0).expect("bind"),
            db,
            acf,
            stats.clone(),
        )
        .expect("the second start runs");
        assert!(
            stats.uptime() < failed_at.elapsed(),
            "uptime counts from the start that failed"
        );
        let port = serving.tcp_port().to_string();
        assert!(
            iocsh::casr_report_lines(1)
                .iter()
                .any(|l| l.contains(&port)),
            "casr does not report the start that runs"
        );
        serving.shutdown();
        serving.join().expect("the serving ends");
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
