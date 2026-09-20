//! Which of the two native drivers an image runs, chosen once.
//!
//! [`blocking`](super::blocking) and [`reactor`](super::reactor) expose the
//! same runtime surface with the same signatures — every method on
//! [`PvaServerDriver`] is a one-line forward on both — and differ only in what
//! a connection costs. The blocking driver takes a thread per connection
//! (1,589,554 B measured on `armv7-rtems-eabihf`); the reactor driver takes
//! none, and its test `the_thread_count_does_not_grow_with_the_client_count`
//! measures 0 added threads for 8 concurrent clients where the blocking
//! driver adds 24.
//!
//! Two identical surfaces are a trait, not a `match` at every call site: with
//! [`bind`] returning `Arc<dyn PvaServerDriver>` the choice is made in one
//! place and nothing downstream can tell — or come to depend on — which it
//! got. That matters more than the line count, because the consumer is a
//! target IOC whose accept thread, status PV and shutdown path would
//! otherwise each carry their own copy of the fork.
//!
//! # The default is the driver the measurement chose
//!
//! [`DriverKind::Reactor`], on the bring-up box's numbers. One tree, two
//! `armv7-rtems-eabihf` images differing only in the baked boot line: a
//! connection costs 38,483 B of target heap on the reactor driver against
//! 1,559,487 B on the blocking one, whose pooled worker set keeps its
//! high-water stacks — of the 36.5 MB that 24 concurrent connections added,
//! the blocking driver still held 36.5 MB two minutes after the last one
//! closed, the reactor driver 18.3 KB. Thirty serialised connect-and-get
//! round-trips are a wash (0.97 s against 1.00 s), and under 24 simultaneous
//! connections the median time to the server's unprompted
//! CONNECTION_VALIDATION is 81 ms against 131 ms. Neither refused a
//! connection.
//!
//! `x86_64-wrs-vxworks` separates them further, and there the difference is a
//! ceiling rather than a cost. One `.vxe`, two runs differing only in the
//! `EPICS_PVA_RS_DRIVER` the kernel shell `putenv`s before `rtpSp`: the
//! blocking driver serves 18 held connections and refuses from the 19th on,
//! the worker pool naming its own budget — each connection's thread set wants
//! 6144 KiB, and 156 of 160 MiB are already reserved. The reactor driver held
//! 64 with no refusal. `MEM_USED` (mimalloc's committed bytes, the one heap
//! figure VxWorks answers) rises 17.12 MB → 42.80 MB across 14 of those
//! blocking connections, 1.83 MB each, and does not move at all from 0 to 64
//! on the reactor driver. Thirty serialised round-trips stay 7.2 s either way
//! while both are still serving. The backend under the reactor driver there is
//! `select` — `kqueue` is selected only on RTEMS — so this pair measures the
//! driver alone.
//!
//! Whole-burst wall clocks are deliberately not quoted. qemu's SLIRP
//! `hostfwd` drops about one connection in a few dozen under a concurrent
//! burst — the host `connect` succeeds against qemu's own listener while
//! nothing reaches the guest, so the target never accepts it, never counts it
//! in `PVA_CONN_CNT` and never logs it — and `pvxget` absorbs that as a 6 s or
//! 16 s retry. A burst time measured through that rig is the emulator's
//! number, not the driver's; the figures above are per-connection, over the
//! connections the target actually accepted.
//!
//! `EPICS_PVA_RS_DRIVER=blocking` selects the other; the `_RS_` in the name is
//! this workspace's mark for a variable C EPICS does not have
//! (`EPICS_CA_RS_CHAOS`, `EPICS_BASE_RS_MACRO_ENV_TEST`).
//!
//! An unreadable value is refused rather than defaulted. The reason is the
//! shape of the mistake: `EPICS_PVA_RS_DRIVER=blockign` on a boot command line
//! would otherwise boot the reactor driver and say nothing, and the operator
//! would be reading thread counts from the driver they thought they had turned
//! off. Case is not that mistake and is accepted.
//!
//! # This is also what gates `kqueue`
//!
//! [`ReactorPvaServer`] is the only production constructor of a
//! [`Poller`](epics_base_rs::runtime::readiness::Poller), and the poller is
//! what picks `kqueue` on RTEMS. A stock image therefore now runs that backend
//! on the target, and `EPICS_PVA_RS_DRIVER=blocking` is also the switch that
//! takes it back out.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;

use epics_base_rs::runtime::task::Reactor;

use super::blocking::BlockingPvaServer;
use super::config::PvaServerConfig;
use super::reactor::ReactorPvaServer;
use super::source::DynSource;
use crate::error::PvaResult;

/// The environment variable [`DriverKind::from_env`] reads.
pub const DRIVER_ENV: &str = "EPICS_PVA_RS_DRIVER";

/// The runtime surface both native drivers implement.
///
/// `bind` is absent on purpose: it is generic over the address and returns
/// `Self`, neither of which an object-safe trait can carry, and it is the one
/// call that has to know which driver it is building. [`bind`] is that call.
pub trait PvaServerDriver: Send + Sync {
    /// The actual bound address — the value to use when the configured port
    /// was 0.
    fn local_addr(&self) -> io::Result<SocketAddr>;

    /// The bound TCP port, answerable without touching the socket.
    fn tcp_port(&self) -> u16;

    /// The server GUID, stamped at `bind`.
    fn guid(&self) -> [u8; 12];

    /// Connections currently being served.
    fn active_connections(&self) -> usize;

    /// Run the accept loop on the calling thread until [`shutdown`](Self::shutdown).
    fn serve(&self, reactor: &Reactor);

    /// Run the UDP name-search responder on the calling thread.
    fn serve_udp_search(&self, socket: UdpSocket) -> PvaResult<()>;

    /// Stop the accept loop and end every live connection.
    fn shutdown(&self);
}

impl PvaServerDriver for BlockingPvaServer {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        BlockingPvaServer::local_addr(self)
    }

    fn tcp_port(&self) -> u16 {
        BlockingPvaServer::tcp_port(self)
    }

    fn guid(&self) -> [u8; 12] {
        BlockingPvaServer::guid(self)
    }

    fn active_connections(&self) -> usize {
        BlockingPvaServer::active_connections(self)
    }

    fn serve(&self, reactor: &Reactor) {
        BlockingPvaServer::serve(self, reactor)
    }

    fn serve_udp_search(&self, socket: UdpSocket) -> PvaResult<()> {
        BlockingPvaServer::serve_udp_search(self, socket)
    }

    fn shutdown(&self) {
        BlockingPvaServer::shutdown(self)
    }
}

impl PvaServerDriver for ReactorPvaServer {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        ReactorPvaServer::local_addr(self)
    }

    fn tcp_port(&self) -> u16 {
        ReactorPvaServer::tcp_port(self)
    }

    fn guid(&self) -> [u8; 12] {
        ReactorPvaServer::guid(self)
    }

    fn active_connections(&self) -> usize {
        ReactorPvaServer::active_connections(self)
    }

    fn serve(&self, reactor: &Reactor) {
        ReactorPvaServer::serve(self, reactor)
    }

    fn serve_udp_search(&self, socket: UdpSocket) -> PvaResult<()> {
        ReactorPvaServer::serve_udp_search(self, socket)
    }

    fn shutdown(&self) {
        ReactorPvaServer::shutdown(self)
    }
}

/// Which driver to build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DriverKind {
    /// One thread per connection.
    Blocking,
    /// One readiness poller for all connections. The default; see the module
    /// doc for the measurement that chose it.
    #[default]
    Reactor,
}

impl DriverKind {
    /// Read [`DRIVER_ENV`].
    ///
    /// `Err` carries the operator-facing message; the caller decides whether a
    /// bad value is fatal, and in an IOC it is.
    pub fn from_env() -> Result<Self, String> {
        Self::parse(std::env::var(DRIVER_ENV).ok().as_deref())
    }

    /// [`from_env`](Self::from_env) without the environment, so the rule is
    /// testable without a process-global write.
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        let Some(raw) = raw else {
            return Ok(Self::default());
        };
        match raw.trim().to_ascii_lowercase().as_str() {
            // Unset and set-to-empty mean the same thing: an unset variable on
            // a boot command line is often written as the empty assignment.
            "" => Ok(Self::default()),
            "blocking" => Ok(Self::Blocking),
            "reactor" => Ok(Self::Reactor),
            other => Err(format!(
                "{DRIVER_ENV}={other}: expected `blocking` or `reactor`"
            )),
        }
    }

    /// The name this kind answers to, for a startup line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocking => "blocking",
            Self::Reactor => "reactor",
        }
    }
}

/// Bind the accept socket with the selected driver.
///
/// The error is the driver's own: both bind a [`TcpListener`](std::net::TcpListener)
/// and both keep "bind failed" distinguishable from "local_addr failed", which
/// on RTEMS are different defects.
pub fn bind<A: ToSocketAddrs>(
    kind: DriverKind,
    addr: A,
    source: DynSource,
    config: PvaServerConfig,
) -> io::Result<Arc<dyn PvaServerDriver>> {
    Ok(match kind {
        DriverKind::Blocking => Arc::new(BlockingPvaServer::bind(addr, source, config)?),
        DriverKind::Reactor => Arc::new(ReactorPvaServer::bind(addr, source, config)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_native::blocking::tests::{isolated_config, test_source};
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn an_unset_variable_selects_the_measured_driver() {
        assert_eq!(DriverKind::parse(None), Ok(DriverKind::Reactor));
        assert_eq!(DriverKind::parse(Some("")), Ok(DriverKind::Reactor));
        assert_eq!(DriverKind::parse(Some("   ")), Ok(DriverKind::Reactor));
    }

    #[test]
    fn both_names_are_accepted_in_any_case() {
        for raw in ["reactor", "Reactor", "  REACTOR  "] {
            assert_eq!(
                DriverKind::parse(Some(raw)),
                Ok(DriverKind::Reactor),
                "{raw}"
            );
        }
        for raw in ["blocking", "Blocking", " BLOCKING "] {
            assert_eq!(
                DriverKind::parse(Some(raw)),
                Ok(DriverKind::Blocking),
                "{raw}"
            );
        }
    }

    /// A typo must not boot the other driver in silence — that is the whole
    /// reason this returns a `Result`.
    #[test]
    fn an_unreadable_value_is_refused_rather_than_defaulted() {
        let err = DriverKind::parse(Some("reactr")).expect_err("a typo is refused");
        assert!(err.contains(DRIVER_ENV), "{err}");
        assert!(
            err.contains("reactr"),
            "the message quotes what was set: {err}"
        );
        assert!(err.contains("blocking") && err.contains("reactor"), "{err}");
    }

    /// The point of the trait: one call site builds either driver and the
    /// caller holds one type.
    #[test]
    fn the_factory_builds_either_driver_behind_one_handle() {
        for kind in [DriverKind::Blocking, DriverKind::Reactor] {
            let server = bind(
                kind,
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                test_source(),
                isolated_config(),
            )
            .unwrap_or_else(|e| panic!("{kind:?} binds: {e}"));
            assert_ne!(server.tcp_port(), 0, "{kind:?} reports its bound port");
            assert_eq!(
                server.local_addr().expect("local_addr").port(),
                server.tcp_port(),
                "{kind:?} agrees with its own socket"
            );
            assert_eq!(server.active_connections(), 0, "{kind:?} starts idle");
            assert_ne!(server.guid(), [0u8; 12], "{kind:?} stamps a GUID");
            server.shutdown();
        }
    }
}
