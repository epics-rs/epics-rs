//! The server's TCP listener set, pvxs `ServIface` (`serverconn.cpp:405-466`,
//! `server.cpp:407-487`): one listener per expanded interface, bound with
//! std sockets so every driver — tokio, blocking, reactor — binds the same set.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, ToSocketAddrs};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use epics_libcom_rs::runtime::socket::AcceptWaker;
use tracing::{error, warn};

use super::config::PvaServerConfig;
use super::search_engine::ServerInterfaces;
use crate::error::{PvaError, PvaResult};
use crate::pvxs_log::{self, Level, TCP_SETUP};
use crate::sock_attach::can_ipv6;

/// Resolve the TCP listener bind addresses from the server interface
/// list.
///
/// `EPICS_PVAS_INTF_ADDR_LIST` (`interfaces`) is the address set the
/// server binds to; when empty it falls back to `bind_ip`, which pvxs
/// does not have: its empty list is `0.0.0.0` (`config.cpp:166-168`).
/// One listener is bound per specific address, in list order, so each
/// interface is constrained at the kernel. A listed entry with no port
/// takes `port`, the server's resolved TCP port, as pvxs writes it into
/// every interface string (`config.cpp:166-168`). A multicast entry is a
/// search group, never a TCP listener (`server.cpp:413-418`). Any wildcard
/// entry, of either family, becomes the wildcard listeners after the
/// specific ones, on port 0 whatever port the entry named
/// (`server.cpp:441-458`): with `can_ipv6`, `[::]` alone on Linux, whose
/// dual-stack socket would collide with a `0.0.0.0` one, and elsewhere
/// `0.0.0.0` then `[::]`, the order Darwin requires; without it `0.0.0.0`.
/// With a specific listener already on that port the wildcard fails to
/// bind, and pvxs fails the same way.
pub(crate) fn tcp_bind_addresses(
    interfaces: &[SocketAddr],
    bind_ip: IpAddr,
    port: u16,
    can_ipv6: bool,
) -> Vec<SocketAddr> {
    let mut addrs: Vec<SocketAddr> = interfaces
        .iter()
        .filter(|a| !a.ip().is_multicast() && !a.ip().is_unspecified())
        .map(|a| match a.port() {
            0 => SocketAddr::new(a.ip(), port),
            _ => *a,
        })
        .collect();
    let bind_any = match interfaces {
        [] => bind_ip.is_unspecified(),
        _ => interfaces.iter().any(|a| a.ip().is_unspecified()),
    };
    if bind_any {
        let v4 = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0));
        let v6 = SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0));
        match (can_ipv6, cfg!(target_os = "linux")) {
            (true, true) => addrs.push(v6),
            (true, false) => addrs.extend([v4, v6]),
            (false, _) => addrs.push(v4),
        }
    }
    if addrs.is_empty() {
        addrs.push(SocketAddr::new(bind_ip, 0));
    }
    addrs
}

/// Bind one listener per address, pvxs `server.cpp:476-486`: an address
/// keeps its own port and only a 0 takes `port`, which becomes the first
/// listener's bound port. Returns the listeners and that port.
///
/// deliberate deviation: when every requested port is 0, a held number
/// (`port_held`) on a later interface releases the whole set and binds it
/// again on a fresh ephemeral number. pvxs binds interface 1 on 0 and the
/// rest on that number (`server.cpp:476-486`), so a number the kernel handed
/// out but another socket already holds on a later address fails the start;
/// with nothing asked for there is nothing to refuse. A non-zero request, any
/// other error, or a collision with an address of the set itself (a
/// duplicate, or on Linux a wildcard beside a specific entry) behaves as
/// pvxs: it fails. Windows and Darwin bind a wildcard beside a specific
/// entry, as pvxs does there.
pub(crate) fn bind_serv_ifaces(
    addrs: impl IntoIterator<Item = SocketAddr>,
    port: u16,
) -> PvaResult<(Vec<TcpListener>, u16)> {
    bind_serv_ifaces_with(addrs, port, &mut |_| {})
}

/// [`bind_serv_ifaces`], calling `after_first` with the number the first
/// interface bound before the others try it.
pub(crate) fn bind_serv_ifaces_with(
    addrs: impl IntoIterator<Item = SocketAddr>,
    port: u16,
    after_first: &mut dyn FnMut(u16),
) -> PvaResult<(Vec<TcpListener>, u16)> {
    const EPHEMERAL_TRIES: usize = 16;
    let addrs: Vec<SocketAddr> = addrs.into_iter().collect();
    let all_ephemeral = port == 0 && addrs.iter().all(|a| a.port() == 0);
    let mut tries = 1;
    loop {
        match bind_serv_ifaces_once(&addrs, port, after_first) {
            Err((at, PvaError::Io(e)))
                if all_ephemeral
                    && epics_base_rs::net::port_held(&e)
                    && tries < EPHEMERAL_TRIES
                    && !addrs[..at].iter().any(|a| addresses_overlap(a, &addrs[at])) =>
            {
                warn!("ephemeral server port is held on a later interface; choosing another");
                tries += 1;
            }
            Err((_, e)) => return Err(e),
            Ok(bound) => return Ok(bound),
        }
    }
}

/// Whether a socket on `a` and one on `b`, on one port, cannot both listen:
/// the same address, or either one a wildcard.
fn addresses_overlap(a: &SocketAddr, b: &SocketAddr) -> bool {
    a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified()
}

/// One pass over the set; on failure, the index of the address that failed.
fn bind_serv_ifaces_once(
    addrs: &[SocketAddr],
    mut port: u16,
    after_first: &mut dyn FnMut(u16),
) -> Result<(Vec<TcpListener>, u16), (usize, PvaError)> {
    let mut listeners = Vec::new();
    for (at, &(mut addr)) in addrs.iter().enumerate() {
        if addr.port() == 0 {
            addr.set_port(port);
        }
        let first = listeners.is_empty();
        let (listener, bound) = bind_serv_iface(addr, first).map_err(|e| (at, e))?;
        if first {
            port = bound;
            after_first(port);
        }
        listeners.push(listener);
    }
    Ok((listeners, port))
}

/// Bind one server interface as pvxs `ServIface::ServIface` does
/// (`serverconn.cpp:405-466`): with `fallback` (the first interface), a taken
/// or forbidden port falls back once to an OS-picked one; a bind that is not
/// recovered logs pvxs's ERR line before failing.
fn bind_serv_iface(addr: SocketAddr, mut fallback: bool) -> PvaResult<(TcpListener, u16)> {
    let mut bind_addr = addr;
    loop {
        match TcpListener::bind(bind_addr) {
            Ok(listener) => {
                // On RTEMS `bind` succeeds and `local_addr` fails (libc omits
                // the BSD `sin_len` byte); the two must stay distinguishable.
                let name = listener.local_addr().map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!("bind() succeeded; local_addr() on the listener failed: {e}"),
                    )
                })?;
                if addr.port() != 0 && name.port() != addr.port() {
                    pvxs_log::log(
                        TCP_SETUP,
                        Level::Warn,
                        format_args!(
                            "Server unable to bind port {}, falling back to {name}",
                            addr.port()
                        ),
                    );
                }
                return Ok((listener, name.port()));
            }
            Err(e) if fallback && epics_base_rs::net::bind_falls_back(&e) => {
                pvxs_log::log(
                    TCP_SETUP,
                    Level::Debug,
                    format_args!(
                        "Address {} in use or not permitted: {}",
                        crate::config::env::sock_addr_text(bind_addr),
                        epics_base_rs::net::sock_err_string(&e)
                    ),
                );
                bind_addr.set_port(0);
                fallback = false;
            }
            Err(e) => {
                // pvxs's suffix tracks the flag, so an interface bound
                // without a fallback says "after fallback" too.
                pvxs_log::log(
                    TCP_SETUP,
                    Level::Err,
                    format_args!(
                        "Bind to {} fails{} with {}",
                        crate::config::env::sock_addr_text(bind_addr),
                        if fallback { "" } else { " after fallback" },
                        e.raw_os_error().unwrap_or(0)
                    ),
                );
                return Err(PvaError::Io(e));
            }
        }
    }
}

/// [`ServListeners::state`]: no accept loop yet, accept loops running, and
/// stopped for good.
const IDLE: u8 = 0;
const SERVING: u8 = 1;
const STOPPED: u8 = 2;

/// A driver's bound listeners, one accept loop each, as rsrv runs one
/// `CAS-TCP` per interface.
pub(crate) struct ServListeners {
    listeners: Vec<TcpListener>,
    /// One per listener, opened with it, so [`Self::stop`] needs no
    /// descriptor to wake a parked accept.
    wakers: Vec<AcceptWaker>,
    port: u16,
    /// [`IDLE`], [`SERVING`] or [`STOPPED`]; only [`Self::stop`] out of
    /// `SERVING` dials a listener.
    state: AtomicU8,
    /// [`Self::serve`] is refused the further accept thread at this index.
    #[cfg(test)]
    pub(crate) refuse_thread: Option<usize>,
}

impl ServListeners {
    /// The listeners for `config`'s expanded interface list; `default` is
    /// the listener when that list is empty and the port an entry without
    /// one takes, the tokio runtime's `bind_ip` and `tcp_port`.
    pub(crate) fn bind(default: impl ToSocketAddrs, config: &PvaServerConfig) -> io::Result<Self> {
        let default = default
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no bind address"))?;
        let interfaces = ServerInterfaces::resolve(
            &config.interfaces,
            &config.beacon_destinations,
            config.auto_beacon,
        )?;
        let addrs = tcp_bind_addresses(&interfaces.addrs, default.ip(), default.port(), can_ipv6());
        let (listeners, port) = bind_serv_ifaces(addrs, default.port()).map_err(|e| match e {
            PvaError::Io(e) => e,
            e => io::Error::other(e),
        })?;
        let wakers = listeners
            .iter()
            .map(|l| AcceptWaker::new(l.local_addr()?))
            .collect::<io::Result<_>>()?;
        Ok(Self {
            listeners,
            wakers,
            port,
            state: AtomicU8::new(IDLE),
            #[cfg(test)]
            refuse_thread: None,
        })
    }

    /// The port every listener without an explicit one bound.
    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// The first listener's address.
    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listeners[0].local_addr()
    }

    /// Run `accept` on every listener until each returns: the first on the
    /// calling thread, each further one on a thread of its own. `started`
    /// runs once every thread exists, and the listeners accept only if it
    /// returns `true` and [`Self::stop`] has not run. A thread that cannot be
    /// created is the `Err`, and then no listener has accepted: pvxs's
    /// `Server` throws on a listener it cannot set up
    /// (`serverconn.cpp:426-449`). `accept` returns once [`Self::stopped`].
    pub(crate) fn serve(
        &self,
        accept: impl Fn(&TcpListener) + Sync,
        started: impl FnOnce() -> bool,
    ) -> io::Result<()> {
        use epics_base_rs::runtime::task::{StackSizeClass, spawn_dedicated_thread_scoped};
        let (first, rest) = self.listeners.split_first().expect("bind leaves one");
        let all_created = OnceLock::new();
        std::thread::scope(|scope| {
            for (thread, listener) in rest.iter().enumerate() {
                let (accept, all_created) = (&accept, &all_created);
                let spawned = match self.refusal(thread) {
                    Some(refused) => Err(refused),
                    None => spawn_dedicated_thread_scoped(
                        scope,
                        "PVAS-TCP".to_string(),
                        super::blocking::PVA_SERVER_PRIORITY,
                        StackSizeClass::Medium,
                        move || {
                            if *all_created.wait() {
                                accept(listener);
                            }
                        },
                    )
                    .map(drop),
                };
                if let Err(e) = spawned {
                    error!(addr = ?listener.local_addr(), "PVA accept thread not started: {e}");
                    let _ = all_created.set(false);
                    return Err(e);
                }
            }
            let run = started()
                && self
                    .state
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |s| {
                        (s != STOPPED).then_some(SERVING)
                    })
                    .is_ok();
            let _ = all_created.set(run);
            if run {
                accept(first);
            }
            Ok(())
        })
    }

    #[cfg(not(test))]
    fn refusal(&self, _thread: usize) -> Option<io::Error> {
        None
    }

    /// A test's stand-in for a target out of threads.
    #[cfg(test)]
    fn refusal(&self, thread: usize) -> Option<io::Error> {
        (self.refuse_thread == Some(thread)).then(|| io::ErrorKind::WouldBlock.into())
    }

    /// End accepting for good, waking any accept parked in [`Self::serve`]
    /// with its listener's waker. Only a served listener is dialled, and the
    /// dial never waits: one that meets a full backlog finds that listener's
    /// accept loop not parked, and it sees [`Self::stopped`] at its next
    /// accept.
    pub(crate) fn stop(&self) {
        if self.state.swap(STOPPED, Ordering::SeqCst) == SERVING {
            for waker in &self.wakers {
                let _ = waker.wake();
            }
        }
    }

    /// Whether [`Self::stop`] has run: an accept loop's exit condition.
    pub(crate) fn stopped(&self) -> bool {
        self.state.load(Ordering::SeqCst) == STOPPED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addrs(list: &[&str]) -> Vec<SocketAddr> {
        list.iter().map(|a| a.parse().unwrap()).collect()
    }

    /// Binds with `held` called on each first-picked number; returns the
    /// result and every number tried.
    fn bind_counting(
        list: &[&str],
        port: u16,
        mut held: impl FnMut(usize, u16),
    ) -> (PvaResult<(Vec<TcpListener>, u16)>, Vec<u16>) {
        let mut tried = Vec::new();
        let r = bind_serv_ifaces_with(addrs(list), port, &mut |p| {
            held(tried.len(), p);
            tried.push(p);
        });
        (r, tried)
    }

    fn io_kind(r: PvaResult<(Vec<TcpListener>, u16)>) -> io::ErrorKind {
        match r {
            Err(PvaError::Io(e)) => e.kind(),
            other => panic!("{:?}", other.map(|(_, p)| p)),
        }
    }

    #[test]
    fn an_ephemeral_number_held_on_a_later_interface_is_chosen_again() {
        let mut held = Vec::new();
        let mut held_ports = Vec::new();
        let (r, tried) = bind_counting(&["127.0.0.1:0", "127.0.0.2:0"], 0, |n, port| {
            if n < 2 {
                // A number the kernel hands out twice is already held.
                if let Ok(l) = TcpListener::bind(("127.0.0.2", port)) {
                    held.push(l);
                }
                held_ports.push(port);
            }
        });
        let (listeners, port) = r.expect("bind");
        assert!(tried.len() >= 3, "{tried:?}");
        assert_eq!(*tried.last().unwrap(), port);
        assert!(!held_ports.contains(&port), "{held_ports:?} {port}");
        let bound: Vec<_> = listeners.iter().map(|l| l.local_addr().unwrap()).collect();
        assert_eq!(
            bound,
            ["127.0.0.1", "127.0.0.2"].map(|a| SocketAddr::new(a.parse().unwrap(), port))
        );
    }

    /// A requested number is never replaced: the later interface fails the
    /// start on the first try, as in pvxs.
    #[test]
    fn a_requested_number_held_on_a_later_interface_fails_the_start() {
        let held = TcpListener::bind("127.0.0.2:0").unwrap();
        let port = held.local_addr().unwrap().port();
        let list = [format!("127.0.0.1:{port}"), format!("127.0.0.2:{port}")];
        let (r, tried) = bind_counting(&[&list[0], &list[1]], port, |_, _| {});
        assert_eq!(io_kind(r), io::ErrorKind::AddrInUse);
        assert_eq!(tried.len(), 1, "{tried:?}");
    }

    /// Only a collision with a stranger is retried: anything else a later
    /// interface reports ends the start at once.
    #[test]
    fn another_error_on_a_later_interface_is_not_retried() {
        // 192.0.2.1 (TEST-NET-1) is on no interface of this host.
        let (r, tried) = bind_counting(&["127.0.0.1:0", "192.0.2.1:0"], 0, |_, _| {});
        assert_eq!(io_kind(r), io::ErrorKind::AddrNotAvailable);
        assert_eq!(tried.len(), 1, "{tried:?}");
    }

    /// A wildcard beside a specific entry collides with the set's own
    /// listener, which no other number cures.
    // Linux only; on Windows and Darwin the wildcard binds beside it, as
    // pvxs's does (`wildcard_entry_binds_after_a_specific_one`).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_wildcard_beside_a_specific_entry_fails_on_the_first_try() {
        let (r, tried) = bind_counting(&["127.0.0.1:0", "0.0.0.0:0"], 0, |_, _| {});
        assert_eq!(io_kind(r), io::ErrorKind::AddrInUse);
        assert_eq!(tried.len(), 1, "{tried:?}");
    }

    /// The unconfigured server takes both families on one port, as
    /// softIocPVX's `[::]` listener does: `EPICS_PVA_NAME_SERVERS=[::1]:port`
    /// was refused while rs bound `0.0.0.0`.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_default_listener_takes_v4_and_v6_peers() {
        use std::net::TcpStream;

        if !can_ipv6() {
            return;
        }
        let listeners = ServListeners::bind(("0.0.0.0", 0), &PvaServerConfig::default()).unwrap();
        assert_eq!(listeners.listeners.len(), 1);
        for ip in [
            IpAddr::from(Ipv4Addr::LOCALHOST),
            Ipv6Addr::LOCALHOST.into(),
        ] {
            TcpStream::connect((ip, listeners.port())).unwrap();
        }
    }
}
