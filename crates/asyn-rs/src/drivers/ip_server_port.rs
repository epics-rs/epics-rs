//! TCP server-mode IP port driver (drvAsynIPServerPort equivalent).
//!
//! Mirrors C asyn's `drvAsynIPServerPortConfigure` PR #148/#109. Where
//! [`super::ip_port::DrvAsynIPPort`] dials out to a remote endpoint as
//! a TCP/UDP client, this driver listens on a local port, accepts
//! incoming connections, and routes their traffic through the asyn
//! framework as `asynOctet` channels — useful for IOC-as-server
//! protocols (e.g. a motor controller that initiates the connection
//! to the IOC, reverse-protocol gateways, scripted test harnesses).
//!
//! # Configuration string
//!
//! `"host:port [TCP|UDP] [SO_REUSEPORT]"` — same syntax as
//! [`super::ip_port::IpPortConfig::parse`], with two extras:
//!
//! - `host` may be `"0.0.0.0"` (all IPv4) or a specific bind address.
//! - The trailing `SO_REUSEPORT` token (case-insensitive, optional)
//!   sets the Linux/BSD `SO_REUSEPORT` socket option so multiple
//!   independent listeners can share the bound port for kernel-level
//!   load balancing — matches asyn PR #109's `RUL` reuse flag.
//!
//! # Connection lifecycle
//!
//! Each accepted client maps to an `addr` slot starting at 0. When the
//! connection closes, the slot is freed and reusable. Reads/writes
//! address a slot via [`crate::user::AsynUser::addr`]. The `addr=-1`
//! sentinel (broadcast) writes to every connected client.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::exception::AsynException;
use crate::interpose::{EomReason, OctetReadResult};
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::user::AsynUser;

/// Maximum simultaneous accepted clients. Keeps the slot table
/// bounded; mirror C asyn `MAX_NUM_CLIENTS=4` default but raised to
/// 64 to match the multi-NIC / multi-instrument density we expect on
/// modern IOC hosts. Tunable per-port via [`IpServerConfig::max_clients`].
pub const DEFAULT_MAX_CLIENTS: usize = 64;

/// Server-mode transport protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IpServerProtocol {
    /// TCP listener — accepts connections, each becomes a slot.
    #[default]
    Tcp,
    /// UDP server — receives datagrams from any peer; each unique
    /// source address auto-occupies a slot. Writes back to a slot
    /// send to that peer's address. No connection lifecycle —
    /// idle peers are evicted on slot pressure.
    Udp,
}

/// Configuration parsed from a `drvAsynIPServerPortConfigure`-style spec.
#[derive(Debug, Clone)]
pub struct IpServerConfig {
    /// Bind address (`0.0.0.0` to accept on every interface, or a
    /// specific NIC IP / `127.0.0.1` for loopback-only).
    pub bind_host: String,
    /// Bind TCP / UDP port. `0` requests an OS-assigned ephemeral
    /// port — useful for tests; the actual port can be queried via
    /// [`DrvAsynIPServerPort::local_port`] post-bind.
    pub bind_port: u16,
    /// Transport protocol — TCP listener or UDP receiver.
    pub protocol: IpServerProtocol,
    /// Enable `SO_REUSEPORT` (asyn PR #109). On Linux/macOS this
    /// allows multiple processes / threads to bind the same port for
    /// kernel-level load balancing across listening sockets. No-op
    /// on platforms without the option.
    pub reuse_port: bool,
    /// Slot table cap — see [`DEFAULT_MAX_CLIENTS`].
    pub max_clients: usize,
    /// Per-accepted-connection read timeout. Affects the worker
    /// task's `set_read_timeout`; defaults to no timeout (block until
    /// data or EOF).
    pub read_timeout: Option<Duration>,
}

impl IpServerConfig {
    /// Parse a `drvAsynIPServerPortConfigure`-style spec.
    ///
    /// Syntax: `"host:port [TCP] [SO_REUSEPORT]"` (the `TCP` token is
    /// accepted-and-ignored — UDP server mode is not yet wired). The
    /// host may be IPv4 (`0.0.0.0`, `127.0.0.1`, or specific NIC IP);
    /// IPv6 bracket form `[::]:port` is also accepted.
    pub fn parse(spec: &str) -> AsynResult<Self> {
        let trimmed = spec.trim();
        let mut tokens: Vec<&str> = trimmed.split_whitespace().collect();
        if tokens.is_empty() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "empty IP server port spec".into(),
            });
        }

        let mut reuse_port = false;
        let mut protocol = IpServerProtocol::Tcp;
        // Strip option tokens from the tail.
        while tokens.len() > 1 {
            let last = tokens.last().unwrap().to_ascii_uppercase();
            match last.as_str() {
                "TCP" => {
                    protocol = IpServerProtocol::Tcp;
                    tokens.pop();
                }
                "UDP" => {
                    protocol = IpServerProtocol::Udp;
                    tokens.pop();
                }
                "SO_REUSEPORT" | "REUSEPORT" => {
                    reuse_port = true;
                    tokens.pop();
                }
                _ => break,
            }
        }
        if tokens.len() != 1 {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("unexpected tokens after host:port in '{spec}'"),
            });
        }
        let addr_part = tokens[0];

        // Reuse the host:port parser shape from ip_port — we accept
        // IPv6 bracket form too.
        let (host, port) = if let Some(rest) = addr_part.strip_prefix('[') {
            let end = rest.find(']').ok_or_else(|| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("missing closing bracket in IPv6 address: '{spec}'"),
            })?;
            let host = rest[..end].to_string();
            let port_part = rest[end + 1..]
                .strip_prefix(':')
                .ok_or_else(|| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("missing port after bracketed IPv6 address: '{spec}'"),
                })?;
            let port: u16 = port_part.parse().map_err(|_| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("invalid port in '{spec}'"),
            })?;
            (host, port)
        } else {
            let (host, port_part) =
                addr_part
                    .rsplit_once(':')
                    .ok_or_else(|| AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("missing port in '{spec}' (expected host:port)"),
                    })?;
            let port: u16 = port_part.parse().map_err(|_| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("invalid port in '{spec}'"),
            })?;
            (host.to_string(), port)
        };

        Ok(Self {
            bind_host: host,
            bind_port: port,
            protocol,
            reuse_port,
            max_clients: DEFAULT_MAX_CLIENTS,
            read_timeout: None,
        })
    }
}

/// Per-accepted-connection state.
struct ClientSlot {
    stream: Mutex<Option<TcpStream>>,
    peer: SocketAddr,
}

impl ClientSlot {
    fn new(stream: TcpStream, peer: SocketAddr) -> Self {
        Self {
            stream: Mutex::new(Some(stream)),
            peer,
        }
    }
}

/// Server-mode IP port driver.
pub struct DrvAsynIPServerPort {
    base: PortDriverBase,
    config: IpServerConfig,
    listener: Mutex<Option<TcpListener>>,
    /// Fixed-size client slot table. `slots[addr]` is `None` until a
    /// connection is assigned to that addr. Slot reuse on disconnect
    /// keeps `addr` stable for the lifetime of the connection.
    slots: Vec<Mutex<Option<Arc<ClientSlot>>>>,
}

impl DrvAsynIPServerPort {
    /// Create a new server-mode IP port driver. Does not bind yet —
    /// call [`Self::connect`] (or let the asyn framework's auto-connect
    /// drive it).
    pub fn new(port_name: &str, spec: &str) -> AsynResult<Self> {
        let config = IpServerConfig::parse(spec)?;
        Self::with_config(port_name, config)
    }

    /// Create from an explicit config (skips the spec parser, useful
    /// for callers building config programmatically — tests, etc.).
    pub fn with_config(port_name: &str, config: IpServerConfig) -> AsynResult<Self> {
        let max = config.max_clients.max(1);
        let mut base = PortDriverBase::new(
            port_name,
            max,
            PortFlags {
                multi_device: true,
                can_block: true,
                destructible: true,
            },
        );
        base.connected = false;
        base.auto_connect = true;
        let mut slots = Vec::with_capacity(max);
        for _ in 0..max {
            slots.push(Mutex::new(None));
        }
        Ok(Self {
            base,
            config,
            listener: Mutex::new(None),
            slots,
        })
    }

    /// Bind the listener socket and mark the port connected.
    fn open_listener(&mut self) -> AsynResult<()> {
        if self.config.protocol == IpServerProtocol::Udp {
            // UDP server mode parses fully via `IpServerConfig` but
            // the runtime accept-loop / per-peer slot routing is a
            // separate code path from the TCP listener (UDP has no
            // connection lifecycle — every datagram carries its
            // source address). The parser landed first so callers
            // can configure the spec; runtime wiring is a follow-up
            // when a use case lands.
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!(
                    "UDP server-mode runtime not yet wired (config parsed: \
                     bind={}:{} reuse_port={}); pending follow-up that adds \
                     the per-datagram peer-slot routing",
                    self.config.bind_host, self.config.bind_port, self.config.reuse_port,
                ),
            });
        }
        let bind_str = if self.config.bind_host.contains(':') {
            // IPv6
            format!("[{}]:{}", self.config.bind_host, self.config.bind_port)
        } else {
            format!("{}:{}", self.config.bind_host, self.config.bind_port)
        };

        // Use socket2 for SO_REUSEPORT control on Linux/BSD; fall
        // through to plain TcpListener otherwise.
        let listener = self.bind_with_options(&bind_str)?;
        listener
            .set_nonblocking(false)
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("set_nonblocking failed: {e}"),
            })?;
        *self.listener.lock().unwrap() = Some(listener);
        self.base.connected = true;
        self.base.announce_exception(AsynException::Connect, -1);
        Ok(())
    }

    fn bind_with_options(&self, bind_str: &str) -> AsynResult<TcpListener> {
        let addr: SocketAddr =
            bind_str
                .parse()
                .map_err(|e: std::net::AddrParseError| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("invalid bind address '{bind_str}': {e}"),
                })?;
        let domain = if addr.is_ipv4() {
            socket2::Domain::IPV4
        } else {
            socket2::Domain::IPV6
        };
        let socket =
            socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))
                .map_err(|e| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("socket() failed: {e}"),
                })?;
        socket
            .set_reuse_address(true)
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("SO_REUSEADDR failed: {e}"),
            })?;
        if self.config.reuse_port {
            #[cfg(unix)]
            {
                if let Err(e) = socket.set_reuse_port(true) {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("SO_REUSEPORT failed: {e}"),
                    });
                }
            }
            #[cfg(not(unix))]
            {
                // SO_REUSEPORT is Linux/BSD-only; ignore on Windows
                // matching asyn PR #109's portable fallback.
            }
        }
        socket.bind(&addr.into()).map_err(|e| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("bind '{bind_str}' failed: {e}"),
        })?;
        // Backlog independent of `max_clients` — the slot cap bounds
        // *concurrent* accepted clients, not the kernel's pending-
        // connection queue. A small backlog (= max_clients) caused
        // third-party connect() to block in tests when 2 prior
        // connections were already queued. 128 mirrors the typical
        // SOMAXCONN on Linux/macOS while staying portable.
        socket.listen(128).map_err(|e| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("listen failed: {e}"),
        })?;
        Ok(TcpListener::from(socket))
    }

    /// Return the actual bound port (useful when `bind_port = 0`).
    /// Returns `0` if not yet listening.
    pub fn local_port(&self) -> u16 {
        self.listener
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|l| l.local_addr().ok())
            .map(|a| a.port())
            .unwrap_or(0)
    }

    /// Accept one pending connection and assign it to a free slot.
    /// Returns the slot index used, or an error if no slot was free
    /// or the listener is not bound.
    pub fn accept_one(&self) -> AsynResult<usize> {
        let listener_guard = self.listener.lock().unwrap();
        let listener = listener_guard.as_ref().ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "listener not bound — connect() the port first".into(),
        })?;
        let (stream, peer) = listener.accept().map_err(|e| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("accept failed: {e}"),
        })?;
        if let Some(t) = self.config.read_timeout {
            stream
                .set_read_timeout(Some(t))
                .map_err(|e| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("set_read_timeout failed: {e}"),
                })?;
        }
        // First-fit slot scan. Linear over `max_clients` (default 64)
        // — plenty fast for the rates an asyn server sees.
        for (i, slot) in self.slots.iter().enumerate() {
            let mut g = slot.lock().unwrap();
            if g.is_none() {
                *g = Some(Arc::new(ClientSlot::new(stream, peer)));
                self.base
                    .announce_exception(AsynException::Connect, i as i32);
                return Ok(i);
            }
        }
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: format!(
                "no free client slot (max_clients={}); dropped connection from {peer}",
                self.config.max_clients
            ),
        })
    }

    /// Drop the connection in `addr`, freeing the slot.
    pub fn drop_client(&self, addr: i32) -> AsynResult<()> {
        let idx = self.slot_index(addr)?;
        let mut g = self.slots[idx].lock().unwrap();
        if g.is_some() {
            *g = None;
            self.base.announce_exception(AsynException::Connect, addr);
        }
        Ok(())
    }

    fn slot_index(&self, addr: i32) -> AsynResult<usize> {
        if addr < 0 || (addr as usize) >= self.slots.len() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("addr {addr} out of range (max {})", self.slots.len()),
            });
        }
        Ok(addr as usize)
    }

    fn slot_arc(&self, addr: i32) -> AsynResult<Arc<ClientSlot>> {
        let idx = self.slot_index(addr)?;
        self.slots[idx]
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("slot {addr} has no connected client"),
            })
    }

    /// Return the peer SocketAddr of the slot, if connected.
    pub fn peer(&self, addr: i32) -> Option<SocketAddr> {
        let idx = self.slot_index(addr).ok()?;
        self.slots[idx].lock().unwrap().as_ref().map(|c| c.peer)
    }
}

impl PortDriver for DrvAsynIPServerPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        if self.base.connected && self.listener.lock().unwrap().is_some() {
            return Ok(());
        }
        self.open_listener()
    }

    fn disconnect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        // Tear down all per-client slots first so the asynUser sees
        // every Disconnect exception before the port-level one.
        for (i, slot) in self.slots.iter().enumerate() {
            let mut g = slot.lock().unwrap();
            if g.take().is_some() {
                self.base
                    .announce_exception(AsynException::Connect, i as i32);
            }
        }
        *self.listener.lock().unwrap() = None;
        self.base.connected = false;
        self.base.announce_exception(AsynException::Connect, -1);
        Ok(())
    }

    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        let res = self.base_read_octet(user, buf)?;
        Ok(res.nbytes_transferred)
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
        if user.addr < 0 {
            // Broadcast: send to every connected slot. Errors per
            // slot are logged but never abort the broadcast — a dead
            // peer mustn't take out the rest.
            for (i, slot) in self.slots.iter().enumerate() {
                let arc = match slot.lock().unwrap().clone() {
                    Some(a) => a,
                    None => continue,
                };
                if let Err(e) = self.write_to_slot(&arc, data) {
                    tracing::debug!(
                        target: "asyn_rs::ip_server_port",
                        addr = i,
                        error = %e,
                        "broadcast write to slot failed"
                    );
                    // Drop the slot if the write looks fatal (peer
                    // closed). Match the connection-refused / broken-
                    // pipe pattern from drvAsynIPPort.
                    *slot.lock().unwrap() = None;
                    self.base
                        .announce_exception(AsynException::Connect, i as i32);
                }
            }
            return Ok(());
        }
        let arc = self.slot_arc(user.addr)?;
        match self.write_to_slot(&arc, data) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Mark slot disconnected so the next read/write fails fast.
                if let Ok(idx) = self.slot_index(user.addr) {
                    *self.slots[idx].lock().unwrap() = None;
                    self.base
                        .announce_exception(AsynException::Connect, user.addr);
                }
                Err(e)
            }
        }
    }
}

impl DrvAsynIPServerPort {
    fn base_read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
        let arc = self.slot_arc(user.addr)?;
        let mut stream_guard = arc.stream.lock().unwrap();
        let stream = stream_guard.as_mut().ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("slot {} stream gone", user.addr),
        })?;
        if user.timeout > Duration::from_nanos(0) {
            stream
                .set_read_timeout(Some(user.timeout))
                .map_err(|e| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("set_read_timeout failed: {e}"),
                })?;
        }
        match stream.read(buf) {
            Ok(0) => {
                // Peer closed — drop the slot, surface as Disconnect.
                drop(stream_guard);
                if let Ok(idx) = self.slot_index(user.addr) {
                    *self.slots[idx].lock().unwrap() = None;
                    self.base
                        .announce_exception(AsynException::Connect, user.addr);
                }
                Err(AsynError::Status {
                    status: AsynStatus::Disconnected,
                    message: format!("peer closed slot {}", user.addr),
                })
            }
            Ok(n) => Ok(OctetReadResult {
                nbytes_transferred: n,
                eom_reason: EomReason::empty(),
            }),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "read timeout".into(),
                })
            }
            Err(e) => Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("read failed: {e}"),
            }),
        }
    }

    fn write_to_slot(&self, slot: &ClientSlot, data: &[u8]) -> AsynResult<()> {
        let mut g = slot.stream.lock().unwrap();
        let stream = g.as_mut().ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "slot stream gone".into(),
        })?;
        stream.write_all(data).map_err(|e| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("write failed: {e}"),
        })?;
        stream.flush().map_err(|e| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("flush failed: {e}"),
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_ipv4() {
        let cfg = IpServerConfig::parse("0.0.0.0:8080").unwrap();
        assert_eq!(cfg.bind_host, "0.0.0.0");
        assert_eq!(cfg.bind_port, 8080);
        assert!(!cfg.reuse_port);
        assert_eq!(cfg.max_clients, DEFAULT_MAX_CLIENTS);
    }

    #[test]
    fn parse_with_tcp_token() {
        let cfg = IpServerConfig::parse("127.0.0.1:5000 TCP").unwrap();
        assert_eq!(cfg.bind_host, "127.0.0.1");
        assert_eq!(cfg.bind_port, 5000);
        assert!(!cfg.reuse_port);
    }

    #[test]
    fn parse_with_so_reuseport() {
        let cfg = IpServerConfig::parse("0.0.0.0:9000 TCP SO_REUSEPORT").unwrap();
        assert!(cfg.reuse_port);
        let cfg2 = IpServerConfig::parse("0.0.0.0:9000 reuseport").unwrap();
        assert!(cfg2.reuse_port);
    }

    #[test]
    fn parse_udp_protocol() {
        let cfg = IpServerConfig::parse("0.0.0.0:7000 UDP").unwrap();
        assert_eq!(cfg.protocol, IpServerProtocol::Udp);
        assert_eq!(cfg.bind_port, 7000);

        let cfg2 = IpServerConfig::parse("127.0.0.1:5000 UDP SO_REUSEPORT").unwrap();
        assert_eq!(cfg2.protocol, IpServerProtocol::Udp);
        assert!(cfg2.reuse_port);

        // Default protocol is TCP when no token supplied.
        let cfg3 = IpServerConfig::parse("0.0.0.0:1234").unwrap();
        assert_eq!(cfg3.protocol, IpServerProtocol::Tcp);
    }

    #[test]
    fn udp_server_runtime_returns_not_yet_wired() {
        let mut srv = DrvAsynIPServerPort::new("udp_srv", "127.0.0.1:0 UDP").unwrap();
        let user = AsynUser::default();
        let err = srv.connect(&user).unwrap_err();
        match err {
            AsynError::Status { message, .. } => {
                assert!(
                    message.contains("UDP server-mode runtime not yet wired"),
                    "expected UDP-not-wired error, got: {message}"
                );
            }
            _ => panic!("wrong error variant"),
        }
    }

    #[test]
    fn parse_ipv6_bracket_form() {
        let cfg = IpServerConfig::parse("[::1]:7000").unwrap();
        assert_eq!(cfg.bind_host, "::1");
        assert_eq!(cfg.bind_port, 7000);
    }

    #[test]
    fn parse_rejects_missing_port() {
        assert!(IpServerConfig::parse("0.0.0.0").is_err());
    }

    #[test]
    fn parse_rejects_unknown_token() {
        let err = IpServerConfig::parse("0.0.0.0:8080 BOGUS").unwrap_err();
        match err {
            AsynError::Status { message, .. } => {
                assert!(message.contains("unexpected"), "msg={message}");
            }
            _ => panic!("expected Status err"),
        }
    }

    /// End-to-end: bind on 127.0.0.1:0 (ephemeral), accept one
    /// client, exchange one round-trip request/response.
    #[test]
    fn server_accepts_and_round_trips() {
        let mut srv = DrvAsynIPServerPort::new("srv1", "127.0.0.1:0").unwrap();
        let user = AsynUser::default();
        srv.connect(&user).unwrap();
        let port = srv.local_port();
        assert!(port > 0);

        // Spawn a client thread that connects, sends, then receives.
        let client_handle = std::thread::spawn(move || {
            let mut s = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
            s.write_all(b"hello-server").unwrap();
            let mut buf = [0u8; 32];
            let n = s.read(&mut buf).unwrap();
            buf[..n].to_vec()
        });

        // Server side: accept, read, write reply.
        let addr = srv.accept_one().unwrap();
        assert_eq!(addr, 0, "first slot");
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 32];
        let n = srv.read_octet(&user, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello-server");
        srv.write_octet(&mut user, b"hello-client").unwrap();

        let reply = client_handle.join().unwrap();
        assert_eq!(reply, b"hello-client");
    }

    /// `accept_one` exhausts slots when more than `max_clients`
    /// connections are pending.
    #[test]
    fn slot_table_caps_concurrent_clients() {
        let cfg = IpServerConfig {
            bind_host: "127.0.0.1".into(),
            bind_port: 0,
            reuse_port: false,
            protocol: IpServerProtocol::Tcp,
            max_clients: 2,
            read_timeout: None,
        };
        let mut srv = DrvAsynIPServerPort::with_config("srv2", cfg).unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let port = srv.local_port();

        let _c1 = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        let _c2 = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        let _c3 = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();

        assert_eq!(srv.accept_one().unwrap(), 0);
        assert_eq!(srv.accept_one().unwrap(), 1);
        // Third accept should fail: no free slot.
        let err = srv.accept_one().unwrap_err();
        match err {
            AsynError::Status { message, .. } => {
                assert!(
                    message.contains("no free client slot"),
                    "expected slot-full error, got: {message}"
                );
            }
            _ => panic!("wrong error variant"),
        }
    }

    /// `drop_client` frees a slot for reuse.
    ///
    /// Note on accept-when-full semantics: when the slot table is
    /// saturated, `accept_one` still pulls the next pending
    /// connection from the kernel queue (POSIX `accept(2)` has no
    /// peek primitive) and returns Err — the just-accepted stream
    /// drops, so the peer sees the connection close. Operators must
    /// `drop_client` an existing slot BEFORE the next client connects
    /// if they want it to land. This test exercises that contract.
    #[test]
    fn drop_client_releases_slot() {
        let cfg = IpServerConfig {
            bind_host: "127.0.0.1".into(),
            bind_port: 0,
            reuse_port: false,
            protocol: IpServerProtocol::Tcp,
            max_clients: 1,
            read_timeout: None,
        };
        let mut srv = DrvAsynIPServerPort::with_config("srv3", cfg).unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let port = srv.local_port();

        let _c1 = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        assert_eq!(srv.accept_one().unwrap(), 0);
        assert!(srv.peer(0).is_some());

        // Free the slot first (operator action), then connect a new
        // client — the next accept lands it.
        srv.drop_client(0).unwrap();
        assert!(srv.peer(0).is_none());

        let _c2 = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        assert_eq!(srv.accept_one().unwrap(), 0);
        assert!(srv.peer(0).is_some());
    }

    /// SO_REUSEPORT must allow a second listener on the same port
    /// (Linux/BSD only — Windows path silently no-ops the option).
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
    #[test]
    fn so_reuseport_allows_second_bind() {
        let cfg1 = IpServerConfig {
            bind_host: "127.0.0.1".into(),
            bind_port: 0,
            reuse_port: true,
            protocol: IpServerProtocol::Tcp,
            max_clients: 1,
            read_timeout: None,
        };
        let mut srv1 = DrvAsynIPServerPort::with_config("ru1", cfg1).unwrap();
        srv1.connect(&AsynUser::default()).unwrap();
        let port = srv1.local_port();

        let cfg2 = IpServerConfig {
            bind_host: "127.0.0.1".into(),
            bind_port: port,
            reuse_port: true,
            protocol: IpServerProtocol::Tcp,
            max_clients: 1,
            read_timeout: None,
        };
        let mut srv2 = DrvAsynIPServerPort::with_config("ru2", cfg2).unwrap();
        srv2.connect(&AsynUser::default())
            .expect("second SO_REUSEPORT bind on the same port must succeed");
    }
}
