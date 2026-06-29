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
//! `"host:port [TCP|UDP]"` — matches C `drvAsynIPServerPort.c`
//! `sscanf(":%u %5s", &portNumber, protocol)` (lines 580-600). Only
//! `tcp` (default) / `udp` are accepted as the protocol token; there is
//! **no `SO_REUSEPORT` token in upstream C asyn** (earlier versions of
//! this module accepted one — removed for parity). `SO_REUSEADDR` is set
//! unconditionally on the listening socket (`drvAsynIPServerPort.c:430`).
//! In **UDP** mode the socket additionally enables `SO_REUSEPORT` for
//! datagram fanout — C calls `epicsSocketEnableAddressUseForDatagramFanout`
//! for `SOCK_DGRAM` (`drvAsynIPServerPort.c:426-429`) so multiple IOCs can
//! share the port; the TCP listener does not.
//!
//! - `host` may be empty / `"0.0.0.0"` (all IPv4), `"localhost"` (also
//!   all IPv4 — C does not map it to loopback), a bind IP, or a hostname.
//!
//! # Connection lifecycle
//!
//! Each accepted client maps to an `addr` slot starting at 0. When the
//! connection closes, the slot is freed and reusable. Reads/writes
//! address a slot via [`crate::user::AsynUser::addr`]. The `addr=-1`
//! sentinel (broadcast) writes to every connected client.
//!
//! # UDP server mode (C-asyn `drvAsynSerial/drvAsynIPServerPort.c`)
//!
//! With protocol `Udp`, the server binds a UDP socket and a worker
//! thread loops `recv` (the source address is intentionally
//! discarded — C asyn calls `recvfrom(fd, buf, size, 0, NULL, NULL)`
//! at line 311) into a single shared buffer. `read_octet` drains the
//! buffer non-blocking — when the buffer is empty it returns `0`
//! bytes immediately rather than blocking, mirroring the C "if
//! `(UDPbufferPos == 0) && (UDPbufferSize == 0)` then sleep 1ms,
//! return 0" pattern (line 190). `write_octet` always errors —
//! `writeIt` in C is a one-line `return asynError;` (line 251).
//! There is no per-peer slot table; UDP server is a port-wide
//! "what arrived last on the socket" cache.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

// parking_lot::Mutex — consistent with the rest of asyn-rs and
// poison-tolerant: a panic in a worker thread cannot poison the lock
// and take out the port (std::sync::Mutex would).
use parking_lot::Mutex;

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::exception::AsynException;
use crate::interpose::{EomReason, OctetReadResult};
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::user::AsynUser;

/// Maximum simultaneous accepted clients. Keeps the slot table
/// bounded.
///
/// C asyn `drvAsynIPServerPortConfigure` has **no implicit max-clients
/// default** — the caller must pass `maxClients` explicitly as the
/// third iocsh argument (`drvAsynIPServerPort.c:729` —
/// `iocshArg "max clients"`). Rust callers that construct via
/// [`IpServerConfig::parse`] inherit this constant; callers that
/// build `IpServerConfig` directly can override
/// [`IpServerConfig::max_clients`]. 64 picked to fit modern
/// multi-instrument hosts without forcing every caller to invent
/// a value.
pub const DEFAULT_MAX_CLIENTS: usize = 64;

/// IPv4 datagram payload cap from C asyn `THEORETICAL_UDP_MAX_SIZE`
/// (line 83 of drvAsynIPServerPort.c). 65507 = 65535 minus IPv4
/// header (20) and UDP header (8). Matches the largest datagram the
/// kernel will hand us in one `recvfrom`.
pub const UDP_MAX_DATAGRAM: usize = 65507;

/// Server-mode transport protocol — TCP (multi-client slot table)
/// or UDP (single shared cache, no per-peer state). Matches the
/// `socketType` field branch in C asyn drvAsynIPServerPort.c
/// (`SOCK_STREAM` vs `SOCK_DGRAM`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IpServerProtocol {
    #[default]
    Tcp,
    /// UDP receiver. C asyn calls `recvfrom(fd, buf, size, 0, NULL,
    /// NULL)` — **source address discarded** — into a single shared
    /// buffer. `read_octet` drains; `write_octet` is a no-op error.
    Udp,
}

/// Configuration parsed from a `drvAsynIPServerPortConfigure`-style spec.
#[derive(Debug, Clone)]
pub struct IpServerConfig {
    /// Bind address (`0.0.0.0` to accept on every interface, or a
    /// specific NIC IP / `127.0.0.1` for loopback-only).
    pub bind_host: String,
    /// Bind TCP/UDP port. `0` requests an OS-assigned ephemeral port —
    /// useful for tests; the actual port can be queried via
    /// [`DrvAsynIPServerPort::local_port`] post-bind.
    pub bind_port: u16,
    /// Transport protocol — TCP listener or UDP receiver.
    pub protocol: IpServerProtocol,
    /// Slot table cap — see [`DEFAULT_MAX_CLIENTS`]. Ignored in UDP
    /// mode (no per-peer slots).
    pub max_clients: usize,
    /// Per-accepted-connection read timeout. Affects the worker
    /// task's `set_read_timeout`; defaults to no timeout (block until
    /// data or EOF).
    pub read_timeout: Option<Duration>,
}

impl IpServerConfig {
    /// Parse a `drvAsynIPServerPortConfigure`-style spec.
    ///
    /// Syntax: `"host:port [tcp|udp]"`. Matches C `sscanf(":%u %5s",
    /// &portNumber, protocol)` in `drvAsynIPServerPort.c:582`; only
    /// `tcp` (default) and `udp` are accepted. Unknown trailing
    /// tokens are rejected. The host may be IPv4 (`0.0.0.0`,
    /// `127.0.0.1`, or specific NIC IP); IPv6 bracket form
    /// `[::]:port` is also accepted.
    pub fn parse(spec: &str) -> AsynResult<Self> {
        let trimmed = spec.trim();
        let mut tokens: Vec<&str> = trimmed.split_whitespace().collect();
        if tokens.is_empty() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "empty IP server port spec".into(),
            });
        }

        let mut protocol = IpServerProtocol::Tcp;
        // Strip the optional protocol token from the tail. C asyn
        // accepts only `tcp` / `udp` (case-insensitive); anything
        // else is rejected — see drvAsynIPServerPort.c:591-600.
        if tokens.len() == 2 {
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
                _ => {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!(
                            "unknown protocol token '{}' in '{spec}' (expected tcp or udp)",
                            tokens.last().unwrap()
                        ),
                    });
                }
            }
        }
        if tokens.len() != 1 {
            // Intentionally stricter than C: `sscanf(":%u %5s", ...)`
            // (drvAsynIPServerPort.c:582) reads the port and one protocol
            // token and silently ignores any trailing garbage. Rejecting
            // it surfaces config typos instead of swallowing them; no
            // valid C spec carries extra tokens.
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
            max_clients: DEFAULT_MAX_CLIENTS,
            read_timeout: None,
        })
    }
}

/// Per-accepted-connection state.
///
/// Each slot is pre-allocated and shared between the parent server
/// port and an optional [`DrvAsynIPSubport`] (registered as
/// `parent:N`) — C asyn `drvAsynIPServerPort.c:681-708` does the
/// same, creating `maxClients` child asyn ports up-front so external
/// device support can address a specific client by port name.
pub struct ClientSlot {
    stream: Mutex<Option<TcpStream>>,
    peer: Mutex<Option<SocketAddr>>,
}

impl ClientSlot {
    fn new_empty() -> Self {
        Self {
            stream: Mutex::new(None),
            peer: Mutex::new(None),
        }
    }

    fn is_occupied(&self) -> bool {
        self.stream.lock().is_some()
    }

    fn assign(&self, stream: TcpStream, peer: SocketAddr) {
        *self.stream.lock() = Some(stream);
        *self.peer.lock() = Some(peer);
    }

    fn clear(&self) {
        *self.stream.lock() = None;
        *self.peer.lock() = None;
    }

    fn peer_addr(&self) -> Option<SocketAddr> {
        *self.peer.lock()
    }

    /// Toss all pending input on this slot's TCP stream — the single
    /// owner of the server flush drain, shared by the parent
    /// ([`DrvAsynIPServerPort::io_flush`]) and the child
    /// ([`DrvAsynIPSubport::io_flush`]) data paths.
    ///
    /// Mirrors C `drvAsynIPPort::flushIt` (drvAsynIPPort.c:846-861):
    /// each accepted connection is served in C by a full `drvAsynIPPort`
    /// child port (drvAsynIPServerPort.c:690), whose flush sets the
    /// socket non-blocking and `recv`s until empty, discarding staged
    /// input so a flush-then-read returns only the new reply. No-op when
    /// the slot is unoccupied (C guards on `fd != INVALID_SOCKET`).
    fn drain_input(&self) {
        let mut g = self.stream.lock();
        let Some(stream) = g.as_mut() else { return };
        // C toggles non-blocking around the drain (setNonBlock 1 then 0);
        // restore blocking afterwards so the next timed read behaves.
        if stream.set_nonblocking(true).is_err() {
            return;
        }
        let mut buf = [0u8; 512];
        loop {
            match stream.read(&mut buf) {
                // EOF (peer closed) or any bytes tossed: C breaks on
                // `numRecv <= 0` and keeps looping while > 0. Stop on 0,
                // continue on >0.
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        let _ = stream.set_nonblocking(false);
    }
}

/// Server-mode IP port driver.
pub struct DrvAsynIPServerPort {
    base: PortDriverBase,
    config: IpServerConfig,
    listener: Mutex<Option<TcpListener>>,
    /// Fixed-size client slot table. Pre-allocated to `max_clients`
    /// slots; `slots[addr].is_occupied()` says whether a connection
    /// currently owns the slot. Slot identity is stable for the
    /// lifetime of the server port, so child subports (registered
    /// `parent:N`, see [`Self::make_subport`]) can hold an Arc to
    /// their slot for the long term — mirrors C
    /// `drvAsynIPServerPort.c:681-708` pre-creating child ports.
    /// Unused in UDP mode.
    slots: Vec<Arc<ClientSlot>>,

    // ----- UDP mode only -----
    /// Bound UDP socket. `Some` between `connect` and `disconnect` in
    /// UDP mode. The recv worker holds a clone so it can `recv` even
    /// while `disconnect` is replacing the field.
    udp_socket: Mutex<Option<Arc<UdpSocket>>>,
    /// Single shared cache of the most-recently-received datagram.
    /// Mirrors C asyn `tty->UDPbuffer`/`UDPbufferSize`/`UDPbufferPos`
    /// (lines 78-80 of drvAsynIPServerPort.c). The recv worker only
    /// re-fills when the cache is empty (matches line 190's
    /// "if Pos==0 && Size==0 then recvfrom").
    udp_cache: Arc<Mutex<UdpCache>>,
    /// Set to true on disconnect to stop the recv worker. The
    /// worker observes this between `recv` calls (woken by socket
    /// `set_read_timeout(200ms)`).
    udp_shutdown: Arc<AtomicBool>,
    /// Recv worker thread join handle. Joined on disconnect.
    udp_thread: Mutex<Option<JoinHandle<()>>>,
}

/// UDP cache state. `pos < len` means data is available to read;
/// `len == 0` means the recv worker can fetch a fresh datagram.
struct UdpCache {
    /// Bytes from the most-recent datagram (capped at
    /// [`UDP_MAX_DATAGRAM`]).
    data: Vec<u8>,
    /// Read position within `data`. Drained by `read_octet`.
    pos: usize,
}

impl UdpCache {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            pos: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    fn clear(&mut self) {
        self.data.clear();
        self.pos = 0;
    }
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
        // C `drvAsynIPServerPortConfigure` rejects `maxClients == 0` with
        // "No clients." and returns -1 (drvAsynIPServerPort.c:545-548) —
        // unconditionally, before the protocol is even parsed — because a
        // server with zero client slots is useless. Mirror that instead
        // of silently coercing 0 to 1, which hid the caller's mistake.
        if config.max_clients == 0 {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "maxClients must be > 0 (C drvAsynIPServerPort: \"No clients.\")".into(),
            });
        }
        let max = config.max_clients;
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
            slots.push(Arc::new(ClientSlot::new_empty()));
        }
        Ok(Self {
            base,
            config,
            listener: Mutex::new(None),
            slots,
            udp_socket: Mutex::new(None),
            udp_cache: Arc::new(Mutex::new(UdpCache::new())),
            udp_shutdown: Arc::new(AtomicBool::new(false)),
            udp_thread: Mutex::new(None),
        })
    }

    /// Bind the listener socket and mark the port connected.
    fn open_listener(&mut self) -> AsynResult<()> {
        if self.config.protocol == IpServerProtocol::Udp {
            return self.open_udp_listener();
        }
        let addr = self.resolve_bind_addr()?;

        // socket2 path so SO_REUSEADDR is set explicitly — mirrors
        // C asyn's unconditional setsockopt at drvAsynIPServerPort.c:430.
        let listener = self.bind_with_options(addr)?;
        listener
            .set_nonblocking(false)
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("set_nonblocking failed: {e}"),
            })?;
        *self.listener.lock() = Some(listener);
        self.base.set_connected(true);
        Ok(())
    }

    /// UDP-mode bind: open the datagram socket, spawn the recv
    /// worker. Mirrors C asyn's `connectIt` SOCK_DGRAM branch
    /// (drvAsynIPServerPort.c lines ~440-470).
    fn open_udp_listener(&mut self) -> AsynResult<()> {
        let addr = self.resolve_bind_addr()?;
        let domain = if addr.is_ipv4() {
            socket2::Domain::IPV4
        } else {
            socket2::Domain::IPV6
        };
        let sock = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("UDP socket() failed: {e}"),
            })?;
        // C enables datagram fanout on the UDP server socket
        // (drvAsynIPServerPort.c:426-429): for SOCK_DGRAM it calls
        // `epicsSocketEnableAddressUseForDatagramFanout`, which sets
        // SO_REUSEPORT (where available) followed by SO_REUSEADDR — so
        // multiple IOCs can bind the same UDP port and the kernel fans
        // each datagram out to them. The TCP listener gets only
        // SO_REUSEADDR (:430); the fanout helper is SOCK_DGRAM-only.
        #[cfg(unix)]
        sock.set_reuse_port(true).map_err(|e| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("UDP SO_REUSEPORT failed: {e}"),
        })?;
        sock.set_reuse_address(true)
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("UDP SO_REUSEADDR failed: {e}"),
            })?;
        sock.bind(&addr.into()).map_err(|e| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("UDP bind '{addr}' failed: {e}"),
        })?;
        let socket = UdpSocket::from(sock);
        // Read timeout caps shutdown latency — recv wakes every
        // 200ms so the worker can observe `udp_shutdown` flag.
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("UDP set_read_timeout failed: {e}"),
            })?;
        let socket = Arc::new(socket);

        self.udp_shutdown.store(false, Ordering::SeqCst);
        let socket_t = Arc::clone(&socket);
        let cache_t = Arc::clone(&self.udp_cache);
        let shutdown_t = Arc::clone(&self.udp_shutdown);
        let port_name = self.base.port_name.clone();
        let handle = std::thread::Builder::new()
            .name(format!("udp-server-{port_name}"))
            .spawn(move || udp_recv_loop(socket_t, cache_t, shutdown_t, port_name))
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("UDP recv thread spawn failed: {e}"),
            })?;

        *self.udp_socket.lock() = Some(socket);
        *self.udp_thread.lock() = Some(handle);
        self.base.set_connected(true);
        Ok(())
    }

    /// Resolve the configured bind `host:port` to a concrete socket
    /// address — the single owner of bind-address resolution shared by
    /// the TCP ([`Self::bind_with_options`]) and UDP
    /// ([`Self::open_udp_listener`]) paths.
    ///
    /// Mirrors C `createServerSocket` (drvAsynIPServerPort.c:403-419):
    /// the server address defaults to `INADDR_ANY` (`0.0.0.0`, :404) and
    /// is only overridden when the host is non-empty **and** not
    /// `"localhost"` (:412-413), in which case it is resolved by name
    /// (`hostToIPAddr`, :414). C deliberately maps both an empty host and
    /// `"localhost"` to `INADDR_ANY` — its comment tells callers to use
    /// `"127.0.0.1"` when they actually want the loopback interface — so
    /// this is faithful parity, not a copied bug.
    ///
    /// An IP literal (IPv4, or bracketless IPv6 such as `::1`) binds
    /// verbatim, preserving the existing explicit-address paths without a
    /// name lookup; any other host name is resolved like the client
    /// driver (`ip_port.rs::connect_udp`). The earlier
    /// `SocketAddr::parse` of `host:port` rejected empty-host,
    /// `localhost`, and every hostname.
    fn resolve_bind_addr(&self) -> AsynResult<SocketAddr> {
        let host = self.config.bind_host.trim();
        let port = self.config.bind_port;
        // Empty or "localhost" => INADDR_ANY, exactly as C (:404,
        // :412-413). C is IPv4-only here (PF_INET / sockaddr_in), so the
        // any-address is the IPv4 0.0.0.0.
        if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
            return Ok(SocketAddr::from(([0, 0, 0, 0], port)));
        }
        // IP literal => bind verbatim (no lookup); covers explicit IPv4
        // and bracketless IPv6.
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(SocketAddr::new(ip, port));
        }
        // Otherwise resolve the name (C hostToIPAddr, :414).
        use std::net::ToSocketAddrs;
        (host, port)
            .to_socket_addrs()
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("cannot resolve bind host '{host}': {e}"),
            })?
            .next()
            .ok_or_else(|| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("cannot resolve bind host '{host}': no addresses"),
            })
    }

    fn bind_with_options(&self, addr: SocketAddr) -> AsynResult<TcpListener> {
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
        // Unconditional SO_REUSEADDR (drvAsynIPServerPort.c:430).
        socket
            .set_reuse_address(true)
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("SO_REUSEADDR failed: {e}"),
            })?;
        socket.bind(&addr.into()).map_err(|e| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("bind '{addr}' failed: {e}"),
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
            .as_ref()
            .and_then(|l| l.local_addr().ok())
            .map(|a| a.port())
            .unwrap_or(0)
    }

    /// Accept one pending connection and assign it to a free slot.
    /// Returns the slot index used, or an error if no slot was free
    /// or the listener is not bound.
    pub fn accept_one(&self) -> AsynResult<usize> {
        let listener_guard = self.listener.lock();
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
        // First-fit slot scan. Linear over `max_clients` — plenty
        // fast for the rates an asyn server sees. Mirrors C asyn's
        // "search for a port which is disconnected" loop at
        // drvAsynIPServerPort.c:342-350.
        for (i, slot) in self.slots.iter().enumerate() {
            if !slot.is_occupied() {
                slot.assign(stream, peer);
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
        let slot = &self.slots[idx];
        if slot.is_occupied() {
            slot.clear();
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
        let slot = self.slots[idx].clone();
        if slot.is_occupied() {
            Ok(slot)
        } else {
            Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("slot {addr} has no connected client"),
            })
        }
    }

    /// Return the peer SocketAddr of the slot, if connected.
    pub fn peer(&self, addr: i32) -> Option<SocketAddr> {
        let idx = self.slot_index(addr).ok()?;
        self.slots[idx].peer_addr()
    }

    /// Canonical name of the child asyn subport for slot `idx`.
    /// Matches C asyn `drvAsynIPServerPort.c:684-688`'s
    /// `epicsSnprintf(pl->portName, len, "%s:%d", tty->portName, i)`.
    pub fn child_port_name(&self, idx: usize) -> String {
        format!("{}:{}", self.base.port_name, idx)
    }

    /// Names of every child subport this server can spawn. Useful
    /// when an IOC startup script wants to bind device support to
    /// specific slot names before clients connect.
    pub fn child_port_names(&self) -> Vec<String> {
        (0..self.slots.len())
            .map(|i| self.child_port_name(i))
            .collect()
    }

    /// Build a child subport that shares slot `idx` with this parent.
    /// The returned [`DrvAsynIPSubport`] can be registered with the
    /// asyn manager so device support addresses this specific slot
    /// by its port name (`<parent>:<idx>`) — same model as C
    /// asyn's `drvAsynIPPortConfigure` child-port creation at
    /// `drvAsynIPServerPort.c:690-707`.
    ///
    /// Returns `Err` if `idx` is out of range. The subport starts
    /// disconnected; calling `connect()` on it merely re-syncs the
    /// `base.connected` flag with the slot's current occupancy —
    /// real connect/disconnect transitions are driven by the
    /// parent's accept loop.
    pub fn make_subport(&self, idx: usize) -> AsynResult<DrvAsynIPSubport> {
        if idx >= self.slots.len() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!(
                    "subport idx {idx} out of range (max_clients={})",
                    self.slots.len()
                ),
            });
        }
        let name = self.child_port_name(idx);
        Ok(DrvAsynIPSubport::new(name, Arc::clone(&self.slots[idx])))
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
        let already_up = self.base.connected
            && (self.listener.lock().is_some() || self.udp_socket.lock().is_some());
        if already_up {
            return Ok(());
        }
        self.open_listener()
    }

    fn disconnect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        // Tear down all per-client slots first so the asynUser sees
        // every Disconnect exception before the port-level one.
        for (i, slot) in self.slots.iter().enumerate() {
            if slot.is_occupied() {
                slot.clear();
                self.base
                    .announce_exception(AsynException::Connect, i as i32);
            }
        }
        self.stop_udp_worker();
        *self.udp_socket.lock() = None;
        self.udp_cache.lock().clear();
        *self.listener.lock() = None;
        self.base.set_connected(false);
        Ok(())
    }

    fn shutdown(&mut self) -> AsynResult<()> {
        // BUG 3 fix: the UDP recv worker is spawned by
        // `open_udp_listener` and was joined only inside `disconnect()`.
        // On a normal actor teardown — the request channel closes
        // without an explicit `Disconnect` op — the actor calls
        // `driver.shutdown()` (port_actor.rs run / run_with_shutdown)
        // but NOT `disconnect()`. Without this override the recv thread
        // loops forever holding the bound UDP socket. Join it here so
        // the thread (and the socket it owns) is released on every
        // teardown path, matching the teardown `disconnect()` already
        // performs.
        self.stop_udp_worker();
        *self.udp_socket.lock() = None;
        self.udp_cache.lock().clear();
        *self.listener.lock() = None;
        Ok(())
    }

    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        if self.config.protocol == IpServerProtocol::Udp {
            return Ok(self.udp_drain_into(buf));
        }
        let res = self.base_read_octet(user, buf)?;
        Ok(res.nbytes_transferred)
    }

    fn io_read_octet_eom(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
    ) -> AsynResult<(usize, EomReason)> {
        if self.config.protocol == IpServerProtocol::Udp {
            // A UDP datagram is a message boundary: C `readIt`
            // (drvAsynIPServerPort.c:201-207) sets ASYN_EOM_END when the
            // datagram is fully drained, ASYN_EOM_CNT when the caller
            // buffer is too small and more of the datagram remains. The
            // default synthesis reports CNT-only and never END, so the
            // EOS interpose / `asynRecord::EOMR` never see the boundary.
            return Ok(self.udp_drain_into_eom(buf));
        }
        // TCP: surface the real end-of-message reason from the slot read
        // (CNT when the caller buffer filled, empty on a short read).
        let res = self.base_read_octet(user, buf)?;
        Ok((res.nbytes_transferred, res.eom_reason))
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        if self.config.protocol == IpServerProtocol::Udp {
            // C asyn `writeIt` for UDP server is a one-line
            // `return asynError;` — the server is read-only.
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "UDP server-mode port is read-only (C asyn writeIt returns asynError)"
                    .into(),
            });
        }
        if user.addr < 0 {
            // Broadcast: send to every connected slot. Errors per
            // slot are logged but never abort the broadcast — a dead
            // peer mustn't take out the rest.
            for (i, slot) in self.slots.iter().enumerate() {
                if !slot.is_occupied() {
                    continue;
                }
                if let Err(e) = self.write_to_slot(slot, data) {
                    tracing::debug!(
                        target: "asyn_rs::ip_server_port",
                        addr = i,
                        error = %e,
                        "broadcast write to slot failed"
                    );
                    // Drop the slot if the write looks fatal (peer
                    // closed). Match the connection-refused / broken-
                    // pipe pattern from drvAsynIPPort.
                    slot.clear();
                    self.base
                        .announce_exception(AsynException::Connect, i as i32);
                }
            }
            return Ok(data.len());
        }
        let arc = self.slot_arc(user.addr)?;
        match self.write_to_slot(&arc, data) {
            Ok(()) => Ok(data.len()),
            Err(e) => {
                // Mark slot disconnected so the next read/write fails fast.
                if let Ok(idx) = self.slot_index(user.addr) {
                    self.slots[idx].clear();
                    self.base
                        .announce_exception(AsynException::Connect, user.addr);
                }
                Err(e)
            }
        }
    }

    fn io_flush(&mut self, user: &mut AsynUser) -> AsynResult<()> {
        if self.config.protocol == IpServerProtocol::Udp {
            // C registers the UDP server's own `flushIt`
            // (drvAsynIPServerPort.c:655); it discards the cached
            // datagram by resetting `UDPbufferPos`/`UDPbufferSize`
            // (flushIt:244-245) so a flush-then-read waits for a fresh
            // datagram instead of re-returning the stale one. Clearing
            // the cache also lets the recv worker (which only refills
            // when the cache is empty) fetch the next datagram.
            self.udp_cache.lock().clear();
            return Ok(());
        }
        // TCP: each accepted connection is served in C by a full
        // `drvAsynIPPort` child (drvAsynIPServerPort.c:690) whose
        // `flushIt` drains the socket (drvAsynIPPort.c:846-861). The Rust
        // child `DrvAsynIPSubport` drains the same slot on its own flush;
        // the parent also serves clients by `addr`, so drain the
        // addressed slot here too. `addr < 0` (broadcast) drains every
        // connected slot, symmetric with the broadcast write path.
        if user.addr < 0 {
            for slot in &self.slots {
                slot.drain_input();
            }
        } else if let Ok(idx) = self.slot_index(user.addr) {
            self.slots[idx].drain_input();
        }
        Ok(())
    }
}

impl DrvAsynIPServerPort {
    fn base_read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
        let arc = self.slot_arc(user.addr)?;
        let mut stream_guard = arc.stream.lock();
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
                    self.slots[idx].clear();
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
                // C parity: CNT only when the requested count was
                // reached; a short read leaves the reason empty so the
                // EOS interpose keeps reading.
                eom_reason: if n >= buf.len() {
                    EomReason::CNT
                } else {
                    EomReason::empty()
                },
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
        let mut g = slot.stream.lock();
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

    /// UDP-mode read: copy at most `buf.len()` bytes from the cache,
    /// advance pos. When the cache fully drains, clear it so the
    /// recv worker can fetch the next datagram. Returns 0 (NOT an
    /// error) when the cache is empty — caller polls. Mirrors C
    /// asyn `readIt` (drvAsynIPServerPort.c lines 167-238) in
    /// behaviour, simplified to drop the off-by-one C bug
    /// (`maxchars - 1` copy with `+= maxchars` advance).
    fn udp_drain_into(&self, buf: &mut [u8]) -> usize {
        self.udp_drain_into_eom(buf).0
    }

    /// Drain the UDP cache and report the end-of-message reason. Single
    /// owner of the datagram-boundary decision shared by [`read_octet`]
    /// (count only) and [`io_read_octet_eom`] (count + EOM). Mirrors C
    /// `readIt` (drvAsynIPServerPort.c:201-207,232-235) with two
    /// independent conditions: `ASYN_EOM_END` when the datagram is fully
    /// drained, and `ASYN_EOM_CNT` when the caller buffer is filled. A
    /// datagram that exactly fills the buffer meets both. (C's `:235`
    /// CNT branch is dead only because its `:196-200` off-by-one short-
    /// reads by one byte; with the off-by-one removed the buffer-filled
    /// CNT condition is live, so it is honoured here.) An empty cache
    /// yields `(0, empty)` — the caller polls (no boundary to report).
    ///
    /// [`read_octet`]: PortDriver::read_octet
    /// [`io_read_octet_eom`]: PortDriver::io_read_octet_eom
    fn udp_drain_into_eom(&self, buf: &mut [u8]) -> (usize, EomReason) {
        let mut cache = self.udp_cache.lock();
        if cache.is_empty() {
            return (0, EomReason::empty());
        }
        let avail = cache.data.len() - cache.pos;
        let n = avail.min(buf.len());
        buf[..n].copy_from_slice(&cache.data[cache.pos..cache.pos + n]);
        cache.pos += n;
        let mut eom = EomReason::empty();
        if cache.is_empty() {
            // Datagram fully consumed — end of message (C `:201-204`).
            cache.clear();
            eom |= EomReason::END;
        }
        if n == buf.len() && !buf.is_empty() {
            // Caller buffer filled (C `:232-235`, de-deadened). When the
            // cache still holds more this is the sole reason; on an exact
            // fit it accompanies END.
            eom |= EomReason::CNT;
        }
        (n, eom)
    }

    /// Signal the UDP recv worker to stop and join it. Single owner
    /// for the worker-thread teardown transition — `disconnect()` and
    /// `shutdown()` both route through here so no teardown path can
    /// leave the thread (and the socket it holds) alive.
    ///
    /// The worker observes `udp_shutdown` between `recv` calls; the
    /// socket's 200ms read timeout caps the join latency. Idempotent:
    /// a no-op when no worker is running (TCP mode, or already torn
    /// down).
    fn stop_udp_worker(&mut self) {
        self.udp_shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.udp_thread.lock().take() {
            let _ = handle.join();
        }
    }

    /// Total bytes currently in the UDP cache (for tests/diagnostics).
    pub fn udp_cache_pending(&self) -> usize {
        let c = self.udp_cache.lock();
        c.data.len().saturating_sub(c.pos)
    }
}

/// UDP recv worker thread. Loops `socket.recv` (source address
/// discarded — C parity, line 311 `recvfrom(fd, buf, size, 0,
/// NULL, NULL)`). Only fetches a fresh datagram when the cache is
/// empty (C parity, line 190 `if Pos==0 && Size==0 then recvfrom
/// else sleep`). Exits when `shutdown` flips to true; the socket's
/// 200ms read timeout caps shutdown latency.
fn udp_recv_loop(
    socket: Arc<UdpSocket>,
    cache: Arc<Mutex<UdpCache>>,
    shutdown: Arc<AtomicBool>,
    port_name: String,
) {
    let mut buf = vec![0u8; UDP_MAX_DATAGRAM];
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        // Only `recv` if the cache is fully drained — match C asyn's
        // single-buffer protocol where new data is only fetched once
        // the consumer (read_octet) has finished with the previous
        // datagram.
        let cache_empty = cache.lock().is_empty();
        if !cache_empty {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        match socket.recv(&mut buf) {
            Ok(n) => {
                let mut c = cache.lock();
                c.data.clear();
                c.data.extend_from_slice(&buf[..n]);
                c.pos = 0;
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // 200ms read-timeout wake — loop and re-check shutdown.
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    target: "asyn_rs::ip_server_port",
                    port = %port_name,
                    error = %e,
                    "UDP recv error — exiting recv loop"
                );
                break;
            }
        }
    }
}

// --- Child subport (parent:N) ---

/// Child IP subport — represents a single accepted-connection slot
/// of a [`DrvAsynIPServerPort`] as its own asyn port.
///
/// Mirrors C asyn's `drvAsynIPServerPortConfigure` child port model
/// (`drvAsynIPServerPort.c:681-708`): the parent server port
/// pre-creates `maxClients` child asyn ports named `parent:0`,
/// `parent:1`, … so external device support can target a specific
/// client by port name. Each child shares a [`ClientSlot`] handle
/// with the parent — when the parent's accept loop assigns a TCP
/// stream to that slot the child becomes connected; when the slot
/// clears (peer closed or `drop_client`) the child reports
/// disconnected on the next access.
///
/// The child has no listener of its own — `connect()` is a passive
/// state sync (refreshes `base.connected` from the slot's occupancy).
/// Real connect / disconnect transitions are driven by the parent.
pub struct DrvAsynIPSubport {
    base: PortDriverBase,
    slot: Arc<ClientSlot>,
}

impl DrvAsynIPSubport {
    fn new(port_name: String, slot: Arc<ClientSlot>) -> Self {
        let mut base = PortDriverBase::new(
            &port_name,
            1,
            PortFlags {
                multi_device: false,
                can_block: true,
                destructible: true,
            },
        );
        base.connected = slot.is_occupied();
        base.auto_connect = false; // C uses noAutoConnect=1 for the child ports
        Self { base, slot }
    }

    /// Peer address currently bound to this subport's slot, if any.
    pub fn peer(&self) -> Option<SocketAddr> {
        self.slot.peer_addr()
    }
}

impl PortDriver for DrvAsynIPSubport {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        // Passive sync — child port's connection is driven by the
        // parent's accept loop, not by an outbound dial.
        self.base.set_connected(self.slot.is_occupied());
        if !self.base.connected {
            return Err(AsynError::Status {
                status: AsynStatus::Disconnected,
                message: "no client assigned to this subport slot yet".into(),
            });
        }
        Ok(())
    }

    fn disconnect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        // Subport disconnect drops the slot — same effect as the
        // parent's drop_client(idx). Slot clear is an explicit
        // ownership boundary (the slot owns the announce for
        // per-addr); the port-level Connect transition is owner-API.
        if self.slot.is_occupied() {
            self.slot.clear();
            self.base.announce_exception(AsynException::Connect, 0);
        }
        self.base.set_connected(false);
        Ok(())
    }

    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        let mut stream_guard = self.slot.stream.lock();
        let stream = stream_guard.as_mut().ok_or_else(|| AsynError::Status {
            status: AsynStatus::Disconnected,
            message: "subport slot has no client".into(),
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
                drop(stream_guard);
                self.slot.clear();
                // Per-addr Connect carries addr=0 (this is the
                // subport's single device slot). The port-level
                // set_connected(false) handles the port-level
                // transition exactly once thanks to its edge
                // guard. Both fan-outs are necessary because
                // observers can listen at either the port or
                // the device granularity.
                self.base.announce_exception(AsynException::Connect, 0);
                self.base.set_connected(false);
                Err(AsynError::Status {
                    status: AsynStatus::Disconnected,
                    message: "peer closed".into(),
                })
            }
            Ok(n) => Ok(n),
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

    fn write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let mut g = self.slot.stream.lock();
        let stream = g.as_mut().ok_or_else(|| AsynError::Status {
            status: AsynStatus::Disconnected,
            message: "subport slot has no client".into(),
        })?;
        stream.write_all(data).map_err(|e| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("write failed: {e}"),
        })?;
        stream.flush().map_err(|e| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("flush failed: {e}"),
        })?;
        Ok(data.len())
    }

    fn io_flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
        // C parity: this child port is a full `drvAsynIPPort`
        // (drvAsynIPServerPort.c:690) whose `flushIt` tosses all pending
        // socket input (drvAsynIPPort.c:846-861) so a flush-then-read
        // returns only the new reply. Drain this slot's stream through
        // the shared owner.
        self.slot.drain_input();
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
        assert_eq!(cfg.max_clients, DEFAULT_MAX_CLIENTS);
    }

    #[test]
    fn parse_with_tcp_token() {
        let cfg = IpServerConfig::parse("127.0.0.1:5000 TCP").unwrap();
        assert_eq!(cfg.bind_host, "127.0.0.1");
        assert_eq!(cfg.bind_port, 5000);
    }

    #[test]
    fn parse_rejects_so_reuseport_token() {
        // C asyn drvAsynIPServerPort.c:597 prints "Unknown protocol"
        // and returns -1 for anything other than tcp/udp. Reject
        // SO_REUSEPORT — it is not a valid token in C asyn.
        assert!(IpServerConfig::parse("0.0.0.0:9000 SO_REUSEPORT").is_err());
        assert!(IpServerConfig::parse("0.0.0.0:9000 TCP SO_REUSEPORT").is_err());
    }

    #[test]
    fn parse_ipv6_bracket_form() {
        let cfg = IpServerConfig::parse("[::1]:7000").unwrap();
        assert_eq!(cfg.bind_host, "::1");
        assert_eq!(cfg.bind_port, 7000);
    }

    #[test]
    fn parse_udp_protocol_token() {
        let cfg = IpServerConfig::parse("0.0.0.0:7000 UDP").unwrap();
        assert_eq!(cfg.protocol, IpServerProtocol::Udp);
        let cfg2 = IpServerConfig::parse("0.0.0.0:7000").unwrap();
        assert_eq!(cfg2.protocol, IpServerProtocol::Tcp, "default is TCP");
    }

    /// DRV-22: `maxClients == 0` is rejected, matching C
    /// `drvAsynIPServerPortConfigure` "No clients." (drvAsynIPServerPort.c:545-548),
    /// not silently coerced to 1. Applies in both TCP and UDP mode (C
    /// checks before parsing the protocol).
    #[test]
    fn with_config_rejects_zero_max_clients() {
        let cfg = IpServerConfig {
            bind_host: "127.0.0.1".into(),
            bind_port: 0,
            protocol: IpServerProtocol::Tcp,
            max_clients: 0,
            read_timeout: None,
        };
        match DrvAsynIPServerPort::with_config("zero_clients", cfg) {
            Err(AsynError::Status { message, .. }) => {
                assert!(
                    message.contains("maxClients"),
                    "expected maxClients rejection, got: {message}"
                );
            }
            Ok(_) => panic!("expected maxClients==0 to be rejected"),
            Err(other) => panic!("wrong error variant: {other:?}"),
        }

        // A positive count still builds.
        let ok = IpServerConfig {
            bind_host: "127.0.0.1".into(),
            bind_port: 0,
            protocol: IpServerProtocol::Tcp,
            max_clients: 1,
            read_timeout: None,
        };
        assert!(DrvAsynIPServerPort::with_config("one_client", ok).is_ok());
    }

    /// DRV-19: bind-host resolution. C `createServerSocket`
    /// (drvAsynIPServerPort.c:403-419) defaults to `INADDR_ANY` and only
    /// overrides it for a non-empty host that is not `"localhost"`. An
    /// empty host and `"localhost"` (any case) both bind `0.0.0.0`; an IP
    /// literal binds verbatim. The pre-fix bare `SocketAddr::parse` of
    /// `host:port` rejected all three of empty/localhost/hostname.
    #[test]
    fn resolve_bind_addr_maps_localhost_and_empty_to_inaddr_any() {
        let any = IpAddr::from([0, 0, 0, 0]);

        let empty = DrvAsynIPServerPort::new("rb_empty", ":0").unwrap();
        assert_eq!(
            empty.resolve_bind_addr().unwrap().ip(),
            any,
            "empty host => INADDR_ANY"
        );

        let lh = DrvAsynIPServerPort::new("rb_localhost", "localhost:0").unwrap();
        assert_eq!(
            lh.resolve_bind_addr().unwrap().ip(),
            any,
            "localhost => INADDR_ANY (C does NOT map it to loopback)"
        );

        let lh_upper = DrvAsynIPServerPort::new("rb_localhost_upper", "LocalHost:0").unwrap();
        assert_eq!(
            lh_upper.resolve_bind_addr().unwrap().ip(),
            any,
            "localhost match is case-insensitive (C epicsStrCaseCmp)"
        );

        let explicit = DrvAsynIPServerPort::new("rb_explicit", "127.0.0.1:0").unwrap();
        assert_eq!(
            explicit.resolve_bind_addr().unwrap().ip(),
            IpAddr::from([127, 0, 0, 1]),
            "explicit IP literal binds verbatim"
        );
    }

    /// DRV-19 end-to-end: a `localhost`-named server now binds and
    /// listens. Pre-fix the bare `SocketAddr::parse("localhost:0")`
    /// errored, so a named-host server could never `connect()`.
    #[test]
    fn connect_localhost_named_host_binds() {
        let mut srv = DrvAsynIPServerPort::new("rb_connect_lh", "localhost:0").unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        assert!(srv.local_port() > 0, "listener bound to an ephemeral port");
        srv.disconnect(&AsynUser::default()).unwrap();
    }

    /// DRV-18: the UDP server socket enables SO_REUSEPORT (datagram
    /// fanout) while the TCP listener does not. C calls
    /// `epicsSocketEnableAddressUseForDatagramFanout` (SO_REUSEPORT +
    /// SO_REUSEADDR) for SOCK_DGRAM only (drvAsynIPServerPort.c:426-429);
    /// the TCP path gets SO_REUSEADDR alone (:430). Read the option back
    /// off the bound socket so the assertion does not depend on the
    /// platform's permissive double-bind behaviour.
    #[cfg(unix)]
    #[test]
    fn reuse_port_set_for_udp_only_not_tcp() {
        use socket2::SockRef;

        let mut udp = DrvAsynIPServerPort::new("rp_udp", "127.0.0.1:0 UDP").unwrap();
        udp.connect(&AsynUser::default()).unwrap();
        {
            let g = udp.udp_socket.lock();
            let s = g.as_ref().expect("udp socket bound");
            assert!(
                SockRef::from(&**s).reuse_port().unwrap(),
                "UDP server must enable SO_REUSEPORT (C datagram-fanout helper)"
            );
        }
        udp.disconnect(&AsynUser::default()).unwrap();

        let mut tcp = DrvAsynIPServerPort::new("rp_tcp", "127.0.0.1:0").unwrap();
        tcp.connect(&AsynUser::default()).unwrap();
        {
            let g = tcp.listener.lock();
            let s = g.as_ref().expect("tcp listener bound");
            assert!(
                !SockRef::from(s).reuse_port().unwrap(),
                "TCP listener must NOT set SO_REUSEPORT (fanout is UDP-only in C)"
            );
        }
        tcp.disconnect(&AsynUser::default()).unwrap();
    }

    /// DRV-20: a flush on the UDP server discards the cached datagram (C
    /// `flushIt` resets `UDPbufferPos`/`UDPbufferSize`,
    /// drvAsynIPServerPort.c:244-245), so a flush-then-read waits for a
    /// fresh datagram instead of re-returning the stale one.
    #[test]
    fn udp_server_flush_discards_cached_datagram() {
        let mut srv = DrvAsynIPServerPort::new("udp_flush", "127.0.0.1:0 UDP").unwrap();
        // Seed a datagram directly so the assertion is deterministic.
        {
            let mut cache = srv.udp_cache.lock();
            cache.data = b"stale".to_vec();
            cache.pos = 0;
        }
        assert_eq!(srv.udp_cache_pending(), 5);

        let mut user = AsynUser::default().with_addr(0);
        srv.io_flush(&mut user).unwrap();
        assert_eq!(
            srv.udp_cache_pending(),
            0,
            "flush must discard the cached datagram"
        );

        let mut buf = [0u8; 16];
        let n = srv
            .read_octet(&AsynUser::default().with_addr(0), &mut buf)
            .unwrap();
        assert_eq!(
            n, 0,
            "flush-then-read must not re-return the stale datagram"
        );
    }

    /// DRV-20 (TCP sibling, found in adversarial review): a flush on a
    /// TCP server connection drains staged socket input, matching C's
    /// child `drvAsynIPPort::flushIt` (drvAsynIPPort.c:846-861) — each
    /// server child is a full `drvAsynIPPort` (drvAsynIPServerPort.c:690).
    /// Pre-fix the TCP flush was a no-op and a flush-then-read returned
    /// the stale bytes. Exercises the parent's addr-routed flush path.
    #[test]
    fn tcp_server_flush_drains_staged_socket_input() {
        use std::io::Write as _;
        use std::net::TcpStream as ClientStream;

        let mut srv = DrvAsynIPServerPort::new("tcp_flush_drain", "127.0.0.1:0").unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let port = srv.local_port();

        let mut client = ClientStream::connect(("127.0.0.1", port)).unwrap();
        let idx = srv.accept_one().unwrap();
        client.write_all(b"stale-bytes").unwrap();
        client.flush().unwrap();
        // Let the bytes land on the server socket (instant on loopback).
        std::thread::sleep(Duration::from_millis(50));

        let mut user = AsynUser::default().with_addr(idx as i32);
        srv.io_flush(&mut user).unwrap();

        // The staged bytes were drained: a short-timeout read returns no
        // data (times out) rather than the stale "stale-bytes".
        let read_user = AsynUser::default()
            .with_addr(idx as i32)
            .with_timeout(Duration::from_millis(100));
        let mut buf = [0u8; 64];
        match srv.read_octet(&read_user, &mut buf) {
            Err(AsynError::Status {
                status: AsynStatus::Timeout,
                ..
            }) => {}
            Ok(0) => {}
            other => panic!("expected drained (timeout / 0 bytes), got {other:?}"),
        }

        srv.disconnect(&AsynUser::default()).unwrap();
    }

    /// DRV-20: a TCP flush with no connected client is a harmless no-op
    /// (C guards the drain on `fd != INVALID_SOCKET`).
    #[test]
    fn tcp_server_flush_no_connection_is_harmless() {
        let mut srv = DrvAsynIPServerPort::new("tcp_flush_empty", "127.0.0.1:0").unwrap();
        let mut user = AsynUser::default().with_addr(0);
        srv.io_flush(&mut user).unwrap();
        // Broadcast addr also harmless with no slots occupied.
        let mut bcast = AsynUser::default().with_addr(-1);
        srv.io_flush(&mut bcast).unwrap();
    }

    /// DRV-20 (TCP sibling): the child subport's flush drains its slot's
    /// socket through the same shared owner as the parent.
    #[test]
    fn subport_flush_drains_staged_socket_input() {
        use std::io::Write as _;
        use std::net::TcpStream as ClientStream;

        let mut srv = DrvAsynIPServerPort::new("sub_flush_drain", "127.0.0.1:0").unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let port = srv.local_port();

        let mut client = ClientStream::connect(("127.0.0.1", port)).unwrap();
        let idx = srv.accept_one().unwrap();
        let mut sub = srv.make_subport(idx).unwrap();
        sub.connect(&AsynUser::default()).unwrap();

        client.write_all(b"stale").unwrap();
        client.flush().unwrap();
        std::thread::sleep(Duration::from_millis(50));

        sub.io_flush(&mut AsynUser::default()).unwrap();

        let read_user = AsynUser::default().with_timeout(Duration::from_millis(100));
        let mut buf = [0u8; 64];
        match sub.read_octet(&read_user, &mut buf) {
            Err(AsynError::Status {
                status: AsynStatus::Timeout,
                ..
            }) => {}
            Ok(0) => {}
            other => panic!("expected drained (timeout / 0 bytes), got {other:?}"),
        }

        srv.disconnect(&AsynUser::default()).unwrap();
    }

    /// UDP-mode end-to-end: bind ephemeral, two clients each fire one
    /// datagram, server polls `read_octet` until both arrive (in any
    /// order — kernel scheduling is non-deterministic but C asyn's
    /// single-buffer cache means each `read_octet` returns one
    /// complete datagram).
    #[test]
    fn udp_server_receives_datagrams_from_any_peer() {
        use std::net::UdpSocket as ClientSock;
        let mut srv = DrvAsynIPServerPort::new("udp_srv", "127.0.0.1:0 UDP").unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let server_addr = srv
            .udp_socket
            .lock()
            .as_ref()
            .unwrap()
            .local_addr()
            .unwrap();

        let c1 = ClientSock::bind("127.0.0.1:0").unwrap();
        let c2 = ClientSock::bind("127.0.0.1:0").unwrap();
        c1.send_to(b"alpha", server_addr).unwrap();
        c2.send_to(b"bravo", server_addr).unwrap();

        // Drain two datagrams via polling — read_octet returns 0
        // when cache empty, so loop with a brief sleep until we
        // collect both.
        let user = AsynUser::default()
            .with_addr(0)
            .with_timeout(Duration::from_secs(2));
        let mut got: Vec<String> = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut buf = [0u8; 64];
        while got.len() < 2 && std::time::Instant::now() < deadline {
            let n = srv.read_octet(&user, &mut buf).unwrap();
            if n == 0 {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            got.push(String::from_utf8_lossy(&buf[..n]).to_string());
        }
        got.sort();
        assert_eq!(got, vec!["alpha".to_string(), "bravo".to_string()]);

        srv.disconnect(&AsynUser::default()).unwrap();
    }

    /// UDP write must error — C asyn `writeIt` is `return asynError;`.
    #[test]
    fn udp_server_write_octet_errors() {
        let mut srv = DrvAsynIPServerPort::new("udp_srv2", "127.0.0.1:0 UDP").unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let mut user = AsynUser::default().with_addr(0);
        let err = srv.write_octet(&mut user, b"x").unwrap_err();
        match err {
            AsynError::Status { message, .. } => {
                assert!(
                    message.contains("read-only"),
                    "expected read-only error, got: {message}"
                );
            }
            _ => panic!("wrong error variant"),
        }
        srv.disconnect(&AsynUser::default()).unwrap();
    }

    /// `read_octet` on empty UDP cache returns 0 (NOT an error) — the
    /// C asyn semantics is "poll", not "block". Caller is expected to
    /// retry or use the I/O Intr path.
    #[test]
    fn udp_server_read_returns_zero_when_empty() {
        let mut srv = DrvAsynIPServerPort::new("udp_srv3", "127.0.0.1:0 UDP").unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let user = AsynUser::default()
            .with_addr(0)
            .with_timeout(Duration::from_millis(50));
        let mut buf = [0u8; 64];
        let n = srv.read_octet(&user, &mut buf).unwrap();
        assert_eq!(n, 0, "empty UDP cache must return 0 bytes, not error");
        srv.disconnect(&AsynUser::default()).unwrap();
    }

    /// DRV-23 regression: a UDP datagram is a message boundary.
    /// `io_read_octet_eom` must report `ASYN_EOM_END` when the datagram
    /// is fully drained and `ASYN_EOM_CNT` when the caller buffer is too
    /// small and more of the datagram remains (C drvAsynIPServerPort.c
    /// readIt:201-207). The default synthesis reports CNT-only, never END.
    #[test]
    fn udp_server_read_eom_reports_end_at_datagram_boundary() {
        let mut srv = DrvAsynIPServerPort::new("udp_srv_eom", "127.0.0.1:0 UDP").unwrap();
        // Seed the cache directly so the assertion is deterministic (no
        // datagram-arrival race).
        {
            let mut cache = srv.udp_cache.lock();
            cache.data = b"hello".to_vec();
            cache.pos = 0;
        }
        let user = AsynUser::default().with_addr(0);

        // Caller buffer too small (3 < 5): partial drain -> CNT, no END.
        let mut small = [0u8; 3];
        let (n, eom) = srv.io_read_octet_eom(&user, &mut small).unwrap();
        assert_eq!(n, 3);
        assert_eq!(&small[..3], b"hel");
        assert!(eom.contains(EomReason::CNT), "partial drain must flag CNT");
        assert!(
            !eom.contains(EomReason::END),
            "partial drain must NOT flag END"
        );

        // Remainder fits: full drain -> END, no CNT.
        let mut rest = [0u8; 16];
        let (n, eom) = srv.io_read_octet_eom(&user, &mut rest).unwrap();
        assert_eq!(n, 2);
        assert_eq!(&rest[..2], b"lo");
        assert!(
            eom.contains(EomReason::END),
            "datagram boundary must flag END"
        );
        assert!(
            !eom.contains(EomReason::CNT),
            "full drain must NOT flag CNT"
        );

        // Empty cache -> (0, empty): a poll, no boundary to report.
        let mut buf = [0u8; 16];
        let (n, eom) = srv.io_read_octet_eom(&user, &mut buf).unwrap();
        assert_eq!(n, 0);
        assert!(eom.is_empty(), "empty cache poll reports no EOM");

        // Exact fit: a datagram that exactly fills the caller buffer is
        // both fully drained (END) and buffer-filled (CNT) — the two
        // conditions are independent (C readIt:201-204 + :232-235,
        // de-deadened off-by-one).
        {
            let mut cache = srv.udp_cache.lock();
            cache.data = b"abcd".to_vec();
            cache.pos = 0;
        }
        let mut exact = [0u8; 4];
        let (n, eom) = srv.io_read_octet_eom(&user, &mut exact).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&exact, b"abcd");
        assert!(eom.contains(EomReason::END), "exact fit must flag END");
        assert!(eom.contains(EomReason::CNT), "exact fit must also flag CNT");
        // And the datagram is gone afterwards (a poll returns empty).
        let mut after = [0u8; 4];
        let (n, eom) = srv.io_read_octet_eom(&user, &mut after).unwrap();
        assert_eq!(n, 0);
        assert!(eom.is_empty());
    }

    /// disconnect must stop the worker cleanly so a subsequent
    /// connect/disconnect cycle works (the previous worker thread
    /// must have released its socket Arc).
    #[test]
    fn udp_server_disconnect_stops_worker_cleanly() {
        let mut srv = DrvAsynIPServerPort::new("udp_srv4", "127.0.0.1:0 UDP").unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        srv.disconnect(&AsynUser::default()).unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        srv.disconnect(&AsynUser::default()).unwrap();
    }

    /// BUG 3 regression: `shutdown()` (called by the actor on a normal
    /// channel-close teardown, NOT `disconnect()`) must stop and join
    /// the UDP recv worker. Pre-fix `shutdown()` was the trait default
    /// no-op and the worker looped forever holding the bound socket.
    ///
    /// Verified two ways: (1) the worker thread terminates — checked
    /// by re-binding the same ephemeral port is not possible, so we
    /// instead confirm a fresh connect/disconnect cycle succeeds after
    /// shutdown (the worker released its socket Arc); (2) the join
    /// completes promptly (≤ ~1s — the worker's 200ms recv timeout
    /// caps latency).
    #[test]
    fn udp_server_shutdown_joins_recv_worker() {
        let mut srv = DrvAsynIPServerPort::new("udp_srv_sd", "127.0.0.1:0 UDP").unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        // Worker thread is running.
        assert!(srv.udp_thread.lock().is_some());

        let start = std::time::Instant::now();
        // shutdown() — NOT disconnect() — is the actor's normal
        // teardown call.
        srv.shutdown().unwrap();
        let elapsed = start.elapsed();

        // Worker handle was taken and joined.
        assert!(
            srv.udp_thread.lock().is_none(),
            "shutdown must join and clear the recv worker handle"
        );
        // Socket released.
        assert!(
            srv.udp_socket.lock().is_none(),
            "shutdown must drop the UDP socket"
        );
        // Join completed promptly — the worker observed udp_shutdown.
        assert!(
            elapsed < Duration::from_secs(3),
            "shutdown join took too long ({elapsed:?}) — worker did not exit"
        );

        // The socket was released, so a fresh connect/disconnect cycle
        // works (would fail if the old worker still held the port).
        srv.connect(&AsynUser::default()).unwrap();
        srv.disconnect(&AsynUser::default()).unwrap();
    }

    /// `shutdown()` on a TCP server is a benign no-op for the UDP
    /// worker path (no worker thread exists) and still releases the
    /// listener.
    #[test]
    fn tcp_server_shutdown_releases_listener() {
        let mut srv = DrvAsynIPServerPort::new("tcp_srv_sd", "127.0.0.1:0").unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        assert!(srv.listener.lock().is_some());
        srv.shutdown().unwrap();
        assert!(
            srv.listener.lock().is_none(),
            "shutdown must drop the TCP listener"
        );
    }

    #[test]
    fn parse_rejects_missing_port() {
        assert!(IpServerConfig::parse("0.0.0.0").is_err());
    }

    #[test]
    fn parse_rejects_unknown_protocol_token() {
        let err = IpServerConfig::parse("0.0.0.0:8080 BOGUS").unwrap_err();
        match err {
            AsynError::Status { message, .. } => {
                assert!(
                    message.contains("unknown protocol token") || message.contains("BOGUS"),
                    "msg={message}"
                );
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

    /// Child port name matches C `epicsSnprintf(pl->portName, len,
    /// "%s:%d", tty->portName, i)` at drvAsynIPServerPort.c:688.
    #[test]
    fn child_port_names_match_c_convention() {
        let cfg = IpServerConfig {
            bind_host: "127.0.0.1".into(),
            bind_port: 0,
            protocol: IpServerProtocol::Tcp,
            max_clients: 3,
            read_timeout: None,
        };
        let srv = DrvAsynIPServerPort::with_config("parent", cfg).unwrap();
        assert_eq!(srv.child_port_name(0), "parent:0");
        assert_eq!(srv.child_port_name(1), "parent:1");
        assert_eq!(srv.child_port_name(2), "parent:2");
        assert_eq!(
            srv.child_port_names(),
            vec![
                "parent:0".to_string(),
                "parent:1".to_string(),
                "parent:2".to_string()
            ]
        );
    }

    /// make_subport on an out-of-range index errors rather than panic.
    #[test]
    fn make_subport_rejects_out_of_range_idx() {
        let cfg = IpServerConfig {
            bind_host: "127.0.0.1".into(),
            bind_port: 0,
            protocol: IpServerProtocol::Tcp,
            max_clients: 2,
            read_timeout: None,
        };
        let srv = DrvAsynIPServerPort::with_config("p2", cfg).unwrap();
        assert!(srv.make_subport(0).is_ok());
        assert!(srv.make_subport(1).is_ok());
        match srv.make_subport(2) {
            Err(AsynError::Status { message, .. }) => {
                assert!(message.contains("out of range"), "msg={message}");
            }
            Ok(_) => panic!("expected out-of-range error"),
            Err(other) => panic!("expected Status error, got {other:?}"),
        }
    }

    /// Subport shares the slot with the parent: parent.accept_one
    /// fills slot 0, subport's connect() then succeeds and its
    /// read_octet/write_octet operate on the same TCP stream as the
    /// parent's addr=0 path. Mirrors C drvAsynIPServerPort.c:357-360
    /// where the parent assigns the FD to the child port and triggers
    /// its connectDevice.
    #[test]
    fn subport_shares_slot_with_parent_after_accept() {
        let cfg = IpServerConfig {
            bind_host: "127.0.0.1".into(),
            bind_port: 0,
            protocol: IpServerProtocol::Tcp,
            max_clients: 1,
            read_timeout: None,
        };
        let mut srv = DrvAsynIPServerPort::with_config("psh", cfg).unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let port = srv.local_port();

        let mut sub = srv.make_subport(0).unwrap();
        // Before any client connects, subport's connect() must error
        // — no FD assigned yet.
        assert!(sub.connect(&AsynUser::default()).is_err());

        let client_handle = std::thread::spawn(move || {
            let mut c = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
            let mut buf = [0u8; 5];
            let _ = c.read(&mut buf).unwrap();
            buf
        });

        assert_eq!(srv.accept_one().unwrap(), 0);
        // Subport now sees the assigned stream.
        sub.connect(&AsynUser::default()).unwrap();
        assert!(sub.peer().is_some());

        // Write via the subport — the client receives it on the same
        // TCP stream the parent assigned.
        let mut user = AsynUser::default();
        sub.write_octet(&mut user, b"hello").unwrap();
        let buf = client_handle.join().unwrap();
        assert_eq!(&buf, b"hello");
    }
}
