//! Socket construction with pre-bind options: the one place in the workspace
//! that opens a socket, sets the address-reuse/broadcast options a protocol
//! needs, and only then binds or connects it.
//!
//! # Why this exists, and why here
//!
//! `socket2` — the obvious way to set an option on an unbound socket — does not
//! build for `armv7-rtems-eabihf` or the `*-wrs-vxworks*` triples (20 compile
//! errors on the former: `ip_mreqn`, `IovLen`, and friends are Linux shapes the
//! target's `libc` has no equivalent for). So every reactor-free driver that
//! needed a pre-bind option grew its own raw-`libc` copy of the same twenty
//! lines. At the time this module was written there were two, byte-for-byte
//! alike in everything but their error strings:
//!
//! * `epics-ca-rs::server::blocking::bind_udp_search_socket`
//! * `epics-pva-rs::server_native::blocking`'s UDP search responder
//!
//! and `asyn-rs`'s IP drivers were about to be the third. `epics-ca-rs` does not
//! depend on `epics-pva-rs` and must not, and `asyn-rs` depends on neither, so
//! there is exactly one crate all three can reach: this one. A primitive
//! promoted into any protocol crate is one the other two structurally cannot
//! call — the same reasoning that put `runtime::blocking_io` here, for the same
//! reason.
//!
//! The second reason is coverage. `epics-libcom-rs` is the first entry in
//! `CRATES` in both `scripts/rtems-check.sh` and `scripts/vxworks-check.sh`, so
//! target-only `unsafe` placed here is compiled for both triples by the gates
//! that already run. The same code in a crate outside those lists is compiled
//! by nothing.
//!
//! # Why the option must precede the bind
//!
//! `SO_REUSEPORT` is what lets several IOCs share one UDP port and have the
//! kernel fan each datagram out to all of them. The kernel only honours it on
//! an **unbound** socket, so `UdpSocket::bind()` followed by a `setsockopt` is
//! not a slower version of this — it is a version that silently does nothing.
//! That ordering constraint is the whole reason `std`'s constructors are not
//! enough and a raw `socket()`/`setsockopt()`/`bind()` sequence is.
//!
//! # The C authority for each branch
//!
//! Option selection follows EPICS base
//! `libcom/src/osi/os/default/osdSockAddrReuse.cpp`: the datagram-fanout helper
//! sets `SO_REUSEPORT` (where defined) *and then* `SO_REUSEADDR`; the
//! time-wait helper sets `SO_REUSEADDR` alone. Both `SO_REUSEPORT` constants
//! exist on the two embedded targets (`0x0200` on newlib/RTEMS and on VxWorks),
//! so neither takes the `#ifndef SO_REUSEPORT` fallback the C comment describes
//! for older systems.
//!
//! Connect behaviour follows asyn `drvAsynIPPort.c`, whose three branches this
//! module reproduces exactly:
//!
//! | target | connect | timeout honoured | C authority |
//! |---|---|---|---|
//! | hosted | non-blocking, then `poll(POLLOUT)` | yes | `:511`, `:523`, `:544` under `USE_POLL` |
//! | VxWorks | non-blocking, then `select()` via `FAKE_POLL` | yes | `:76`, `:139-164`, `:178` |
//! | RTEMS | **blocking** | **no** | `:71-72` — `__rtems__` takes `USE_SOCKTIMEOUT`, and both `setNonBlock` and the poll block sit inside `#ifdef USE_POLL` |
//!
//! The RTEMS row is not an omission. C genuinely has no connect timeout there;
//! it bounds the *transfer* instead, with `SO_RCVTIMEO`/`SO_SNDTIMEO`
//! (`:652-664`, `:778-790`). Honouring the deadline there anyway would be a
//! deviation invented by the port, so [`tcp_connect`] documents the difference
//! rather than papering over it.

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::time::Duration;

/// Options applied to a fresh socket *before* it is bound or connected.
///
/// A struct rather than three positional `bool`s because the call sites set
/// different subsets and a positional triple reads identically whichever two
/// are swapped.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SocketOptions {
    /// `SO_BROADCAST`. UDP only; asyn's `udp*` protocol suffix.
    pub broadcast: bool,
    /// `SO_REUSEADDR`.
    pub reuse_address: bool,
    /// `SO_REUSEPORT`, where the platform defines it.
    pub reuse_port: bool,
}

impl SocketOptions {
    /// The datagram-fanout pair: `SO_REUSEPORT` **and** `SO_REUSEADDR`, matching
    /// C `epicsSocketEnableAddressUseForDatagramFanout`, which sets both.
    pub const FANOUT: Self = Self {
        broadcast: false,
        reuse_address: true,
        reuse_port: true,
    };

    /// `SO_REUSEADDR` alone, matching C
    /// `epicsSocketEnableAddressReuseDuringTimeWaitState`. This is what a TCP
    /// listener gets; the fanout helper is `SOCK_DGRAM`-only.
    pub const REUSE_ADDRESS: Self = Self {
        broadcast: false,
        reuse_address: true,
        reuse_port: false,
    };
}

/// Open a UDP socket, apply `opts`, and bind it to `local`.
pub fn udp_socket(local: SocketAddr, opts: SocketOptions) -> io::Result<UdpSocket> {
    sys::udp_socket(local, opts)
}

/// Open a TCP socket, apply `opts`, bind it to `local`, and start listening.
pub fn tcp_listener(
    local: SocketAddr,
    opts: SocketOptions,
    backlog: i32,
) -> io::Result<TcpListener> {
    sys::tcp_listener(local, opts, backlog)
}

/// Open a TCP socket, apply `opts`, optionally bind it to `local`, and connect
/// it to `remote` within `timeout`.
///
/// # The timeout is not honoured on RTEMS
///
/// See the module header: C `drvAsynIPPort.c` takes `USE_SOCKTIMEOUT` on
/// `__rtems__`, which compiles out both the pre-connect `setNonBlock` and the
/// `poll(POLLOUT)` deadline, leaving a plain blocking `connect()`. This
/// reproduces that. On every other target the deadline is enforced, as it is in
/// C.
pub fn tcp_connect(
    remote: SocketAddr,
    local: Option<SocketAddr>,
    opts: SocketOptions,
    timeout: Duration,
) -> io::Result<TcpStream> {
    sys::tcp_connect(remote, local, opts, timeout)
}

#[cfg(not(epics_embedded_target))]
mod sys {
    //! Hosted: `socket2` already owns the pre-bind option surface and its
    //! `connect_timeout` is the `poll(POLLOUT)` shape C uses under `USE_POLL`.

    use super::SocketOptions;
    use std::io;
    use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
    use std::time::Duration;

    fn new_socket(
        addr_is_v6: bool,
        ty: socket2::Type,
        protocol: socket2::Protocol,
        opts: SocketOptions,
    ) -> io::Result<socket2::Socket> {
        let domain = if addr_is_v6 {
            socket2::Domain::IPV6
        } else {
            socket2::Domain::IPV4
        };
        let socket = socket2::Socket::new(domain, ty, Some(protocol))?;
        if opts.broadcast {
            socket.set_broadcast(true)?;
        }
        // C's fanout helper sets SO_REUSEPORT first, then SO_REUSEADDR; keep
        // that order so a platform that rejects the second after the first
        // fails the same way it does in C.
        if opts.reuse_port {
            #[cfg(unix)]
            socket.set_reuse_port(true)?;
            // Where the platform has no SO_REUSEPORT the request degrades to
            // SO_REUSEADDR rather than silently doing nothing — C's
            // `#ifndef SO_REUSEPORT / # define USE_SO_REUSEADDR`
            // (`drvAsynIPPort.c:88-92`). Windows is the case that reaches
            // this; both embedded triples define the option and take the arm
            // above.
            #[cfg(not(unix))]
            socket.set_reuse_address(true)?;
        }
        if opts.reuse_address {
            socket.set_reuse_address(true)?;
        }
        Ok(socket)
    }

    pub(super) fn udp_socket(local: SocketAddr, opts: SocketOptions) -> io::Result<UdpSocket> {
        let socket = new_socket(
            local.is_ipv6(),
            socket2::Type::DGRAM,
            socket2::Protocol::UDP,
            opts,
        )?;
        socket.bind(&local.into())?;
        Ok(UdpSocket::from(socket))
    }

    pub(super) fn tcp_listener(
        local: SocketAddr,
        opts: SocketOptions,
        backlog: i32,
    ) -> io::Result<TcpListener> {
        let socket = new_socket(
            local.is_ipv6(),
            socket2::Type::STREAM,
            socket2::Protocol::TCP,
            opts,
        )?;
        socket.bind(&local.into())?;
        socket.listen(backlog)?;
        Ok(TcpListener::from(socket))
    }

    pub(super) fn tcp_connect(
        remote: SocketAddr,
        local: Option<SocketAddr>,
        opts: SocketOptions,
        timeout: Duration,
    ) -> io::Result<TcpStream> {
        let socket = new_socket(
            remote.is_ipv6(),
            socket2::Type::STREAM,
            socket2::Protocol::TCP,
            opts,
        )?;
        if let Some(local) = local {
            socket.bind(&local.into())?;
        }
        match socket.connect_timeout(&remote.into(), timeout) {
            Ok(()) => Ok(TcpStream::from(socket)),
            // `socket2::connect_timeout` polls for POLLIN|POLLOUT and rejects a
            // POLLHUP even when SO_ERROR is clear. macOS raises that when the
            // peer FINs immediately after accepting; Linux does not. The
            // handshake did complete, and C — which only inspects SO_ERROR
            // (`drvAsynIPPort.c:545-560`) — treats the link as connected and
            // lets the later read surface the EOF. So if the socket is in fact
            // connected, the connect succeeded whatever POLLHUP was flagged.
            Err(e) => match socket.peer_addr() {
                Ok(_) => Ok(TcpStream::from(socket)),
                Err(_) => Err(e),
            },
        }
    }
}

#[cfg(epics_embedded_target)]
mod sys {
    //! RTEMS / VxWorks: raw `libc`, because `socket2` does not build for either
    //! triple. Shapes follow `epics-ca-rs::server::blocking`, which is the
    //! proven-on-target version of this sequence.

    use super::SocketOptions;
    use std::io;
    use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
    use std::os::fd::{FromRawFd, RawFd};
    use std::time::Duration;

    /// An fd that closes on drop, so every `?` between `socket()` and the
    /// hand-off to a `std` type releases it. Without this each early return is
    /// its own descriptor leak, which is the bug the two hand-written copies of
    /// this sequence each had to avoid by hand.
    struct OwnedFd(RawFd);

    impl Drop for OwnedFd {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a descriptor this type exclusively owns and
            // has not yet released.
            unsafe { libc::close(self.0) };
        }
    }

    impl OwnedFd {
        /// Give up ownership without closing, for hand-off to a `std` socket.
        fn into_raw(self) -> RawFd {
            let fd = self.0;
            std::mem::forget(self);
            fd
        }
    }

    fn last_error() -> io::Error {
        io::Error::last_os_error()
    }

    fn set_bool_opt(fd: RawFd, level: libc::c_int, opt: libc::c_int) -> io::Result<()> {
        let one: libc::c_int = 1;
        // SAFETY: `fd` is a valid open socket; `one` outlives the call and its
        // size matches the `c_int` the option expects.
        let rc = unsafe {
            libc::setsockopt(
                fd,
                level,
                opt,
                &one as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(last_error());
        }
        Ok(())
    }

    fn new_socket(
        ty: libc::c_int,
        protocol: libc::c_int,
        opts: SocketOptions,
    ) -> io::Result<OwnedFd> {
        // SAFETY: `socket()` returns a fresh owned descriptor or -1.
        let fd = unsafe { libc::socket(libc::AF_INET, ty, protocol) };
        if fd < 0 {
            return Err(last_error());
        }
        // Own it before the first fallible call below, so every `?` closes it.
        let owned = OwnedFd(fd);
        if opts.broadcast {
            set_bool_opt(fd, libc::SOL_SOCKET, libc::SO_BROADCAST)?;
        }
        if opts.reuse_port {
            set_bool_opt(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT)?;
        }
        if opts.reuse_address {
            set_bool_opt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR)?;
        }
        Ok(owned)
    }

    /// Marshal an IPv4 `SocketAddr` into a `sockaddr_in`.
    ///
    /// IPv4 only: both targets' asyn drivers are IPv4 in practice, and an
    /// IPv6 address here is refused loudly rather than silently bound to the
    /// wrong family. `sin_len` (present on VxWorks, absent on Linux) is left at
    /// the zero the `zeroed()` gives it — the same choice
    /// `epics-ca-rs::server::blocking::bind_udp_search_socket` makes, and the
    /// `socklen_t` argument is what both stacks actually read.
    fn sockaddr_in(addr: SocketAddr) -> io::Result<libc::sockaddr_in> {
        let v4 = match addr {
            SocketAddr::V4(v4) => v4,
            SocketAddr::V6(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "IPv6 is not supported on this target",
                ));
            }
        };
        // SAFETY: `sockaddr_in` is a plain-old-data C struct for which all-zero
        // is a valid initial value.
        let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_port = v4.port().to_be();
        sin.sin_addr = libc::in_addr {
            s_addr: u32::from(*v4.ip()).to_be(),
        };
        Ok(sin)
    }

    fn bind_fd(fd: RawFd, addr: SocketAddr) -> io::Result<()> {
        let sin = sockaddr_in(addr)?;
        // SAFETY: `sin` is fully initialised and the length is its exact size.
        let rc = unsafe {
            libc::bind(
                fd,
                &sin as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(last_error());
        }
        Ok(())
    }

    pub(super) fn udp_socket(local: SocketAddr, opts: SocketOptions) -> io::Result<UdpSocket> {
        let owned = new_socket(libc::SOCK_DGRAM, libc::IPPROTO_UDP, opts)?;
        bind_fd(owned.0, local)?;
        // SAFETY: a valid, exclusively-owned socket descriptor released by
        // `into_raw` precisely so `UdpSocket` becomes its sole owner.
        Ok(unsafe { UdpSocket::from_raw_fd(owned.into_raw()) })
    }

    pub(super) fn tcp_listener(
        local: SocketAddr,
        opts: SocketOptions,
        backlog: i32,
    ) -> io::Result<TcpListener> {
        let owned = new_socket(libc::SOCK_STREAM, libc::IPPROTO_TCP, opts)?;
        bind_fd(owned.0, local)?;
        // SAFETY: `owned.0` is a valid bound socket.
        if unsafe { libc::listen(owned.0, backlog) } != 0 {
            return Err(last_error());
        }
        // SAFETY: as in `udp_socket`.
        Ok(unsafe { TcpListener::from_raw_fd(owned.into_raw()) })
    }

    /// Put `fd` into non-blocking mode.
    ///
    /// C `drvAsynIPPort.c::setNonBlock` (`:176-199`) branches exactly here:
    /// VxWorks uses `ioctl(fd, FIONBIO, &flags)` — note it passes the address
    /// of the flag, not the flag — and everything else uses `fcntl`.
    #[cfg(target_os = "vxworks")]
    fn set_nonblocking(fd: RawFd, on: bool) -> io::Result<()> {
        let mut flags: libc::c_int = i32::from(on);
        // SAFETY: `fd` is a valid socket; FIONBIO reads one `int` through the
        // pointer, which `flags` provides for the duration of the call.
        let rc = unsafe { libc::ioctl(fd, libc::FIONBIO, &mut flags as *mut libc::c_int) };
        if rc < 0 {
            return Err(last_error());
        }
        Ok(())
    }

    #[cfg(not(target_os = "vxworks"))]
    fn set_nonblocking(fd: RawFd, on: bool) -> io::Result<()> {
        // SAFETY: `fd` is a valid descriptor; F_GETFL takes no further argument.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
        if flags < 0 {
            return Err(last_error());
        }
        let next = if on {
            flags | libc::O_NONBLOCK
        } else {
            flags & !libc::O_NONBLOCK
        };
        // SAFETY: `fd` is valid and `next` is a flag word derived from F_GETFL.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, next) } < 0 {
            return Err(last_error());
        }
        Ok(())
    }

    pub(super) fn tcp_connect(
        remote: SocketAddr,
        local: Option<SocketAddr>,
        opts: SocketOptions,
        timeout: Duration,
    ) -> io::Result<TcpStream> {
        let owned = new_socket(libc::SOCK_STREAM, libc::IPPROTO_TCP, opts)?;
        if let Some(local) = local {
            bind_fd(owned.0, local)?;
        }
        connect_fd(&owned, remote, timeout)?;
        // SAFETY: as in `udp_socket`.
        Ok(unsafe { TcpStream::from_raw_fd(owned.into_raw()) })
    }

    /// RTEMS: plain blocking connect, no deadline.
    ///
    /// C parity, not a shortcut: `__rtems__` selects `USE_SOCKTIMEOUT`
    /// (`drvAsynIPPort.c:71-72`), which compiles out both the pre-connect
    /// `setNonBlock` (`:511`) and the `poll(POLLOUT)` deadline (`:544`). The
    /// transfer bound C keeps there is `SO_RCVTIMEO`/`SO_SNDTIMEO`, applied by
    /// the read/write path rather than here.
    #[cfg(target_os = "rtems")]
    fn connect_fd(owned: &OwnedFd, remote: SocketAddr, _timeout: Duration) -> io::Result<()> {
        let sin = sockaddr_in(remote)?;
        // SAFETY: `sin` is fully initialised and the length is its exact size.
        let rc = unsafe {
            libc::connect(
                owned.0,
                &sin as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(last_error());
        }
        Ok(())
    }

    /// VxWorks: non-blocking connect bounded by `poll(POLLOUT)`, then
    /// `SO_ERROR`.
    ///
    /// This is C's `USE_POLL` path (`drvAsynIPPort.c:511`, `:523`, `:544-560`),
    /// which VxWorks takes via `FAKE_POLL`. C fakes `poll` with `select()`
    /// because its VxWorks headers lack `poll`; the Rust `libc` binding for the
    /// triple exposes `poll` directly, so the fake is unnecessary and the
    /// observable behaviour is the same.
    #[cfg(not(target_os = "rtems"))]
    fn connect_fd(owned: &OwnedFd, remote: SocketAddr, timeout: Duration) -> io::Result<()> {
        let sin = sockaddr_in(remote)?;
        set_nonblocking(owned.0, true)?;
        // SAFETY: `sin` is fully initialised and the length is its exact size.
        let rc = unsafe {
            libc::connect(
                owned.0,
                &sin as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if rc == 0 {
            set_nonblocking(owned.0, false)?;
            return Ok(());
        }
        let err = last_error();
        let in_progress = matches!(
            err.raw_os_error(),
            Some(e) if e == libc::EINPROGRESS || e == libc::EWOULDBLOCK
        );
        if !in_progress {
            return Err(err);
        }

        let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let mut pfd = libc::pollfd {
            fd: owned.0,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: a one-element `pollfd` array, matching the count passed.
        let n = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, ms) };
        if n < 0 {
            return Err(last_error());
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "connect timed out"));
        }

        // C reads SO_ERROR and treats a non-zero value as the connect failure
        // (`:545-560`); poll reporting the fd ready says only that the attempt
        // finished, not that it succeeded.
        let mut so_error: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `so_error`/`len` are live for the call and sized as SO_ERROR
        // expects.
        let rc = unsafe {
            libc::getsockopt(
                owned.0,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut so_error as *mut libc::c_int as *mut libc::c_void,
                &mut len as *mut libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(last_error());
        }
        if so_error != 0 {
            return Err(io::Error::from_raw_os_error(so_error));
        }
        set_nonblocking(owned.0, false)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn localhost(port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    }

    #[test]
    fn udp_binds_and_reports_its_port() {
        let sock = udp_socket(localhost(0), SocketOptions::default()).unwrap();
        assert_ne!(sock.local_addr().unwrap().port(), 0);
    }

    /// The invariant the whole module exists for: the option is set on an
    /// unbound socket, so two sockets can share one port. Bind-then-setsockopt
    /// would leave the second bind failing with EADDRINUSE.
    #[test]
    fn fanout_options_let_two_sockets_share_a_port() {
        let first = udp_socket(localhost(0), SocketOptions::FANOUT).unwrap();
        let port = first.local_addr().unwrap().port();
        let second = udp_socket(localhost(port), SocketOptions::FANOUT).unwrap();
        assert_eq!(second.local_addr().unwrap().port(), port);
    }

    /// The negative half: without the options the same second bind must fail.
    /// Read as a pair with the test above, this is what proves the options are
    /// doing the work rather than the platform being permissive.
    #[test]
    fn without_fanout_options_a_shared_port_is_refused() {
        let first = udp_socket(localhost(0), SocketOptions::default()).unwrap();
        let port = first.local_addr().unwrap().port();
        assert!(udp_socket(localhost(port), SocketOptions::default()).is_err());
    }

    #[test]
    fn tcp_listener_accepts_a_connect() {
        let listener = tcp_listener(localhost(0), SocketOptions::REUSE_ADDRESS, 8).unwrap();
        let addr = listener.local_addr().unwrap();
        let joiner = std::thread::spawn(move || listener.accept().map(|(s, _)| s));
        let client =
            tcp_connect(addr, None, SocketOptions::default(), Duration::from_secs(5)).unwrap();
        let accepted = joiner.join().unwrap().unwrap();
        assert_eq!(accepted.local_addr().unwrap().port(), addr.port());
        assert_eq!(client.peer_addr().unwrap().port(), addr.port());
    }

    /// A connect to a port nothing listens on must fail rather than hang.
    #[test]
    fn tcp_connect_to_a_closed_port_fails() {
        // Bind then drop, so the port is real but unowned.
        let port = {
            let probe = tcp_listener(localhost(0), SocketOptions::default(), 1).unwrap();
            probe.local_addr().unwrap().port()
        };
        let r = tcp_connect(
            localhost(port),
            None,
            SocketOptions::default(),
            Duration::from_secs(5),
        );
        assert!(r.is_err());
    }

    #[test]
    fn tcp_connect_honours_a_local_bind() {
        let listener = tcp_listener(localhost(0), SocketOptions::REUSE_ADDRESS, 8).unwrap();
        let addr = listener.local_addr().unwrap();
        let joiner = std::thread::spawn(move || listener.accept().map(|(s, _)| s));
        let client = tcp_connect(
            addr,
            Some(localhost(0)),
            SocketOptions::REUSE_ADDRESS,
            Duration::from_secs(5),
        )
        .unwrap();
        let accepted = joiner.join().unwrap().unwrap();
        assert_eq!(
            accepted.peer_addr().unwrap().port(),
            client.local_addr().unwrap().port()
        );
    }
}
