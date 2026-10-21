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
//! # Why the option is ordered against the bind
//!
//! `SO_REUSEPORT` is what lets several IOCs share one UDP port and have the
//! kernel fan each datagram out to all of them. A configured number needs it
//! set before the bind, or the bind of a shared port is `EADDRINUSE`; a number
//! the kernel chooses needs it set after, or Linux may hand `bind(0)` a number
//! a stranger's fanout group holds. [`udp_socket`] takes that order from
//! `net::bind_fanout`; a TCP socket sets every option first. Either way `std`'s
//! constructors, which bind before any option can be set, are not enough.
//!
//! # The C authority for each branch
//!
//! Option selection follows EPICS base (R7.0.10-146-g8f5015b66)
//! `libcom/src/osi/os/default/osdSockAddrReuse.cpp`: the datagram-fanout helper
//! sets `SO_REUSEPORT` (where defined) *and then* `SO_REUSEADDR`; the
//! time-wait helper sets `SO_REUSEADDR` alone. Both `SO_REUSEPORT` constants
//! exist on the two embedded targets (`0x0200` on newlib/RTEMS and on VxWorks),
//! so neither takes the `#ifndef SO_REUSEPORT` fallback the C comment describes
//! for older systems.
//!
//! Connect behaviour follows asyn (R4-46) `drvAsynIPPort.c`, whose three branches this
//! module reproduces exactly:
//!
//! | target | connect | timeout honoured | C authority |
//! |---|---|---|---|
//! | hosted | non-blocking, then `poll(POLLOUT)` and `SO_ERROR` | yes | `:516-566` |
//! | VxWorks | non-blocking, then `select()` via `FAKE_POLL` | yes | `:76`, `:139-171`, `:178` |
//! | RTEMS | **blocking** | **no** | `:71-72` — `__rtems__` takes `USE_SOCKTIMEOUT`, and both `setNonBlock` and the poll block sit inside `#ifdef USE_POLL` |
//!
//! The RTEMS row is not an omission. C genuinely has no connect timeout there;
//! it bounds the *transfer* instead, with `SO_RCVTIMEO`/`SO_SNDTIMEO`
//! (`:670-676`, `:800-806`). Honouring the deadline there anyway would be a
//! deviation invented by the port, so [`tcp_connect`] documents the difference
//! rather than papering over it.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::time::Duration;

/// Options applied to a fresh socket around its bind (see the module docs).
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
    /// C `epicsSocketEnableAddressUseForDatagramFanout`: `SO_REUSEPORT` where
    /// defined, then `SO_REUSEADDR`, each one that fails logged and skipped,
    /// never failing the setup (osdSockAddrReuse.cpp:46-68).
    pub datagram_fanout: bool,
}

impl SocketOptions {
    /// The UDP server's pair: the fanout helper, then its own `SO_REUSEADDR`,
    /// which does fail the setup (drvAsynIPServerPort.c:444-454).
    pub const FANOUT: Self = Self {
        broadcast: false,
        reuse_address: true,
        reuse_port: false,
        datagram_fanout: true,
    };

    /// `SO_REUSEADDR` alone, matching C
    /// `epicsSocketEnableAddressReuseDuringTimeWaitState`. This is what a TCP
    /// listener gets; the fanout helper is `SOCK_DGRAM`-only.
    pub const REUSE_ADDRESS: Self = Self {
        broadcast: false,
        reuse_address: true,
        reuse_port: false,
        datagram_fanout: false,
    };
}

/// C `setfanout`'s report of an option it could not set
/// (osdSockAddrReuse.cpp:46-58); the setup goes on without it.
fn fanout_refused(optname: &str) {
    crate::runtime::log::errlog_vprintf(format_args!(
        "epicsSocketEnablePortUseForDatagramFanout: unable to set {optname}?\n"
    ));
}

/// Open a UDP socket, apply `opts`, and bind it to `local`.
pub fn udp_socket(local: SocketAddr, opts: SocketOptions) -> Result<UdpSocket, SetupError> {
    udp_socket_on(local, opts, crate::net::PortOrigin::Configured)
}

/// [`udp_socket`] on a number of the given origin: one this process chose
/// is bound before its reuse options, as `net::bind_fanout` orders them.
pub fn udp_socket_on(
    local: SocketAddr,
    opts: SocketOptions,
    origin: crate::net::PortOrigin,
) -> Result<UdpSocket, SetupError> {
    sys::udp_socket(local, opts, origin)
}

/// Open a TCP socket, apply `opts`, bind it to `local`, and start listening.
pub fn tcp_listener(
    local: SocketAddr,
    opts: SocketOptions,
    backlog: i32,
) -> Result<TcpListener, SetupError> {
    sys::tcp_listener(local, opts, backlog)
}

/// The setup call that failed before any `connect()`. C `connectIt` words each
/// one differently (`drvAsynIPPort.c:443-523`), so the caller needs to know which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStep {
    /// `socket()`.
    Create,
    /// `SO_BROADCAST`.
    Broadcast,
    /// `SO_REUSEPORT`, or the `SO_REUSEADDR` it degrades to where undefined.
    ReusePort,
    /// `SO_REUSEADDR` requested on its own.
    ReuseAddress,
    /// `bind()` to the local address.
    Bind,
    /// `listen()` on a bound stream socket.
    Listen,
    /// Switching the blocking mode around the bounded connect.
    NonBlocking,
}

/// A socket setup call that failed, and which one.
#[derive(Debug)]
pub struct SetupError {
    pub step: SetupStep,
    pub error: io::Error,
}

impl SetupError {
    fn at(step: SetupStep) -> impl FnOnce(io::Error) -> Self {
        move |error| Self { step, error }
    }
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for SetupError {}

impl From<SetupError> for io::Error {
    fn from(e: SetupError) -> Self {
        e.error
    }
}

/// Why [`tcp_connect`] failed, split by phase. C `drvAsynIPPort.c::connectIt`
/// saves the text of `connect()`'s own errno (`:534-539`) and reports that even
/// when the deadline poll is what ended the attempt (`:568-571`), so a refused
/// non-blocking connect reads "Operation now in progress" there.
#[derive(Debug)]
pub enum TcpConnectError {
    /// A setup call before `connect()`, or the blocking-mode restore after it.
    Setup(SetupError),
    /// `connect()` returned this outright.
    Connect(io::Error),
    /// `connect()` returned `in_progress` and the deadline poll then ended the
    /// attempt with `cause`: the timeout, `SO_ERROR`, or the poll's own error.
    Deferred {
        in_progress: io::Error,
        cause: io::Error,
    },
}

impl TcpConnectError {
    /// C's saved `sockerrmsg`: the failing setup call's error, else what
    /// `connect()` itself returned — never the poll's.
    pub fn sockerr(&self) -> &io::Error {
        match self {
            Self::Setup(SetupError { error: e, .. })
            | Self::Connect(e)
            | Self::Deferred { in_progress: e, .. } => e,
        }
    }
}

impl std::fmt::Display for TcpConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Setup(SetupError { error: e, .. })
            | Self::Connect(e)
            | Self::Deferred { cause: e, .. } => e.fmt(f),
        }
    }
}

impl std::error::Error for TcpConnectError {}

impl From<SetupError> for TcpConnectError {
    fn from(e: SetupError) -> Self {
        Self::Setup(e)
    }
}

/// Which readiness [`poll_ready`] waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Readable,
    Writable,
}

/// A socket [`poll_ready`] can wait on: anything that lends its descriptor.
#[cfg(unix)]
pub trait Pollable: std::os::fd::AsFd {}
#[cfg(unix)]
impl<T: std::os::fd::AsFd> Pollable for T {}
/// A socket [`poll_ready`] can wait on: anything that lends its `SOCKET`.
#[cfg(windows)]
pub trait Pollable: std::os::windows::io::AsSocket {}
#[cfg(windows)]
impl<T: std::os::windows::io::AsSocket> Pollable for T {}

/// One C `poll` of `socket` for `want` under `USE_POLL`, which is `WSAPoll` on
/// WIN32 (drvAsynIPPort.c:77-79): `Ok(true)` ready, `Ok(false)` the `ms`
/// interval expired, `ms < 0` waits without bound. An `EINTR` is returned, not
/// retried — whether a signal ends the wait is the caller's rule.
#[cfg(unix)]
pub fn poll_ready(socket: &impl Pollable, want: Readiness, ms: i32) -> io::Result<bool> {
    use std::os::fd::AsRawFd;

    let mut pfd = libc::pollfd {
        fd: socket.as_fd().as_raw_fd(),
        events: match want {
            Readiness::Readable => libc::POLLIN,
            Readiness::Writable => libc::POLLOUT,
        },
        revents: 0,
    };
    // SAFETY: a one-element `pollfd` array, matching the count passed; the fd
    // is borrowed from `socket` for the call.
    match unsafe { libc::poll(&mut pfd, 1, ms) } {
        0 => Ok(false),
        n if n > 0 => Ok(true),
        _ => Err(io::Error::last_os_error()),
    }
}

/// See the unix [`poll_ready`].
#[cfg(windows)]
pub fn poll_ready(socket: &impl Pollable, want: Readiness, ms: i32) -> io::Result<bool> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{POLLIN, POLLOUT, WSAPOLLFD, WSAPoll};

    let mut pfd = WSAPOLLFD {
        fd: socket.as_socket().as_raw_socket() as usize,
        events: match want {
            Readiness::Readable => POLLIN,
            Readiness::Writable => POLLOUT,
        },
        revents: 0,
    };
    // SAFETY: a one-element `WSAPOLLFD` array, matching the count passed; the
    // socket is borrowed for the call.
    match unsafe { WSAPoll(&mut pfd, 1, ms) } {
        0 => Ok(false),
        n if n > 0 => Ok(true),
        // `WSAGetLastError` is the `GetLastError` `last_os_error` reads.
        _ => Err(io::Error::last_os_error()),
    }
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
) -> Result<TcpStream, TcpConnectError> {
    sys::tcp_connect(remote, local, opts, timeout)
}

/// Enable `SO_KEEPALIVE` on an already-accepted TCP connection.
///
/// The one option here applied *after* the socket exists rather than before it
/// binds: a server does not construct the sockets it serves, it accepts them.
/// It still belongs here, because "reach `setsockopt` without `socket2` on the
/// embedded triples" is the whole reason this module exists, and a keepalive
/// setter written at a call site would be the third hand-rolled copy of that
/// twenty lines (`epics-tools-rs::procserv::client::set_keepalive` is the
/// second).
///
/// The error is returned, not swallowed. C treats this option as required —
/// `create_tcp_client` calls `destroy_client` and refuses the connection when
/// it fails (`caservertask.c:1456-1462`) — so the caller gets to make C's
/// decision.
///
/// Only the flag is set, which is all C sets. The idle/probe tuning the hosted
/// CA driver adds through `socket2` (`epics-ca-rs::server::tcp`, 15 s + 5 s) is
/// deliberately not reproduced: `TCP_KEEPIDLE` is a Linux spelling, BSD-derived
/// stacks call it `TCP_KEEPALIVE`, and neither has been measured on RTEMS or
/// VxWorks. A target's default idle is therefore what applies here, as it is in
/// C.
pub fn enable_keepalive(sock: &TcpStream) -> io::Result<()> {
    sys::enable_keepalive(sock)
}

/// C `hostToIPAddr`: the first IPv4 address of `name`. The error is the
/// `errno` the resolver leaves, which is all a C caller's
/// `epicsSocketConvertErrnoToString` can print; `getaddrinfo` does not promise
/// one, so it may be `0` or left over from an earlier call, as it is in C.
pub fn host_to_ip_addr(name: &str) -> Result<Ipv4Addr, io::Error> {
    sys::host_to_ip_addr(name)
}

/// C `aToIPAddr` (`aToIPAddr.c:75-194`): its six `sscanf` patterns in C's
/// order — dotted quad, quad `:` port, raw number, raw number `:` port, host
/// name, host name `:` port. `%u` is decimal and wraps as glibc's
/// `(unsigned) strtoul`, so `010.0.0.1` is 10.0.0.1, and on LP64 a port of
/// 2^32+N is N. The second member is the host name when the address came from
/// [`host_to_ip_addr`], for a caller that re-resolves it.
pub fn a_to_ip_addr(s: &str, default_port: u16) -> Option<(SocketAddrV4, Option<&str>)> {
    use super::stdlib::{c_isspace, scanf_ulong, ulong_to_uint};
    use std::ffi::c_ulong;

    fn blank(s: &str) -> bool {
        s.trim_start_matches(c_isspace).is_empty()
    }
    /// A whitespace directive, then a literal character.
    fn lit(s: &str, c: char) -> Option<&str> {
        s.trim_start_matches(c_isspace).strip_prefix(c)
    }
    fn lu(s: &str) -> Option<(c_ulong, &str)> {
        scanf_ulong(s, 10).map(|(v, end)| (v, &s[end..]))
    }
    fn u(s: &str) -> Option<(u32, &str)> {
        lu(s).map(|(v, rest)| (ulong_to_uint(v), rest))
    }
    /// `addrArrayToUL` refuses a part above 0xff, after all four are read.
    fn quad(s: &str) -> Option<(Option<Ipv4Addr>, &str)> {
        let (a, s) = u(s)?;
        let (b, s) = u(lit(s, '.')?)?;
        let (c, s) = u(lit(s, '.')?)?;
        let (d, s) = u(lit(s, '.')?)?;
        let part = |p: u32| u8::try_from(p).ok();
        let ip = (|| Some(Ipv4Addr::new(part(a)?, part(b)?, part(c)?, part(d)?)))();
        Some((ip, s))
    }
    /// A raw address above 0xffffffff is refused; where `long` is 32 bits,
    /// `strtoul` saturated at 0xffffffff instead.
    fn raw(v: c_ulong) -> Option<Ipv4Addr> {
        u32::try_from(u128::from(v)).ok().map(Ipv4Addr::from)
    }
    /// `initIPAddr` refuses a port above 0xffff.
    fn sock(ip: Ipv4Addr, port: u32, name: Option<&str>) -> Option<(SocketAddrV4, Option<&str>)> {
        Some((SocketAddrV4::new(ip, u16::try_from(port).ok()?), name))
    }

    // " %u . %u . %u . %u %7s " == 4, then " %u . %u . %u . %u : %u %7s" >= 5,
    // where a sixth conversion is detritus and the end.
    if let Some((ip, rest)) = quad(s) {
        if blank(rest) {
            return sock(ip?, default_port.into(), None);
        }
        if let Some((port, rest)) = lit(rest, ':').and_then(u) {
            return if blank(rest) {
                sock(ip?, port, None)
            } else {
                None
            };
        }
    }
    // " %lu %7s " == 1, then " %lu : %u %7s " >= 2.
    if let Some((v, rest)) = lu(s) {
        if blank(rest) {
            return sock(raw(v)?, default_port.into(), None);
        }
        if let Some((port, rest)) = lit(rest, ':').and_then(u) {
            return if blank(rest) {
                sock(raw(v)?, port, None)
            } else {
                None
            };
        }
    }
    // " %511[^:] %s " == 1, then " %511[^:] : %u %s " >= 2; a name that does
    // not resolve in the first falls through to the second.
    let s = s.trim_start_matches(c_isspace);
    let end = s.find(':').unwrap_or(s.len()).min(511);
    // Cut inside a character, the rest starts with a byte `%s` takes and `:`
    // does not match, so neither pattern can succeed.
    if end == 0 || !s.is_char_boundary(end) {
        return None;
    }
    let (name, rest) = s.split_at(end);
    if blank(rest) {
        if let Ok(ip) = host_to_ip_addr(name) {
            return sock(ip, default_port.into(), Some(name));
        }
    }
    let (port, rest) = u(lit(rest, ':')?)?;
    if !blank(rest) {
        return None;
    }
    sock(host_to_ip_addr(name).ok()?, port, Some(name))
}

/// [`host_to_ip_addr`] through `std`'s resolver, for the targets whose C
/// `hostToIPAddr` is not the `getaddrinfo(AF_INET)` one.
#[cfg(any(epics_embedded_target, not(unix)))]
fn std_host_to_ip_addr(name: &str) -> Result<Ipv4Addr, io::Error> {
    use std::net::ToSocketAddrs;
    let found = (name, 0).to_socket_addrs().ok().and_then(|mut addrs| {
        addrs.find_map(|a| match a {
            SocketAddr::V4(v4) => Some(*v4.ip()),
            SocketAddr::V6(_) => None,
        })
    });
    found.ok_or_else(io::Error::last_os_error)
}

#[cfg(not(epics_embedded_target))]
mod sys {
    //! Hosted: `socket2` owns the pre-bind option surface; the connect deadline
    //! is C's `poll(POLLOUT)` under `USE_POLL`.

    use super::{SetupError, SetupStep, SocketOptions, TcpConnectError};
    use std::io;
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
    use std::time::Duration;

    /// The `USE_INFO` `hostToIPAddr` (osdSock.c:250-273): `getaddrinfo` with an
    /// `AF_INET` hint, the first answer, and on failure nothing between the
    /// call and the caller's `errno` read.
    #[cfg(unix)]
    pub(super) fn host_to_ip_addr(name: &str) -> Result<Ipv4Addr, io::Error> {
        let Ok(node) = std::ffi::CString::new(name) else {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        };
        // SAFETY: zeroed `addrinfo` is the hint C builds with `memset`.
        let mut hint: libc::addrinfo = unsafe { std::mem::zeroed() };
        hint.ai_family = libc::AF_INET;
        let mut result: *mut libc::addrinfo = std::ptr::null_mut();
        // SAFETY: `node` is NUL-terminated, `hint` and `result` outlive the call.
        let ret = unsafe { libc::getaddrinfo(node.as_ptr(), std::ptr::null(), &hint, &mut result) };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut found = None;
        let mut ai = result;
        while !ai.is_null() {
            // SAFETY: `ai` walks the list `getaddrinfo` returned, freed below.
            let entry = unsafe { &*ai };
            if entry.ai_family == libc::AF_INET && !entry.ai_addr.is_null() {
                // SAFETY: an `AF_INET` entry's `ai_addr` is a `sockaddr_in`.
                let sin = unsafe { &*(entry.ai_addr as *const libc::sockaddr_in) };
                found = Some(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)));
                break;
            }
            ai = entry.ai_next;
        }
        let err = io::Error::last_os_error();
        // SAFETY: `result` came from a successful `getaddrinfo` and is freed once.
        unsafe { libc::freeaddrinfo(result) };
        found.ok_or(err)
    }

    #[cfg(not(unix))]
    pub(super) fn host_to_ip_addr(name: &str) -> Result<Ipv4Addr, io::Error> {
        super::std_host_to_ip_addr(name)
    }

    fn new_socket(
        addr_is_v6: bool,
        ty: socket2::Type,
        protocol: socket2::Protocol,
        opts: SocketOptions,
    ) -> Result<socket2::Socket, SetupError> {
        let domain = if addr_is_v6 {
            socket2::Domain::IPV6
        } else {
            socket2::Domain::IPV4
        };
        let socket = socket2::Socket::new(domain, ty, Some(protocol))
            .map_err(SetupError::at(SetupStep::Create))?;
        if opts.broadcast {
            socket
                .set_broadcast(true)
                .map_err(SetupError::at(SetupStep::Broadcast))?;
        }
        Ok(socket)
    }

    /// The reuse options of `opts`; [`udp_socket`] orders them against the
    /// bind with `bind_fanout`, every other socket sets them first.
    fn set_reuse(socket: &socket2::Socket, opts: SocketOptions) -> Result<(), SetupError> {
        if opts.datagram_fanout {
            // Windows defines no SO_REUSEPORT, so C's helper sets only the
            // second (osdSockAddrReuse.cpp:63-66).
            #[cfg(unix)]
            if socket.set_reuse_port(true).is_err() {
                super::fanout_refused("SO_REUSEPORT");
            }
            if socket.set_reuse_address(true).is_err() {
                super::fanout_refused("SO_REUSEADDR");
            }
        }
        if opts.reuse_port {
            #[cfg(unix)]
            let set = socket.set_reuse_port(true);
            // Where the platform has no SO_REUSEPORT the request degrades to
            // SO_REUSEADDR rather than silently doing nothing — C's
            // `#ifndef SO_REUSEPORT / # define USE_SO_REUSEADDR`
            // (`drvAsynIPPort.c:90-92`). Windows is the case that reaches
            // this; both embedded triples define the option and take the arm
            // above.
            #[cfg(not(unix))]
            let set = socket.set_reuse_address(true);
            set.map_err(SetupError::at(SetupStep::ReusePort))?;
        }
        if opts.reuse_address {
            socket
                .set_reuse_address(true)
                .map_err(SetupError::at(SetupStep::ReuseAddress))?;
        }
        Ok(())
    }

    fn bind(socket: &socket2::Socket, local: SocketAddr) -> Result<(), SetupError> {
        socket
            .bind(&local.into())
            .map_err(SetupError::at(SetupStep::Bind))
    }

    pub(super) fn udp_socket(
        local: SocketAddr,
        opts: SocketOptions,
        origin: crate::net::PortOrigin,
    ) -> Result<UdpSocket, SetupError> {
        let socket = new_socket(
            local.is_ipv6(),
            socket2::Type::DGRAM,
            socket2::Protocol::UDP,
            opts,
        )?;
        crate::net::bind_fanout(
            local.port(),
            origin,
            || set_reuse(&socket, opts),
            || bind(&socket, local),
        )?;
        Ok(UdpSocket::from(socket))
    }

    pub(super) fn tcp_listener(
        local: SocketAddr,
        opts: SocketOptions,
        backlog: i32,
    ) -> Result<TcpListener, SetupError> {
        let socket = new_socket(
            local.is_ipv6(),
            socket2::Type::STREAM,
            socket2::Protocol::TCP,
            opts,
        )?;
        set_reuse(&socket, opts)?;
        bind(&socket, local)?;
        socket
            .listen(backlog)
            .map_err(SetupError::at(SetupStep::Listen))?;
        Ok(TcpListener::from(socket))
    }

    pub(super) fn tcp_connect(
        remote: SocketAddr,
        local: Option<SocketAddr>,
        opts: SocketOptions,
        timeout: Duration,
    ) -> Result<TcpStream, TcpConnectError> {
        let socket = new_socket(
            remote.is_ipv6(),
            socket2::Type::STREAM,
            socket2::Protocol::TCP,
            opts,
        )?;
        set_reuse(&socket, opts)?;
        if let Some(local) = local {
            bind(&socket, local)?;
        }
        connect_within(&socket, remote, timeout)?;
        Ok(TcpStream::from(socket))
    }

    /// C's `USE_POLL` connect (`drvAsynIPPort.c:516-566`): non-blocking
    /// `connect()`, then `poll(POLLOUT)` and `SO_ERROR` — `WSAPoll` on WIN32
    /// (:77-79). Only `SO_ERROR` decides success, as in C — a POLLHUP from a
    /// peer that FINs straight after the accept (macOS) is still a completed
    /// handshake.
    fn connect_within(
        socket: &socket2::Socket,
        remote: SocketAddr,
        timeout: Duration,
    ) -> Result<(), TcpConnectError> {
        socket
            .set_nonblocking(true)
            .map_err(SetupError::at(SetupStep::NonBlocking))?;
        if let Err(e) = socket.connect(&remote.into()) {
            // C's `SOCK_EWOULDBLOCK || SOCK_EINPROGRESS` (:541); WIN32's
            // non-blocking connect leaves the first.
            if e.kind() != io::ErrorKind::WouldBlock && e.raw_os_error() != Some(EINPROGRESS) {
                return Err(TcpConnectError::Connect(e));
            }
            if let Err(cause) = await_connect(socket, timeout) {
                return Err(TcpConnectError::Deferred {
                    in_progress: e,
                    cause,
                });
            }
        }
        socket
            .set_nonblocking(false)
            .map_err(SetupError::at(SetupStep::NonBlocking))?;
        Ok(())
    }

    #[cfg(unix)]
    const EINPROGRESS: i32 = libc::EINPROGRESS;
    /// C's `SOCK_EINPROGRESS` on WIN32.
    #[cfg(windows)]
    const EINPROGRESS: i32 = windows_sys::Win32::Networking::WinSock::WSAEINPROGRESS;

    /// The poll half of [`connect_within`]: `Ok` once `SO_ERROR` is clear.
    fn await_connect(socket: &socket2::Socket, timeout: Duration) -> io::Result<()> {
        let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        // One poll, no EINTR retry: C fails the connect on any return but 1
        // (drvAsynIPPort.c:555), a signal included.
        if !super::poll_ready(socket, super::Readiness::Writable, ms)? {
            return Err(io::ErrorKind::TimedOut.into());
        }
        match socket.take_error()? {
            None => Ok(()),
            Some(cause) => Err(cause),
        }
    }

    pub(super) fn enable_keepalive(sock: &TcpStream) -> io::Result<()> {
        socket2::SockRef::from(sock).set_keepalive(true)
    }
}

#[cfg(epics_embedded_target)]
mod sys {
    //! RTEMS / VxWorks: raw `libc`, because `socket2` does not build for either
    //! triple. Shapes follow `epics-ca-rs::server::blocking`, which is the
    //! proven-on-target version of this sequence.

    use super::{SetupError, SetupStep, SocketOptions, TcpConnectError};
    use std::io;
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
    use std::os::fd::{FromRawFd, RawFd};
    use std::time::Duration;

    /// RTEMS takes C's `USE_BY` `gethostbyname` (osdSock.c:44-48) and VxWorks
    /// its own; neither has been probed for the `errno` it leaves.
    pub(super) fn host_to_ip_addr(name: &str) -> Result<Ipv4Addr, io::Error> {
        super::std_host_to_ip_addr(name)
    }

    /// An fd that closes on drop, so every `?` between `socket()` and the
    /// hand-off to a `std` type releases it. Without this each early return is
    /// its own descriptor leak, which is the bug the two hand-written copies of
    /// this sequence each had to avoid by hand.
    struct OwnedFd(RawFd);

    impl std::os::fd::AsFd for OwnedFd {
        fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
            // SAFETY: `self.0` stays open until `self` drops, which the
            // borrow cannot outlive.
            unsafe { std::os::fd::BorrowedFd::borrow_raw(self.0) }
        }
    }

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
    ) -> Result<OwnedFd, SetupError> {
        // SAFETY: `socket()` returns a fresh owned descriptor or -1.
        let fd = unsafe { libc::socket(libc::AF_INET, ty, protocol) };
        if fd < 0 {
            return Err(SetupError::at(SetupStep::Create)(last_error()));
        }
        // Own it before the first fallible call below, so every `?` closes it.
        let owned = OwnedFd(fd);
        if opts.broadcast {
            set_bool_opt(fd, libc::SOL_SOCKET, libc::SO_BROADCAST)
                .map_err(SetupError::at(SetupStep::Broadcast))?;
        }
        Ok(owned)
    }

    /// The reuse options of `opts`; [`udp_socket`] orders them against the
    /// bind with `bind_fanout`, every other socket sets them first.
    fn set_reuse(fd: RawFd, opts: SocketOptions) -> Result<(), SetupError> {
        if opts.datagram_fanout {
            if set_bool_opt(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT).is_err() {
                super::fanout_refused("SO_REUSEPORT");
            }
            if set_bool_opt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR).is_err() {
                super::fanout_refused("SO_REUSEADDR");
            }
        }
        if opts.reuse_port {
            set_bool_opt(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT)
                .map_err(SetupError::at(SetupStep::ReusePort))?;
        }
        if opts.reuse_address {
            set_bool_opt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR)
                .map_err(SetupError::at(SetupStep::ReuseAddress))?;
        }
        Ok(())
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

    fn bind_fd(fd: RawFd, addr: SocketAddr) -> Result<(), SetupError> {
        let sin = sockaddr_in(addr).map_err(SetupError::at(SetupStep::Bind))?;
        // SAFETY: `sin` is fully initialised and the length is its exact size.
        let rc = unsafe {
            libc::bind(
                fd,
                &sin as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(SetupError::at(SetupStep::Bind)(last_error()));
        }
        Ok(())
    }

    pub(super) fn udp_socket(
        local: SocketAddr,
        opts: SocketOptions,
        origin: crate::net::PortOrigin,
    ) -> Result<UdpSocket, SetupError> {
        let owned = new_socket(libc::SOCK_DGRAM, libc::IPPROTO_UDP, opts)?;
        crate::net::bind_fanout(
            local.port(),
            origin,
            || set_reuse(owned.0, opts),
            || bind_fd(owned.0, local),
        )?;
        // SAFETY: a valid, exclusively-owned socket descriptor released by
        // `into_raw` precisely so `UdpSocket` becomes its sole owner.
        Ok(unsafe { UdpSocket::from_raw_fd(owned.into_raw()) })
    }

    pub(super) fn tcp_listener(
        local: SocketAddr,
        opts: SocketOptions,
        backlog: i32,
    ) -> Result<TcpListener, SetupError> {
        let owned = new_socket(libc::SOCK_STREAM, libc::IPPROTO_TCP, opts)?;
        set_reuse(owned.0, opts)?;
        bind_fd(owned.0, local)?;
        // SAFETY: `owned.0` is a valid bound socket.
        if unsafe { libc::listen(owned.0, backlog) } != 0 {
            return Err(SetupError::at(SetupStep::Listen)(last_error()));
        }
        // SAFETY: as in `udp_socket`.
        Ok(unsafe { TcpListener::from_raw_fd(owned.into_raw()) })
    }

    // This module's target set is exactly {rtems, vxworks} — that is what
    // `epics_embedded_target` means. Both `connect_fd` and `set_nonblocking`
    // below are written as that closed pair rather than as a
    // `vxworks`/`everything else` split, so adding a third embedded triple
    // fails to compile *here*, at the decision, instead of silently taking
    // whichever arm happened to be the fallback.
    #[cfg(not(any(target_os = "rtems", target_os = "vxworks")))]
    compile_error!(
        "epics_embedded_target gained a triple beyond rtems/vxworks: choose \
         its connect_fd and set_nonblocking arms explicitly"
    );

    /// Put `fd` into non-blocking mode.
    ///
    /// C `drvAsynIPPort.c::setNonBlock` (`:176-200`) branches exactly here:
    /// VxWorks uses `ioctl(fd, FIONBIO, &flags)` — note it passes the address
    /// of the flag, not the flag itself — where a POSIX target uses `fcntl`.
    /// Only the VxWorks arm exists: RTEMS takes the blocking `connect_fd`
    /// below and never needs the socket switched, so an `fcntl` arm here
    /// would be dead on every triple this module compiles for.
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

    pub(super) fn tcp_connect(
        remote: SocketAddr,
        local: Option<SocketAddr>,
        opts: SocketOptions,
        timeout: Duration,
    ) -> Result<TcpStream, TcpConnectError> {
        let owned = new_socket(libc::SOCK_STREAM, libc::IPPROTO_TCP, opts)?;
        set_reuse(owned.0, opts)?;
        if let Some(local) = local {
            bind_fd(owned.0, local)?;
        }
        connect_fd(&owned, remote, timeout)?;
        // SAFETY: as in `udp_socket`.
        Ok(unsafe { TcpStream::from_raw_fd(owned.into_raw()) })
    }

    pub(super) fn enable_keepalive(sock: &TcpStream) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        set_bool_opt(sock.as_raw_fd(), libc::SOL_SOCKET, libc::SO_KEEPALIVE)
    }

    /// RTEMS: plain blocking connect, no deadline.
    ///
    /// C parity, not a shortcut: `__rtems__` selects `USE_SOCKTIMEOUT`
    /// (`drvAsynIPPort.c:71-72`), which compiles out both the `setNonBlock`
    /// under `USE_POLL` (`:515-524`) and the `poll(POLLOUT)` connect deadline
    /// (`:540-567`). The transfer bound C keeps there is
    /// `SO_RCVTIMEO`/`SO_SNDTIMEO`, applied by the read/write path rather than
    /// here.
    #[cfg(target_os = "rtems")]
    fn connect_fd(
        owned: &OwnedFd,
        remote: SocketAddr,
        _timeout: Duration,
    ) -> Result<(), TcpConnectError> {
        let sin = sockaddr_in(remote).map_err(TcpConnectError::Connect)?;
        // SAFETY: `sin` is fully initialised and the length is its exact size.
        let rc = unsafe {
            libc::connect(
                owned.0,
                &sin as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(TcpConnectError::Connect(last_error()));
        }
        Ok(())
    }

    /// VxWorks: non-blocking connect bounded by `poll(POLLOUT)`, then
    /// `SO_ERROR`.
    ///
    /// This is C's `USE_POLL` path (`drvAsynIPPort.c:515-567`). VxWorks takes
    /// it via `FAKE_POLL`. C fakes `poll` with `select()` because its VxWorks
    /// headers lack `poll`; the Rust `libc` binding for the triple exposes
    /// `poll` directly, so the fake is unnecessary and the observable
    /// behaviour is the same.
    #[cfg(target_os = "vxworks")]
    fn connect_fd(
        owned: &OwnedFd,
        remote: SocketAddr,
        timeout: Duration,
    ) -> Result<(), TcpConnectError> {
        let sin = sockaddr_in(remote).map_err(TcpConnectError::Connect)?;
        set_nonblocking(owned.0, true).map_err(SetupError::at(SetupStep::NonBlocking))?;
        // SAFETY: `sin` is fully initialised and the length is its exact size.
        let rc = unsafe {
            libc::connect(
                owned.0,
                &sin as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            let err = last_error();
            let in_progress = matches!(
                err.raw_os_error(),
                Some(e) if e == libc::EINPROGRESS || e == libc::EWOULDBLOCK
            );
            if !in_progress {
                return Err(TcpConnectError::Connect(err));
            }
            if let Err(cause) = await_connect(owned, timeout) {
                return Err(TcpConnectError::Deferred {
                    in_progress: err,
                    cause,
                });
            }
        }
        set_nonblocking(owned.0, false).map_err(SetupError::at(SetupStep::NonBlocking))?;
        Ok(())
    }

    /// The poll half of the VxWorks [`connect_fd`]: `Ok` once `SO_ERROR` is
    /// clear.
    #[cfg(target_os = "vxworks")]
    fn await_connect(owned: &OwnedFd, timeout: Duration) -> io::Result<()> {
        let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        if !super::poll_ready(owned, super::Readiness::Writable, ms)? {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "connect timed out"));
        }

        // C reads SO_ERROR and treats a non-zero value as the connect failure
        // (`:555-565`); poll reporting the fd ready says only that the attempt
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

    /// Privileged port 1: no test can bind it, so a connect is always refused.
    /// A freed ephemeral port can be taken by a concurrent test's listener.
    const DEAD_PORT: u16 = 1;

    /// Rows from libCom's own `aToIPAddr` with default port 5064.
    #[test]
    fn a_to_ip_addr_is_c_sscanf() {
        let mut rows = vec![
            ("010.0.0.1", Some("10.0.0.1:5064")),
            ("1.2.3.4:5", Some("1.2.3.4:5")),
            (" 1 . 2 . 3 . 4 ", Some("1.2.3.4:5064")),
            ("1.2.3.4 : 5064", Some("1.2.3.4:5064")),
            ("1.2.3.4 :5", Some("1.2.3.4:5")),
            ("1.2.3.4:+5", Some("1.2.3.4:5")),
            ("1.2.3.4:65535", Some("1.2.3.4:65535")),
            ("1.2.3.4:65536", None),
            ("1.2.3.4:5064x", None),
            ("1.2.3.4:5 6", None),
            ("1.2.3.4:", None),
            ("1.2.3.256", None),
            ("-1.2.3.4", None),
            ("16909060", Some("1.2.3.4:5064")),
            ("16909060:5064", Some("1.2.3.4:5064")),
            ("4294967295", Some("255.255.255.255:5064")),
            ("localhost", Some("127.0.0.1:5064")),
            (" localhost", Some("127.0.0.1:5064")),
            ("localhost:5065", Some("127.0.0.1:5065")),
            ("localhost:5065 x", None),
            ("localhost:-1", None),
            ("::1", None),
            ("fe80::1", None),
            ("", None),
        ];
        if std::ffi::c_ulong::BITS == 64 {
            rows.extend([
                ("1.2.3.4:4294972360", Some("1.2.3.4:5064")),
                ("4294967306.0.0.1", Some("10.0.0.1:5064")),
                ("-4294967286.0.0.1", Some("10.0.0.1:5064")),
                ("4294967296", None),
                ("16909060:4294972360", Some("1.2.3.4:5064")),
                ("localhost:4294972360", Some("127.0.0.1:5064")),
            ]);
        }
        for (s, want) in rows {
            let got = a_to_ip_addr(s, 5064).map(|(a, _)| a.to_string());
            assert_eq!(got.as_deref(), want, "{s:?}");
        }
        assert_eq!(a_to_ip_addr(" localhost", 0).unwrap().1, Some("localhost"));
        assert_eq!(a_to_ip_addr("16909060:1", 0).unwrap().1, None);
    }

    /// Read the flag back off the socket, because "the call returned `Ok`" and
    /// "the option is on" are different claims — C checks the `setsockopt`
    /// status and this checks the state that status is supposed to mean.
    #[cfg(unix)]
    #[test]
    fn enable_keepalive_actually_sets_so_keepalive() {
        use std::os::fd::AsRawFd;

        let listener = tcp_listener(localhost(0), SocketOptions::REUSE_ADDRESS, 4).unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = TcpStream::connect(localhost(port)).unwrap();
        let (accepted, _) = listener.accept().unwrap();

        let mut before: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `accepted` owns a live socket for the borrow; `before`/`len`
        // are sized as SO_KEEPALIVE's `c_int` optval expects.
        let rc = unsafe {
            libc::getsockopt(
                accepted.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_KEEPALIVE,
                std::ptr::addr_of_mut!(before).cast(),
                &mut len,
            )
        };
        assert_eq!(rc, 0, "getsockopt failed: {}", io::Error::last_os_error());
        assert_eq!(before, 0, "the fixture must start with the option off");

        enable_keepalive(&accepted).expect("SO_KEEPALIVE");

        let mut after: libc::c_int = 0;
        // SAFETY: as above.
        let rc = unsafe {
            libc::getsockopt(
                accepted.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_KEEPALIVE,
                std::ptr::addr_of_mut!(after).cast(),
                &mut len,
            )
        };
        assert_eq!(rc, 0, "getsockopt failed: {}", io::Error::last_os_error());
        assert_ne!(after, 0, "SO_KEEPALIVE not enabled");
        drop(client);
    }

    #[test]
    fn udp_binds_and_reports_its_port() {
        let sock = udp_socket(localhost(0), SocketOptions::default()).unwrap();
        assert_ne!(sock.local_addr().unwrap().port(), 0);
    }

    /// The invariant the whole module exists for: two fanout sockets share
    /// one port, the first on a number it chose.
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

    /// A fanout `bind(0)` never lands on a number a same-user fanout socket
    /// holds: with the options set first, Linux hands out such a number about
    /// one time in fifteen against 2000 holders (`net::bind_fanout`).
    #[cfg(unix)]
    #[test]
    fn a_fanout_bind_zero_is_never_a_strangers_number() {
        let any = |port| SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port));
        let held: Vec<_> = (0..2000)
            .map(|_| {
                let s = socket2::Socket::new(
                    socket2::Domain::IPV4,
                    socket2::Type::DGRAM,
                    Some(socket2::Protocol::UDP),
                )
                .unwrap();
                s.set_reuse_address(true).unwrap();
                s.set_reuse_port(true).unwrap();
                s.bind(&any(0).into()).unwrap();
                s
            })
            .collect();
        let theirs: std::collections::HashSet<u16> = held
            .iter()
            .map(|s| s.local_addr().unwrap().as_socket().unwrap().port())
            .collect();
        for _ in 0..200 {
            let port = udp_socket(any(0), SocketOptions::FANOUT)
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            assert!(!theirs.contains(&port), "bound {port}, a stranger's");
        }
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
        let r = tcp_connect(
            localhost(DEAD_PORT),
            None,
            SocketOptions::default(),
            Duration::from_secs(5),
        );
        assert!(r.is_err());
    }

    /// The refusal arrives through `SO_ERROR` after `connect()` said
    /// EINPROGRESS, and both halves survive: asyn prints the first
    /// (drvAsynIPPort.c:534-539, :568-571). Linux defers a loopback refusal;
    /// other stacks may refuse in `connect()` itself.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_refusal_found_by_the_poll_keeps_connect_s_own_errno() {
        let err = tcp_connect(
            localhost(DEAD_PORT),
            None,
            SocketOptions::default(),
            Duration::from_secs(5),
        )
        .unwrap_err();
        match &err {
            TcpConnectError::Deferred { in_progress, cause } => {
                assert_eq!(in_progress.raw_os_error(), Some(libc::EINPROGRESS));
                assert_eq!(cause.kind(), io::ErrorKind::ConnectionRefused);
            }
            other => panic!("expected Deferred, got {other:?}"),
        }
        assert_eq!(err.sockerr().raw_os_error(), Some(libc::EINPROGRESS));
    }

    /// A signal that interrupts the connect poll fails the connect with
    /// connect()'s own errno, as C's single `poll` does (drvAsynIPPort.c:555,
    /// :568-571); it is not retried to the deadline.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_signal_during_the_connect_poll_fails_the_connect() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        extern "C" fn ignore(_: libc::c_int) {}
        // SAFETY: a zeroed `sigaction` with a no-op handler, no SA_RESTART.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = ignore as extern "C" fn(libc::c_int) as usize;
            assert_eq!(libc::sigaction(libc::SIGUSR2, &sa, std::ptr::null_mut()), 0);
        }

        // A full accept queue drops further SYNs, so the next connect pends.
        let listener = tcp_listener(localhost(0), SocketOptions::default(), 0).unwrap();
        let addr = listener.local_addr().unwrap();
        let mut queued = Vec::new();
        loop {
            match tcp_connect(
                addr,
                None,
                SocketOptions::default(),
                Duration::from_millis(200),
            ) {
                Ok(s) => queued.push(s),
                Err(TcpConnectError::Deferred { cause, .. })
                    if cause.kind() == io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(e) => panic!("filling the accept queue: {e}"),
            }
        }

        // SAFETY: `pthread_self` has no preconditions.
        let me = unsafe { libc::pthread_self() };
        let done = Arc::new(AtomicBool::new(false));
        let kicker = {
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(100));
                    // SAFETY: `me` is the test thread, alive until the join below.
                    unsafe { libc::pthread_kill(me, libc::SIGUSR2) };
                }
            })
        };
        let start = std::time::Instant::now();
        let err =
            tcp_connect(addr, None, SocketOptions::default(), Duration::from_secs(5)).unwrap_err();
        done.store(true, Ordering::SeqCst);
        kicker.join().unwrap();

        match &err {
            TcpConnectError::Deferred { in_progress, cause } => {
                assert_eq!(in_progress.raw_os_error(), Some(libc::EINPROGRESS));
                assert_eq!(cause.raw_os_error(), Some(libc::EINTR));
            }
            other => panic!("expected Deferred, got {other:?}"),
        }
        assert!(start.elapsed() < Duration::from_secs(4));
    }

    /// connect() failing outright is C's `sockerrmsg` with no poll behind it
    /// (drvAsynIPPort.c:534-539): a multicast destination is unreachable at once.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_connect_that_fails_at_once_is_not_deferred() {
        let remote = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(224, 0, 0, 1), 1));
        match tcp_connect(
            remote,
            None,
            SocketOptions::default(),
            Duration::from_secs(5),
        ) {
            Err(TcpConnectError::Connect(e)) => {
                assert_eq!(e.raw_os_error(), Some(libc::ENETUNREACH));
            }
            other => panic!("expected Connect, got {other:?}"),
        }
    }

    /// The bounded connect switches the socket non-blocking for its poll; the
    /// stream handed back is blocking again, like any `TcpStream::connect`.
    /// That is libcom's contract, not C's (asyn keeps its fd non-blocking and
    /// sets that mode itself after the connect).
    #[cfg(unix)]
    #[test]
    fn tcp_connect_returns_a_blocking_stream() {
        use std::os::fd::AsRawFd;

        let listener = tcp_listener(localhost(0), SocketOptions::default(), 8).unwrap();
        let client = tcp_connect(
            listener.local_addr().unwrap(),
            None,
            SocketOptions::default(),
            Duration::from_secs(5),
        )
        .unwrap();
        // SAFETY: F_GETFL on a live fd owned by `client`.
        let flags = unsafe { libc::fcntl(client.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(flags & libc::O_NONBLOCK, 0);
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
