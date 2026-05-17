//! TCP/UDP port driver (drvAsynIPPort equivalent).
//!
//! Supports TCP, UDP, and Unix domain sockets, with protocol suffixes
//! matching C asyn's `drvAsynIPPortConfigure` specification format.

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::time::Duration;

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::exception::AsynException;
use crate::interpose::{EomReason, OctetNext, OctetReadResult};
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::trace::TraceMask;
use crate::user::AsynUser;
use crate::{asyn_trace, asyn_trace_io};

/// IP transport protocol.
///
/// Mirrors C asyn `drvAsynIPPort.c::parseHostInfo` (lines 356-391)
/// protocol suffix dispatch verbatim:
///
/// - `TCP` or no suffix → blocking TCP (SOCK_STREAM)
/// - `TCP&` → TCP + `SO_REUSEPORT` (NOT non-blocking — see C
///   line 360-363 setting `FLAG_SO_REUSEPORT`)
/// - `UDP` → connected UDP (SOCK_DGRAM)
/// - `UDP&` → UDP + `SO_REUSEPORT` (C line 375-378, NOT broadcast)
/// - `UDP*` → UDP + `SO_BROADCAST` (C line 379-382, NOT multicast)
/// - `UDP*&` → UDP + `SO_BROADCAST` + `SO_REUSEPORT` (C line 383-387)
/// - `unix://path` → Unix domain socket (cfg(unix) only)
/// - `HTTP` → TCP + `FLAG_CONNECT_PER_TRANSACTION` (C line 368-371)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IpProtocol {
    #[default]
    Tcp,
    /// TCP + `SO_REUSEPORT` (`tcp&` in C asyn). C parity:
    /// `FLAG_SO_REUSEPORT` set, no other flags. NOT non-blocking.
    TcpReusePort,
    Udp,
    /// UDP + `SO_REUSEPORT` (`udp&` in C asyn).
    UdpReusePort,
    /// UDP + `SO_BROADCAST` (`udp*` in C asyn).
    UdpBroadcast,
    /// UDP + `SO_BROADCAST` + `SO_REUSEPORT` (`udp*&` in C asyn).
    UdpBroadcastReusePort,
    /// Unix domain socket (unix://path).
    Unix,
    /// HTTP: TCP with connect-per-transaction (C parity:
    /// `FLAG_CONNECT_PER_TRANSACTION` from line 368-371).
    Http,
}

/// Configuration for an IP port connection.
#[derive(Debug, Clone)]
pub struct IpPortConfig {
    pub host: String,
    pub port: u16,
    pub local_port: Option<u16>,
    pub protocol: IpProtocol,
    pub connect_timeout: Duration,
    pub no_delay: bool,
}

impl IpPortConfig {
    /// Parse a connection specification string.
    ///
    /// Formats:
    /// - `"hostname:port[:localPort] [TCP|UDP|TCP&|UDP&|UDP*]"`
    /// - `"[::1]:port[:localPort] [proto]"` (IPv6 in brackets)
    /// - `"unix:///path/to/socket"`
    ///
    /// Protocol suffixes are case-insensitive.
    pub fn parse(spec: &str) -> AsynResult<Self> {
        let spec = spec.trim();

        // Check for unix:// prefix
        if let Some(path) = spec
            .strip_prefix("unix://")
            .or_else(|| spec.strip_prefix("UNIX://"))
        {
            if path.is_empty() {
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: "empty unix socket path".into(),
                });
            }
            return Ok(Self {
                host: path.to_string(),
                port: 0,
                local_port: None,
                protocol: IpProtocol::Unix,
                connect_timeout: Duration::from_secs(5),
                no_delay: false,
            });
        }

        // Parse protocol suffix (case-insensitive)
        let (addr_part, proto) = parse_protocol_suffix(spec);
        let addr_part = addr_part.trim();

        // Parse host:port[:localPort], supporting IPv6 brackets
        let (host, port, local_port) = parse_host_port(addr_part, spec)?;

        Ok(Self {
            host,
            port,
            local_port,
            protocol: proto,
            connect_timeout: Duration::from_secs(5),
            no_delay: true,
        })
    }
}

/// Parse the protocol suffix from the end of a spec string.
/// Returns (remaining_addr_part, protocol).
///
/// Order matters: longest suffix first ("UDP*&" before "UDP*"
/// before "UDP", and "TCP&" before "TCP") because we use
/// `ends_with` and the first match wins.
fn parse_protocol_suffix(spec: &str) -> (&str, IpProtocol) {
    let upper = spec.to_ascii_uppercase();

    for (suffix, proto) in [
        (" UDP*&", IpProtocol::UdpBroadcastReusePort),
        (" UDP&", IpProtocol::UdpReusePort),
        (" UDP*", IpProtocol::UdpBroadcast),
        (" TCP&", IpProtocol::TcpReusePort),
        (" HTTP", IpProtocol::Http),
        (" TCP", IpProtocol::Tcp),
        (" UDP", IpProtocol::Udp),
    ] {
        if upper.ends_with(suffix) {
            return (&spec[..spec.len() - suffix.len()], proto);
        }
    }
    (spec, IpProtocol::Tcp)
}

/// Parse `host:port[:localPort]` with IPv6 bracket support.
fn parse_host_port(addr_part: &str, orig_spec: &str) -> AsynResult<(String, u16, Option<u16>)> {
    // IPv6 bracket format: [::1]:port[:localPort]
    if addr_part.starts_with('[') {
        let bracket_end = addr_part.find(']').ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("missing closing bracket in IPv6 address: '{orig_spec}'"),
        })?;
        let host = addr_part[1..bracket_end].to_string();
        if host.is_empty() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "empty IPv6 address".into(),
            });
        }
        let rest = &addr_part[bracket_end + 1..];
        let rest = rest.strip_prefix(':').ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("expected ':port' after IPv6 bracket: '{orig_spec}'"),
        })?;
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        let port: u16 = parts[0].parse().map_err(|_| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("invalid port number: '{}'", parts[0]),
        })?;
        let local_port = if parts.len() > 1 {
            Some(parts[1].parse::<u16>().map_err(|_| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("invalid local port: '{}'", parts[1]),
            })?)
        } else {
            None
        };
        return Ok((host, port, local_port));
    }

    // Standard format: host:port[:localPort]
    let parts: Vec<&str> = addr_part.splitn(3, ':').collect();
    if parts.len() < 2 {
        return Err(AsynError::Status {
            status: AsynStatus::Error,
            message: format!("invalid IP port spec: expected host:port, got '{orig_spec}'"),
        });
    }

    let host = parts[0].to_string();
    if host.is_empty() {
        return Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "empty hostname".into(),
        });
    }

    let port: u16 = parts[1].parse().map_err(|_| AsynError::Status {
        status: AsynStatus::Error,
        message: format!("invalid port number: '{}'", parts[1]),
    })?;

    let local_port = if parts.len() > 2 {
        Some(parts[2].parse::<u16>().map_err(|_| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("invalid local port: '{}'", parts[2]),
        })?)
    } else {
        None
    };

    Ok((host, port, local_port))
}

/// Internal I/O state holding the transport socket.
enum IpIoInner {
    Tcp(TcpStream),
    Udp(UdpSocket),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

/// Write all data with retry on WouldBlock/Interrupted, enforcing a deadline.
fn write_with_retry(
    stream: &mut impl Write,
    data: &[u8],
    deadline: std::time::Instant,
) -> AsynResult<()> {
    let mut offset = 0;
    while offset < data.len() {
        if std::time::Instant::now() > deadline {
            return Err(AsynError::Status {
                status: AsynStatus::Timeout,
                message: "write timeout".into(),
            });
        }
        match stream.write(&data[offset..]) {
            Ok(0) => {
                return Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "write returned 0 bytes".into(),
                });
            }
            Ok(n) => offset += n,
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(AsynError::Io(e)),
        }
    }
    Ok(())
}

struct IpIoState {
    inner: Option<IpIoInner>,
}

impl OctetNext for IpIoState {
    fn read(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
        let inner = self.inner.as_mut().ok_or_else(|| AsynError::Status {
            status: AsynStatus::Disconnected,
            message: "not connected".into(),
        })?;
        match inner {
            IpIoInner::Tcp(stream) => {
                stream.set_read_timeout(Some(user.timeout))?;
                match stream.read(buf) {
                    Ok(0) => Err(AsynError::Status {
                        status: AsynStatus::Disconnected,
                        message: "EOF".into(),
                    }),
                    Ok(n) => Ok(OctetReadResult {
                        nbytes_transferred: n,
                        // C parity: CNT means the requested count was
                        // reached. A short read leaves the reason empty
                        // so the EOS interpose keeps reading.
                        eom_reason: if n >= buf.len() {
                            EomReason::CNT
                        } else {
                            EomReason::empty()
                        },
                    }),
                    Err(e)
                        if e.kind() == std::io::ErrorKind::TimedOut
                            || e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        Err(AsynError::Status {
                            status: AsynStatus::Timeout,
                            message: "read timeout".into(),
                        })
                    }
                    Err(e) => Err(AsynError::Io(e)),
                }
            }
            IpIoInner::Udp(socket) => {
                socket.set_read_timeout(Some(user.timeout))?;
                match socket.recv(buf) {
                    Ok(0) => Err(AsynError::Status {
                        status: AsynStatus::Disconnected,
                        message: "EOF".into(),
                    }),
                    Ok(n) => Ok(OctetReadResult {
                        nbytes_transferred: n,
                        // C parity: CNT means the requested count was
                        // reached. A short read leaves the reason empty
                        // so the EOS interpose keeps reading.
                        eom_reason: if n >= buf.len() {
                            EomReason::CNT
                        } else {
                            EomReason::empty()
                        },
                    }),
                    Err(e)
                        if e.kind() == std::io::ErrorKind::TimedOut
                            || e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        Err(AsynError::Status {
                            status: AsynStatus::Timeout,
                            message: "read timeout".into(),
                        })
                    }
                    Err(e) => Err(AsynError::Io(e)),
                }
            }
            #[cfg(unix)]
            IpIoInner::Unix(stream) => {
                stream.set_read_timeout(Some(user.timeout))?;
                match stream.read(buf) {
                    Ok(0) => Err(AsynError::Status {
                        status: AsynStatus::Disconnected,
                        message: "EOF".into(),
                    }),
                    Ok(n) => Ok(OctetReadResult {
                        nbytes_transferred: n,
                        // C parity: CNT means the requested count was
                        // reached. A short read leaves the reason empty
                        // so the EOS interpose keeps reading.
                        eom_reason: if n >= buf.len() {
                            EomReason::CNT
                        } else {
                            EomReason::empty()
                        },
                    }),
                    Err(e)
                        if e.kind() == std::io::ErrorKind::TimedOut
                            || e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        Err(AsynError::Status {
                            status: AsynStatus::Timeout,
                            message: "read timeout".into(),
                        })
                    }
                    Err(e) => Err(AsynError::Io(e)),
                }
            }
        }
    }

    fn write(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let inner = self.inner.as_mut().ok_or_else(|| AsynError::Status {
            status: AsynStatus::Disconnected,
            message: "not connected".into(),
        })?;
        let deadline = std::time::Instant::now() + user.timeout;
        match inner {
            IpIoInner::Tcp(stream) => {
                stream.set_write_timeout(Some(user.timeout))?;
                write_with_retry(stream, data, deadline)?;
            }
            IpIoInner::Udp(socket) => {
                socket.set_write_timeout(Some(user.timeout))?;
                socket.send(data)?;
            }
            #[cfg(unix)]
            IpIoInner::Unix(stream) => {
                stream.set_write_timeout(Some(user.timeout))?;
                write_with_retry(stream, data, deadline)?;
            }
        }
        Ok(data.len())
    }

    /// Base-layer flush — C parity with `drvAsynIPPort.c::flushIt`,
    /// which does a non-blocking `recv` loop discarding every byte
    /// already queued in the socket's receive buffer (the serial
    /// driver achieves the same with `tcflush(TCIFLUSH)`).
    ///
    /// This is the *innermost* `OctetNext` in the interpose chain, so
    /// when `DrvAsynIPPort::io_flush` routes through
    /// `OctetInterposeStack::dispatch_flush`, each interpose layer's
    /// `flush` (e.g. `EosInterpose::flush`, which resets its persistent
    /// `in_buf`) runs first and finally delegates here to drain the OS
    /// socket. This mirrors C, where `asynInterposeEos.c::flushIt`
    /// resets `inBufHead/inBufTail/eosInMatch` and then calls the
    /// lower-level (IP port) `flush`.
    ///
    /// EOF / connection-reset during the drain is treated as benign:
    /// there is nothing to flush on a dead socket and the subsequent
    /// write/read will surface the disconnect.
    fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
        let mut scratch = [0u8; 4096];
        match self.inner.as_mut() {
            Some(IpIoInner::Tcp(stream)) => {
                let restore = stream.set_nonblocking(true);
                loop {
                    match stream.read(&mut scratch) {
                        Ok(0) => break, // EOF — nothing left to drain
                        Ok(_) => continue,
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break, // reset/other — write/read will report it
                    }
                }
                if restore.is_ok() {
                    let _ = stream.set_nonblocking(false);
                }
            }
            Some(IpIoInner::Udp(socket)) => {
                let restore = socket.set_nonblocking(true);
                loop {
                    match socket.recv(&mut scratch) {
                        Ok(_) => continue,
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                if restore.is_ok() {
                    let _ = socket.set_nonblocking(false);
                }
            }
            #[cfg(unix)]
            Some(IpIoInner::Unix(stream)) => {
                let restore = stream.set_nonblocking(true);
                loop {
                    match stream.read(&mut scratch) {
                        Ok(0) => break,
                        Ok(_) => continue,
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                if restore.is_ok() {
                    let _ = stream.set_nonblocking(false);
                }
            }
            None => {}
        }
        Ok(())
    }
}

/// TCP/UDP port driver.
pub struct DrvAsynIPPort {
    base: PortDriverBase,
    config: IpPortConfig,
    io: IpIoState,
    /// Auto-disconnect when read times out (default: false).
    disconnect_on_read_timeout: bool,
}

impl DrvAsynIPPort {
    /// Create a new IP port driver.
    ///
    /// The driver starts disconnected with `auto_connect = true` and `can_block = true`.
    pub fn new(port_name: &str, config_str: &str) -> AsynResult<Self> {
        let config = IpPortConfig::parse(config_str)?;
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags {
                multi_device: false,
                can_block: true,
                destructible: true,
            },
        );
        base.connected = false;
        base.auto_connect = true;

        Ok(Self {
            base,
            config,
            io: IpIoState { inner: None },
            disconnect_on_read_timeout: false,
        })
    }

    /// Push an interpose layer onto the octet I/O stack.
    pub fn push_interpose(&mut self, layer: Box<dyn crate::interpose::OctetInterpose>) {
        self.base.push_octet_interpose(layer);
    }

    fn connect_tcp(&mut self) -> AsynResult<TcpStream> {
        let addr_str = format!("{}:{}", self.config.host, self.config.port);

        if let Some(local_port) = self.config.local_port {
            use std::net::ToSocketAddrs;
            // Resolve the remote like the no-local-port branch — the old
            // code used `SocketAddr::parse`, which only accepts literal
            // IPs, so a hostname target (valid per the config parser)
            // failed, as did any IPv6 target (domain was forced to IPV4).
            let addrs: Vec<std::net::SocketAddr> = addr_str
                .to_socket_addrs()
                .map_err(|e| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("failed to resolve '{addr_str}': {e}"),
                })?
                .collect();

            let mut last_err: Option<AsynError> = None;
            for remote_addr in &addrs {
                let (domain, local_str) = if remote_addr.is_ipv6() {
                    (socket2::Domain::IPV6, format!("[::]:{local_port}"))
                } else {
                    (socket2::Domain::IPV4, format!("0.0.0.0:{local_port}"))
                };
                let socket = match socket2::Socket::new(
                    domain,
                    socket2::Type::STREAM,
                    Some(socket2::Protocol::TCP),
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        last_err = Some(AsynError::Io(e));
                        continue;
                    }
                };
                if let Err(e) = socket.set_reuse_address(true) {
                    last_err = Some(AsynError::Io(e));
                    continue;
                }
                let local_addr: std::net::SocketAddr = match local_str.parse() {
                    Ok(a) => a,
                    Err(_) => {
                        last_err = Some(AsynError::Status {
                            status: AsynStatus::Error,
                            message: format!("invalid local address: {local_str}"),
                        });
                        continue;
                    }
                };
                if let Err(e) = socket.bind(&local_addr.into()) {
                    last_err = Some(AsynError::Io(e));
                    continue;
                }
                match socket.connect_timeout(&(*remote_addr).into(), self.config.connect_timeout) {
                    Ok(()) => return Ok(TcpStream::from(socket)),
                    Err(e) => last_err = Some(AsynError::Io(e)),
                }
            }
            Err(last_err.unwrap_or_else(|| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("no addresses found for '{addr_str}'"),
            }))
        } else {
            use std::net::ToSocketAddrs;
            let addrs: Vec<std::net::SocketAddr> = addr_str
                .to_socket_addrs()
                .map_err(|e| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("failed to resolve '{addr_str}': {e}"),
                })?
                .collect();

            let mut last_err = None;
            let mut connected_stream = None;
            for addr in &addrs {
                match TcpStream::connect_timeout(addr, self.config.connect_timeout) {
                    Ok(s) => {
                        connected_stream = Some(s);
                        break;
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            connected_stream.ok_or_else(|| {
                if let Some(e) = last_err {
                    AsynError::Io(e)
                } else {
                    AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("no addresses found for '{addr_str}'"),
                    }
                }
            })
        }
    }

    fn connect_udp(&mut self) -> AsynResult<UdpSocket> {
        let bind_addr = if let Some(local_port) = self.config.local_port {
            format!("0.0.0.0:{local_port}")
        } else {
            "0.0.0.0:0".to_string()
        };
        let socket = UdpSocket::bind(&bind_addr)?;
        let remote = format!("{}:{}", self.config.host, self.config.port);
        socket.connect(&remote).map_err(|e| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("UDP connect to '{remote}': {e}"),
        })?;
        Ok(socket)
    }

    /// UDP variant builder — applies any combination of `SO_BROADCAST`
    /// and `SO_REUSEPORT` requested by the protocol suffix. Mirrors C
    /// asyn `connectIt` UDP socket option flow (drvAsynIPPort.c
    /// branches on `tty->flags & FLAG_BROADCAST` and `FLAG_SO_REUSEPORT`).
    fn connect_udp_with_options(
        &mut self,
        broadcast: bool,
        reuse_port: bool,
    ) -> AsynResult<UdpSocket> {
        let socket = self.connect_udp()?;
        if broadcast {
            socket.set_broadcast(true)?;
        }
        if reuse_port {
            #[cfg(unix)]
            {
                // SockRef borrows the std socket's fd without taking
                // ownership — set_reuse_port goes through socket2's
                // setsockopt(SO_REUSEPORT) wrapper.
                let sref = socket2::SockRef::from(&socket);
                sref.set_reuse_port(true).map_err(|e| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("UDP SO_REUSEPORT failed: {e}"),
                })?;
            }
        }
        Ok(socket)
    }

    #[cfg(unix)]
    fn connect_unix(&mut self) -> AsynResult<std::os::unix::net::UnixStream> {
        let stream = std::os::unix::net::UnixStream::connect(&self.config.host).map_err(|e| {
            AsynError::Status {
                status: AsynStatus::Error,
                message: format!("unix connect to '{}': {e}", self.config.host),
            }
        })?;
        Ok(stream)
    }
}

impl PortDriver for DrvAsynIPPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        match self.config.protocol {
            IpProtocol::Tcp | IpProtocol::TcpReusePort => {
                let stream = self.connect_tcp()?;
                if self.config.no_delay {
                    stream.set_nodelay(true)?;
                }
                if self.config.protocol == IpProtocol::TcpReusePort {
                    // tcp& in C asyn = TCP + SO_REUSEPORT (NOT
                    // non-blocking). Apply via SockRef on the std
                    // TcpStream so we don't churn the socket type.
                    #[cfg(unix)]
                    {
                        let sref = socket2::SockRef::from(&stream);
                        sref.set_reuse_port(true).map_err(|e| AsynError::Status {
                            status: AsynStatus::Error,
                            message: format!("TCP SO_REUSEPORT failed: {e}"),
                        })?;
                    }
                }
                self.io.inner = Some(IpIoInner::Tcp(stream));
            }
            IpProtocol::Udp => {
                let socket = self.connect_udp_with_options(false, false)?;
                self.io.inner = Some(IpIoInner::Udp(socket));
            }
            IpProtocol::UdpReusePort => {
                let socket = self.connect_udp_with_options(false, true)?;
                self.io.inner = Some(IpIoInner::Udp(socket));
            }
            IpProtocol::UdpBroadcast => {
                let socket = self.connect_udp_with_options(true, false)?;
                self.io.inner = Some(IpIoInner::Udp(socket));
            }
            IpProtocol::UdpBroadcastReusePort => {
                let socket = self.connect_udp_with_options(true, true)?;
                self.io.inner = Some(IpIoInner::Udp(socket));
            }
            #[cfg(unix)]
            IpProtocol::Unix => {
                let stream = self.connect_unix()?;
                self.io.inner = Some(IpIoInner::Unix(stream));
            }
            #[cfg(not(unix))]
            IpProtocol::Unix => {
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: "Unix domain sockets not supported on this platform".into(),
                });
            }
            IpProtocol::Http => {
                // C parity: HTTP uses TCP with connect-per-transaction semantics.
                // Connection is established here, but io_write_octet disconnects after
                // each write/read cycle.
                let stream = self.connect_tcp()?;
                stream.set_nodelay(true)?;
                self.io.inner = Some(IpIoInner::Tcp(stream));
            }
        }
        self.base.set_connected(true);
        asyn_trace!(
            Some(self.base.trace),
            &self.base.port_name,
            TraceMask::FLOW,
            "connected to {}:{} ({:?})",
            self.config.host,
            self.config.port,
            self.config.protocol
        );
        Ok(())
    }

    fn disconnect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        asyn_trace!(
            Some(self.base.trace),
            &self.base.port_name,
            TraceMask::FLOW,
            "disconnect"
        );
        self.io.inner = None;
        self.base.set_connected(false);
        Ok(())
    }

    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        // HTTP connect-per-transaction: reconnect if disconnected.
        // Surface the connect failure cause (DNS, refused, TLS reset)
        // rather than letting check_ready() mask it as a generic
        // "port disconnected".
        if self.config.protocol == IpProtocol::Http && !self.base.connected {
            self.connect(&AsynUser::default())?;
        }
        self.base.check_ready()?;
        let result = self
            .base
            .interpose_octet
            .dispatch_read(user, buf, &mut self.io);
        match result {
            Ok(r) => {
                asyn_trace_io!(
                    Some(self.base.trace),
                    &self.base.port_name,
                    TraceMask::IO_DRIVER,
                    &buf[..r.nbytes_transferred],
                    "read"
                );
                // HTTP: disconnect after each read (connect-per-transaction)
                if self.config.protocol == IpProtocol::Http {
                    self.io.inner = None;
                    self.base.set_connected(false);
                }
                Ok(r.nbytes_transferred)
            }
            Err(ref e) => {
                let is_timeout = matches!(
                    e,
                    AsynError::Status {
                        status: AsynStatus::Timeout,
                        ..
                    }
                );
                let is_disconnect = matches!(
                    e,
                    AsynError::Status {
                        status: AsynStatus::Disconnected,
                        ..
                    }
                );
                // C parity: auto-disconnect on:
                // 1. disconnectOnReadTimeout AND timeout error
                // 2. Any real error that isn't timeout/WouldBlock (e.g. connection reset, EOF)
                let should_disconnect = (self.disconnect_on_read_timeout && is_timeout)
                    || is_disconnect
                    || (!is_timeout && matches!(e, AsynError::Io(_)));
                if should_disconnect && self.base.connected {
                    asyn_trace!(
                        Some(self.base.trace),
                        &self.base.port_name,
                        TraceMask::FLOW,
                        "read error, disconnecting: {e}"
                    );
                    self.io.inner = None;
                    self.base.set_connected(false);
                }
                result.map(|r| r.nbytes_transferred)
            }
        }
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
        // HTTP connect-per-transaction: reconnect if disconnected.
        // Surface the connect failure cause rather than masking it.
        if self.config.protocol == IpProtocol::Http && !self.base.connected {
            self.connect(&AsynUser::default())?;
        }
        self.base.check_ready()?;
        asyn_trace_io!(
            Some(self.base.trace),
            &self.base.port_name,
            TraceMask::IO_DRIVER,
            data,
            "write"
        );
        self.base
            .interpose_octet
            .dispatch_write(user, data, &mut self.io)?;
        Ok(())
    }

    fn io_flush(&mut self, user: &mut AsynUser) -> AsynResult<()> {
        // C parity: asynOctetSyncIO::writeRead (asynOctetSyncIO.c:~250)
        // calls flushIt before write+read so the post-write read
        // returns only the response to *this* command.
        //
        // The flush MUST traverse the interpose chain, not just the OS
        // socket. C `asynInterposeEos.c::flushIt` resets the EOS
        // interpose's persistent input buffer
        // (`inBufHead/inBufTail/eosInMatch`) and then calls the
        // lower-level `flush`. An earlier Rust version drained the OS
        // socket directly and never reset the `EosInterpose`'s
        // `in_buf` — so bytes already buffered *inside* the interpose
        // from a prior read leaked into the next response after an
        // `OctetWriteRead`.
        //
        // Routing through `dispatch_flush` runs every interpose layer's
        // `flush` (resetting `EosInterpose::in_buf` etc.) and finally
        // reaches `IpIoState::flush`, which drains the OS socket's
        // receive buffer.
        self.base.interpose_octet.dispatch_flush(user, &mut self.io)
    }

    fn set_option(&mut self, key: &str, value: &str) -> AsynResult<()> {
        match key {
            "noDelay" => {
                let enabled = value == "Y" || value == "y" || value == "1" || value == "yes";
                self.config.no_delay = enabled;
                if let Some(IpIoInner::Tcp(ref stream)) = self.io.inner {
                    stream.set_nodelay(enabled)?;
                }
            }
            "disconnectOnReadTimeout" => {
                self.disconnect_on_read_timeout =
                    value == "Y" || value == "y" || value == "1" || value == "yes";
            }
            "hostInfo" => {
                // Mirror C drvAsynIPPort.c::parseHostInfo (lines 273-401):
                //
                //   if (fd != INVALID_SOCKET) {
                //       flags |= FLAG_SHUTDOWN;
                //       closeConnection(...);
                //       epicsThreadSleep(CLOSE_SOCKET_DELAY);   // 0.02s
                //   }
                //   ... full reparse: protocol, FLAG_BROADCAST,
                //       FLAG_SO_REUSEPORT, hostname, port, localPort
                //   flags &= ~FLAG_SHUTDOWN;
                //
                // Earlier this branch updated only host/port/local_port,
                // so a runtime switch like "udp tcp" or "udp& udp*"
                // left the previous protocol/flags active and the next
                // connect() bound the wrong socket type.
                //
                // We parse first (no observable state change on
                // parse error) and only then drop the live socket and
                // overwrite config.
                let new_config = IpPortConfig::parse(value)?;
                if self.io.inner.is_some() {
                    // Drop in-flight socket; matches C closeConnection.
                    self.io.inner = None;
                    // Owner-API: set_connected handles the edge-guarded
                    // fan-out, so a redundant call here is a no-op for
                    // listeners just like C's exceptionDisconnect.
                    self.base.set_connected(false);
                    // C's "if this delay is not present then the sockets
                    // are not always really closed cleanly" — same 20ms
                    // settle to ensure the kernel actually tears down
                    // the prior socket before we rebind on a fresh one
                    // (especially relevant for UDP+SO_REUSEPORT swaps).
                    std::thread::sleep(Duration::from_millis(20));
                }
                self.config.host = new_config.host;
                self.config.port = new_config.port;
                self.config.local_port = new_config.local_port;
                self.config.protocol = new_config.protocol;
                // connect_timeout / no_delay are first-set-only in C
                // too — parseHostInfo doesn't touch them.
            }
            _ => {
                self.base.options.insert(key.to_string(), value.to_string());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    // --- Config parsing tests ---

    #[test]
    fn test_parse_tcp_default() {
        let cfg = IpPortConfig::parse("localhost:5025").unwrap();
        assert_eq!(cfg.host, "localhost");
        assert_eq!(cfg.port, 5025);
        assert_eq!(cfg.protocol, IpProtocol::Tcp);
        assert_eq!(cfg.local_port, None);
    }

    #[test]
    fn test_parse_tcp_explicit() {
        let cfg = IpPortConfig::parse("192.168.1.1:8080 tcp").unwrap();
        assert_eq!(cfg.host, "192.168.1.1");
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.protocol, IpProtocol::Tcp);
    }

    #[test]
    fn test_parse_udp() {
        let cfg = IpPortConfig::parse("device:9000 udp").unwrap();
        assert_eq!(cfg.protocol, IpProtocol::Udp);
    }

    #[test]
    fn test_parse_local_port() {
        let cfg = IpPortConfig::parse("host:5025:4000").unwrap();
        assert_eq!(cfg.local_port, Some(4000));
    }

    #[test]
    fn test_parse_invalid_no_port() {
        assert!(IpPortConfig::parse("hostname_only").is_err());
    }

    #[test]
    fn test_parse_invalid_port_number() {
        assert!(IpPortConfig::parse("host:abc").is_err());
    }

    #[test]
    fn test_parse_empty_host() {
        assert!(IpPortConfig::parse(":5025").is_err());
    }

    // --- Driver creation tests ---

    #[test]
    fn test_driver_initial_state() {
        let drv = DrvAsynIPPort::new("iptest", "localhost:5025").unwrap();
        assert!(!drv.base().connected);
        assert!(drv.base().auto_connect);
        assert!(drv.base().flags.can_block);
    }

    // --- Integration tests with mock TCP server ---

    fn start_echo_server() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    #[test]
    fn test_connect_disconnect() {
        let (listener, port) = start_echo_server();
        let _handle = thread::spawn(move || {
            let _ = listener.accept();
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        let user = AsynUser::default();
        assert!(!drv.base().connected);

        drv.connect(&user).unwrap();
        assert!(drv.base().connected);

        drv.disconnect(&user).unwrap();
        assert!(!drv.base().connected);
    }

    #[test]
    fn test_read_write_octet_roundtrip() {
        let (listener, port) = start_echo_server();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let n = stream.read(&mut buf).unwrap();
            stream.write_all(&buf[..n]).unwrap();
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user, b"hello").unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 32];
        let n = drv.read_octet(&user, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");

        handle.join().unwrap();
    }

    #[test]
    fn test_read_timeout() {
        let (listener, port) = start_echo_server();
        let _handle = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_secs(5));
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_millis(100));
        let mut buf = [0u8; 32];
        let err = drv.read_octet(&user, &mut buf).unwrap_err();
        match err {
            AsynError::Status {
                status: AsynStatus::Timeout,
                ..
            } => {}
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn test_server_disconnect_eof() {
        let (listener, port) = start_echo_server();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            drop(stream);
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        thread::sleep(Duration::from_millis(50));

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let mut buf = [0u8; 32];
        let err = drv.read_octet(&user, &mut buf).unwrap_err();
        match err {
            AsynError::Status {
                status: AsynStatus::Disconnected,
                ..
            } => {}
            other => panic!("expected Disconnected (EOF), got {other:?}"),
        }

        handle.join().unwrap();
    }

    #[test]
    fn test_partial_read() {
        let (listener, port) = start_echo_server();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"he").unwrap();
            stream.flush().unwrap();
            thread::sleep(Duration::from_millis(50));
            stream.write_all(b"llo").unwrap();
            stream.flush().unwrap();
            thread::sleep(Duration::from_millis(200));
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 32];
        let n1 = drv.read_octet(&user, &mut buf).unwrap();
        assert!(n1 > 0);
        assert!(n1 <= 5);

        handle.join().unwrap();
    }

    #[test]
    fn test_eos_interpose_with_tcp() {
        use crate::interpose::eos::{EosConfig, EosInterpose};

        let (listener, port) = start_echo_server();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"OK\r\n").unwrap();
            stream.flush().unwrap();
            thread::sleep(Duration::from_millis(200));
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        let eos = EosInterpose::new(EosConfig {
            input_eos: vec![b'\r', b'\n'],
            output_eos: vec![],
        });
        drv.push_interpose(Box::new(eos));

        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 32];
        let n = drv.read_octet(&user, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"OK");

        handle.join().unwrap();
    }

    #[test]
    fn test_read_write_when_disconnected() {
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:9999").unwrap();
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let mut buf = [0u8; 32];
        assert!(drv.read_octet(&user, &mut buf).is_err());
        let mut user = AsynUser::new(0);
        assert!(drv.write_octet(&mut user, b"hello").is_err());
    }

    #[test]
    fn test_set_option_nodelay() {
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:5025").unwrap();
        drv.set_option("noDelay", "Y").unwrap();
        assert!(drv.config.no_delay);
        drv.set_option("noDelay", "0").unwrap();
        assert!(!drv.config.no_delay);
    }

    // --- UDP tests ---

    #[test]
    fn test_udp_connect_and_roundtrip() {
        // Start a UDP echo server
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_port = server.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let mut buf = [0u8; 256];
            let (n, src) = server.recv_from(&mut buf).unwrap();
            server.send_to(&buf[..n], src).unwrap();
        });

        let mut drv =
            DrvAsynIPPort::new("udptest", &format!("127.0.0.1:{server_port} udp")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        assert!(drv.base().connected);

        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user, b"ping").unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 32];
        let n = drv.read_octet(&user, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"ping");

        handle.join().unwrap();
    }

    // --- disconnectOnReadTimeout tests ---

    #[test]
    fn test_disconnect_on_read_timeout() {
        let (listener, port) = start_echo_server();
        let _handle = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_secs(5));
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        drv.set_option("disconnectOnReadTimeout", "Y").unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        assert!(drv.base().connected);

        let user = AsynUser::new(0).with_timeout(Duration::from_millis(50));
        let mut buf = [0u8; 32];
        let _ = drv.read_octet(&user, &mut buf);
        assert!(!drv.base().connected);
    }

    // --- hostInfo option tests ---

    #[test]
    fn test_set_option_host_info() {
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:5025").unwrap();
        drv.set_option("hostInfo", "192.168.1.1:8080").unwrap();
        assert_eq!(drv.config.host, "192.168.1.1");
        assert_eq!(drv.config.port, 8080);
    }

    #[test]
    fn test_set_option_host_info_disconnects() {
        let (listener, port) = start_echo_server();
        let _handle = thread::spawn(move || {
            let _ = listener.accept();
            thread::sleep(Duration::from_secs(1));
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        assert!(drv.base().connected);

        drv.set_option("hostInfo", "127.0.0.1:9999").unwrap();
        assert!(!drv.base().connected);
        assert_eq!(drv.config.port, 9999);
    }

    /// hostInfo runtime reparse must update the protocol field too,
    /// matching C parseHostInfo (drvAsynIPPort.c:356-391). Previously
    /// only host/port/local_port were copied, so switching from TCP
    /// to UDP at runtime left the socket type unchanged.
    #[test]
    fn host_info_reparse_updates_protocol_and_flags() {
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:5025 tcp").unwrap();
        assert_eq!(drv.config.protocol, IpProtocol::Tcp);

        drv.set_option("hostInfo", "127.0.0.1:5026 udp").unwrap();
        assert_eq!(drv.config.protocol, IpProtocol::Udp);
        assert_eq!(drv.config.port, 5026);

        drv.set_option("hostInfo", "127.0.0.1:5027 udp*").unwrap();
        assert_eq!(drv.config.protocol, IpProtocol::UdpBroadcast);

        drv.set_option("hostInfo", "127.0.0.1:5028 udp&").unwrap();
        assert_eq!(drv.config.protocol, IpProtocol::UdpReusePort);

        drv.set_option("hostInfo", "127.0.0.1:5029 udp*&").unwrap();
        assert_eq!(drv.config.protocol, IpProtocol::UdpBroadcastReusePort);

        drv.set_option("hostInfo", "127.0.0.1:5030 tcp&").unwrap();
        assert_eq!(drv.config.protocol, IpProtocol::TcpReusePort);
    }

    /// hostInfo reparse must clear `local_port` when the new spec
    /// omits the second-colon field. C parseHostInfo only sets
    /// tty->localAddr when it parses a value (line 339-348); the
    /// previous Rust impl preserved the old local_port on omission,
    /// which silently bound the new socket to the prior outgoing port.
    #[test]
    fn host_info_reparse_clears_omitted_local_port() {
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:5025:12345 tcp").unwrap();
        assert_eq!(drv.config.local_port, Some(12345));
        drv.set_option("hostInfo", "127.0.0.1:5026 tcp").unwrap();
        assert_eq!(
            drv.config.local_port, None,
            "local_port must reset on hostInfo reparse"
        );
    }

    // --- Protocol suffix parsing — C parity (drvAsynIPPort.c:355-391) ---

    #[test]
    fn test_parse_tcp_reuse_port() {
        // C asyn `tcp&` = TCP + SO_REUSEPORT (NOT non-blocking).
        let cfg = IpPortConfig::parse("host:5025 TCP&").unwrap();
        assert_eq!(cfg.protocol, IpProtocol::TcpReusePort);
        assert_eq!(cfg.host, "host");
        assert_eq!(cfg.port, 5025);
    }

    #[test]
    fn test_parse_tcp_reuse_port_lowercase() {
        let cfg = IpPortConfig::parse("host:5025 tcp&").unwrap();
        assert_eq!(cfg.protocol, IpProtocol::TcpReusePort);
    }

    #[test]
    fn test_parse_udp_reuse_port() {
        // C asyn `udp&` = UDP + SO_REUSEPORT (NOT broadcast).
        let cfg = IpPortConfig::parse("192.168.1.10:9000 UDP&").unwrap();
        assert_eq!(cfg.protocol, IpProtocol::UdpReusePort);
        assert_eq!(cfg.host, "192.168.1.10");
    }

    #[test]
    fn test_parse_udp_broadcast() {
        // C asyn `udp*` = UDP + SO_BROADCAST (NOT multicast).
        let cfg = IpPortConfig::parse("192.168.1.255:9000 UDP*").unwrap();
        assert_eq!(cfg.protocol, IpProtocol::UdpBroadcast);
        assert_eq!(cfg.host, "192.168.1.255");
    }

    #[test]
    fn test_parse_udp_broadcast_reuse_port() {
        // C asyn `udp*&` = UDP + SO_BROADCAST + SO_REUSEPORT.
        let cfg = IpPortConfig::parse("192.168.1.255:9000 UDP*&").unwrap();
        assert_eq!(cfg.protocol, IpProtocol::UdpBroadcastReusePort);
    }

    #[test]
    fn test_parse_unix_socket() {
        let cfg = IpPortConfig::parse("unix:///tmp/asyn.sock").unwrap();
        assert_eq!(cfg.protocol, IpProtocol::Unix);
        assert_eq!(cfg.host, "/tmp/asyn.sock");
        assert_eq!(cfg.port, 0);
    }

    #[test]
    fn test_parse_unix_empty_path() {
        assert!(IpPortConfig::parse("unix://").is_err());
    }

    #[test]
    fn test_parse_ipv6_brackets() {
        let cfg = IpPortConfig::parse("[::1]:5025").unwrap();
        assert_eq!(cfg.host, "::1");
        assert_eq!(cfg.port, 5025);
        assert_eq!(cfg.protocol, IpProtocol::Tcp);
    }

    #[test]
    fn test_parse_ipv6_with_local_port() {
        let cfg = IpPortConfig::parse("[::1]:5025:4000").unwrap();
        assert_eq!(cfg.host, "::1");
        assert_eq!(cfg.port, 5025);
        assert_eq!(cfg.local_port, Some(4000));
    }

    #[test]
    fn test_parse_ipv6_with_proto() {
        let cfg = IpPortConfig::parse("[fe80::1]:9000 UDP").unwrap();
        assert_eq!(cfg.host, "fe80::1");
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.protocol, IpProtocol::Udp);
    }

    #[test]
    fn test_parse_case_insensitive() {
        assert_eq!(
            IpPortConfig::parse("h:1 Tcp").unwrap().protocol,
            IpProtocol::Tcp
        );
        assert_eq!(
            IpPortConfig::parse("h:1 Udp").unwrap().protocol,
            IpProtocol::Udp
        );
        assert_eq!(
            IpPortConfig::parse("h:1 Tcp&").unwrap().protocol,
            IpProtocol::TcpReusePort
        );
        assert_eq!(
            IpPortConfig::parse("h:1 Udp&").unwrap().protocol,
            IpProtocol::UdpReusePort
        );
        assert_eq!(
            IpPortConfig::parse("h:1 Udp*").unwrap().protocol,
            IpProtocol::UdpBroadcast
        );
        assert_eq!(
            IpPortConfig::parse("h:1 Udp*&").unwrap().protocol,
            IpProtocol::UdpBroadcastReusePort
        );
    }

    // --- Unix socket integration test ---

    #[cfg(unix)]
    #[test]
    fn test_unix_socket_connect_roundtrip() {
        use std::os::unix::net::UnixListener;

        let sock_path = format!("/tmp/asyn_test_{}.sock", std::process::id());
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();

        let sock_path2 = sock_path.clone();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let n = stream.read(&mut buf).unwrap();
            stream.write_all(&buf[..n]).unwrap();
        });

        let mut drv = DrvAsynIPPort::new("unixtest", &format!("unix://{sock_path}")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        assert!(drv.base().connected);

        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user, b"unix_hello").unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 32];
        let n = drv.read_octet(&user, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"unix_hello");

        handle.join().unwrap();
        let _ = std::fs::remove_file(&sock_path2);
    }

    // --- io_flush input-drain test (BUG 1 regression) ---

    /// `io_flush` must drain stale bytes already queued on the socket's
    /// receive buffer, matching C asyn `asynOctetSyncIO::writeRead`'s
    /// pre-write flush. Pre-fix `DrvAsynIPPort` had no `io_flush`
    /// override, so the trait default no-op left stale input in place
    /// and a subsequent read returned it instead of the fresh response.
    #[test]
    fn io_flush_drains_stale_tcp_input() {
        let (listener, port) = start_echo_server();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Pre-existing stale bytes on the warm line.
            stream.write_all(b"STALE_PROMPT>").unwrap();
            stream.flush().unwrap();
            thread::sleep(Duration::from_millis(100));
            // After the flush + client write, send the real response.
            let mut buf = [0u8; 64];
            let n = stream.read(&mut buf).unwrap();
            assert_eq!(&buf[..n], b"CMD");
            stream.write_all(b"RESPONSE").unwrap();
            stream.flush().unwrap();
            thread::sleep(Duration::from_millis(100));
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        // Let the stale bytes land in the socket's receive buffer.
        thread::sleep(Duration::from_millis(50));

        // Flush should discard "STALE_PROMPT>".
        let mut fuser = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.io_flush(&mut fuser).unwrap();

        // Write the command and read the response — must be "RESPONSE",
        // not the stale prompt.
        let mut wuser = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut wuser, b"CMD").unwrap();
        let ruser = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 64];
        let n = drv.read_octet(&ruser, &mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            b"RESPONSE",
            "io_flush must drain stale input; got {:?}",
            String::from_utf8_lossy(&buf[..n])
        );

        handle.join().unwrap();
    }

    /// BUG 2 regression: `io_flush` must also reset the EOS interpose's
    /// persistent input buffer, not just drain the OS socket. C
    /// `asynInterposeEos.c::flushIt` resets `inBufHead/inBufTail/
    /// eosInMatch`. If `io_flush` only drains the socket, bytes already
    /// buffered *inside* the EOS interpose from a prior read leak into
    /// the next response.
    ///
    /// Scenario: the server sends a long line "OLD_LINE_DATA\n" while
    /// the client reads it with a tiny user buffer, so the EOS layer's
    /// internal `in_buf` ends up holding the unconsumed tail. Then
    /// `io_flush` runs (as `asynOctetSyncIO::writeRead` would before a
    /// command), the client writes "CMD", and the server replies
    /// "NEW\n". The post-flush read must return "NEW", not the leftover
    /// tail of "OLD_LINE_DATA".
    #[test]
    fn io_flush_resets_eos_interpose_buffer() {
        use crate::interpose::eos::{EosConfig, EosInterpose};

        let (listener, port) = start_echo_server();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Stale line on the warm connection.
            stream.write_all(b"OLD_LINE_DATA\n").unwrap();
            stream.flush().unwrap();
            thread::sleep(Duration::from_millis(150));
            // Real response after the client's command.
            let mut buf = [0u8; 64];
            let n = stream.read(&mut buf).unwrap();
            assert_eq!(&buf[..n], b"CMD");
            stream.write_all(b"NEW\n").unwrap();
            stream.flush().unwrap();
            thread::sleep(Duration::from_millis(150));
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        drv.push_interpose(Box::new(EosInterpose::new(EosConfig {
            input_eos: vec![b'\n'],
            output_eos: vec![],
        })));

        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        thread::sleep(Duration::from_millis(50));

        // Read into a tiny buffer: the EOS layer reads "OLD_LINE_DATA\n"
        // from the socket into its 2048-byte in_buf, but can only hand
        // back 4 bytes ("OLD_") — the rest stays buffered inside the
        // interpose.
        let ruser = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut small = [0u8; 4];
        let n = drv.read_octet(&ruser, &mut small).unwrap();
        assert_eq!(&small[..n], b"OLD_");

        // Flush must clear BOTH the socket AND the interpose buffer.
        let mut fuser = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.io_flush(&mut fuser).unwrap();

        // Command + response cycle. If the interpose buffer was not
        // reset, this read returns the leftover "LINE" instead of "NEW".
        let mut wuser = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut wuser, b"CMD").unwrap();
        let ruser2 = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 64];
        let n = drv.read_octet(&ruser2, &mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            b"NEW",
            "io_flush must reset the EOS interpose buffer; got {:?}",
            String::from_utf8_lossy(&buf[..n])
        );

        handle.join().unwrap();
    }

    /// `io_flush` on a disconnected port is a benign no-op (no socket
    /// to drain).
    #[test]
    fn io_flush_noop_when_disconnected() {
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:9999").unwrap();
        let mut user = AsynUser::new(0);
        drv.io_flush(&mut user).unwrap();
    }

    // --- UDP broadcast flag test ---

    /// `UDP*` (broadcast suffix per C asyn) parses correctly and a
    /// driver can be constructed against a broadcast address.
    /// Pre-fix this test asserted UdpBroadcast for `UDP&`, which was
    /// the protocol-suffix swap bug.
    #[test]
    fn test_udp_broadcast_flag() {
        let cfg = IpPortConfig::parse("255.255.255.255:9000 UDP*").unwrap();
        let drv = DrvAsynIPPort::new("bcast_test", "255.255.255.255:9000 UDP*").unwrap();
        assert_eq!(cfg.protocol, IpProtocol::UdpBroadcast);
        assert_eq!(drv.config.protocol, IpProtocol::UdpBroadcast);
    }
}
