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
/// Matches C asyn's protocol suffix conventions:
/// - `TCP` or no suffix → blocking TCP
/// - `TCP&` → non-blocking TCP (connect + poll)
/// - `UDP` → connected UDP
/// - `UDP&` → UDP with SO_BROADCAST
/// - `UDP*` → UDP multicast
/// - `unix://path` → Unix domain socket (cfg(unix) only)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IpProtocol {
    #[default]
    Tcp,
    /// Non-blocking TCP (TCP&): uses O_NONBLOCK + poll for connect.
    TcpNonBlocking,
    Udp,
    /// UDP with SO_BROADCAST enabled (UDP&).
    UdpBroadcast,
    /// UDP multicast (UDP*).
    UdpMulticast,
    /// Unix domain socket (unix://path).
    Unix,
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
fn parse_protocol_suffix(spec: &str) -> (&str, IpProtocol) {
    let upper = spec.to_ascii_uppercase();

    // Check multi-char suffixes first (order matters: "UDP&" before "UDP")
    for (suffix, proto) in [
        (" TCP&", IpProtocol::TcpNonBlocking),
        (" UDP&", IpProtocol::UdpBroadcast),
        (" UDP*", IpProtocol::UdpMulticast),
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
                        eom_reason: EomReason::CNT,
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
                        eom_reason: EomReason::CNT,
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
                        eom_reason: EomReason::CNT,
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
        match inner {
            IpIoInner::Tcp(stream) => {
                stream.set_write_timeout(Some(user.timeout))?;
                stream.write_all(data)?;
            }
            IpIoInner::Udp(socket) => {
                socket.set_write_timeout(Some(user.timeout))?;
                socket.send(data)?;
            }
            #[cfg(unix)]
            IpIoInner::Unix(stream) => {
                stream.set_write_timeout(Some(user.timeout))?;
                stream.write_all(data)?;
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
        match self.inner.as_mut() {
            Some(IpIoInner::Tcp(stream)) => stream.flush()?,
            #[cfg(unix)]
            Some(IpIoInner::Unix(stream)) => stream.flush()?,
            _ => {}
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
            let socket = socket2::Socket::new(
                socket2::Domain::IPV4,
                socket2::Type::STREAM,
                Some(socket2::Protocol::TCP),
            )?;
            socket.set_reuse_address(true)?;
            let local_addr: std::net::SocketAddr = format!("0.0.0.0:{local_port}")
                .parse()
                .map_err(|_| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("invalid local address: 0.0.0.0:{local_port}"),
                })?;
            socket.bind(&local_addr.into())?;

            let remote_addr: std::net::SocketAddr =
                addr_str
                    .parse()
                    .map_err(|e: std::net::AddrParseError| AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("invalid remote address '{addr_str}': {e}"),
                    })?;
            socket.connect_timeout(&remote_addr.into(), self.config.connect_timeout)?;
            Ok(TcpStream::from(socket))
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

    fn connect_udp_broadcast(&mut self) -> AsynResult<UdpSocket> {
        let socket = self.connect_udp()?;
        socket.set_broadcast(true)?;
        Ok(socket)
    }

    fn connect_udp_multicast(&mut self) -> AsynResult<UdpSocket> {
        let bind_addr = if let Some(local_port) = self.config.local_port {
            format!("0.0.0.0:{local_port}")
        } else {
            format!("0.0.0.0:{}", self.config.port)
        };
        let socket = UdpSocket::bind(&bind_addr)?;
        // Try to parse as IPv4 multicast address
        if let Ok(mcast_addr) = self.config.host.parse::<std::net::Ipv4Addr>() {
            socket
                .join_multicast_v4(&mcast_addr, &std::net::Ipv4Addr::UNSPECIFIED)
                .map_err(|e| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("join multicast {}: {e}", self.config.host),
                })?;
        } else if let Ok(mcast_addr) = self.config.host.parse::<std::net::Ipv6Addr>() {
            socket
                .join_multicast_v6(&mcast_addr, 0)
                .map_err(|e| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("join multicast v6 {}: {e}", self.config.host),
                })?;
        } else {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("invalid multicast address: {}", self.config.host),
            });
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
            IpProtocol::Tcp | IpProtocol::TcpNonBlocking => {
                let stream = self.connect_tcp()?;
                if self.config.no_delay {
                    stream.set_nodelay(true)?;
                }
                if self.config.protocol == IpProtocol::TcpNonBlocking {
                    stream.set_nonblocking(true)?;
                }
                self.io.inner = Some(IpIoInner::Tcp(stream));
            }
            IpProtocol::Udp => {
                let socket = self.connect_udp()?;
                self.io.inner = Some(IpIoInner::Udp(socket));
            }
            IpProtocol::UdpBroadcast => {
                let socket = self.connect_udp_broadcast()?;
                self.io.inner = Some(IpIoInner::Udp(socket));
            }
            IpProtocol::UdpMulticast => {
                let socket = self.connect_udp_multicast()?;
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
        }
        self.base.connected = true;
        self.base.announce_exception(AsynException::Connect, -1);
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
        self.base.connected = false;
        self.base.announce_exception(AsynException::Connect, -1);
        Ok(())
    }

    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
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
                Ok(r.nbytes_transferred)
            }
            Err(ref e) if self.disconnect_on_read_timeout => {
                if let AsynError::Status {
                    status: AsynStatus::Timeout,
                    ..
                } = e
                {
                    asyn_trace!(
                        Some(self.base.trace),
                        &self.base.port_name,
                        TraceMask::FLOW,
                        "disconnectOnReadTimeout triggered"
                    );
                    self.io.inner = None;
                    self.base.connected = false;
                    self.base.announce_exception(AsynException::Connect, -1);
                }
                result.map(|r| r.nbytes_transferred)
            }
            Err(_) => result.map(|r| r.nbytes_transferred),
        }
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
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
                // Parse new host:port, disconnect, update config
                let new_config = IpPortConfig::parse(value)?;
                if self.base.connected {
                    self.io.inner = None;
                    self.base.connected = false;
                    self.base.announce_exception(AsynException::Connect, -1);
                }
                self.config.host = new_config.host;
                self.config.port = new_config.port;
                if new_config.local_port.is_some() {
                    self.config.local_port = new_config.local_port;
                }
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

    // --- Phase 3A: protocol suffix parsing ---

    #[test]
    fn test_parse_tcp_nonblocking() {
        let cfg = IpPortConfig::parse("host:5025 TCP&").unwrap();
        assert_eq!(cfg.protocol, IpProtocol::TcpNonBlocking);
        assert_eq!(cfg.host, "host");
        assert_eq!(cfg.port, 5025);
    }

    #[test]
    fn test_parse_tcp_nonblocking_lowercase() {
        let cfg = IpPortConfig::parse("host:5025 tcp&").unwrap();
        assert_eq!(cfg.protocol, IpProtocol::TcpNonBlocking);
    }

    #[test]
    fn test_parse_udp_broadcast() {
        let cfg = IpPortConfig::parse("192.168.1.255:9000 UDP&").unwrap();
        assert_eq!(cfg.protocol, IpProtocol::UdpBroadcast);
        assert_eq!(cfg.host, "192.168.1.255");
    }

    #[test]
    fn test_parse_udp_multicast() {
        let cfg = IpPortConfig::parse("239.1.2.3:5000 UDP*").unwrap();
        assert_eq!(cfg.protocol, IpProtocol::UdpMulticast);
        assert_eq!(cfg.host, "239.1.2.3");
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
            IpProtocol::TcpNonBlocking
        );
        assert_eq!(
            IpPortConfig::parse("h:1 Udp&").unwrap().protocol,
            IpProtocol::UdpBroadcast
        );
        assert_eq!(
            IpPortConfig::parse("h:1 Udp*").unwrap().protocol,
            IpProtocol::UdpMulticast
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

    // --- UDP broadcast flag test ---

    #[test]
    fn test_udp_broadcast_flag() {
        let cfg = IpPortConfig::parse("255.255.255.255:9000 UDP&").unwrap();
        let drv = DrvAsynIPPort::new("bcast_test", "255.255.255.255:9000 UDP&").unwrap();
        assert_eq!(cfg.protocol, IpProtocol::UdpBroadcast);
        assert_eq!(drv.config.protocol, IpProtocol::UdpBroadcast);
    }
}
