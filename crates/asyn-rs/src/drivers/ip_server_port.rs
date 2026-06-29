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
//! `tcp` (default) / `udp` are accepted as the protocol token.
//! `SO_REUSEADDR` is set unconditionally on the listening socket
//! (`drvAsynIPServerPort.c:430`); there is **no `SO_REUSEPORT` token
//! in upstream C asyn** — earlier versions of this module accepted
//! one but it has been removed for parity. See [the audit doc] for
//! the divergence note.
//!
//! - `host` may be `"0.0.0.0"` (all IPv4) or a specific bind address.
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
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
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
        let bind_str = if self.config.bind_host.contains(':') {
            // IPv6
            format!("[{}]:{}", self.config.bind_host, self.config.bind_port)
        } else {
            format!("{}:{}", self.config.bind_host, self.config.bind_port)
        };

        // socket2 path so SO_REUSEADDR is set explicitly — mirrors
        // C asyn's unconditional setsockopt at drvAsynIPServerPort.c:430.
        let listener = self.bind_with_options(&bind_str)?;
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
        let bind_str = if self.config.bind_host.contains(':') {
            format!("[{}]:{}", self.config.bind_host, self.config.bind_port)
        } else {
            format!("{}:{}", self.config.bind_host, self.config.bind_port)
        };
        let addr: SocketAddr =
            bind_str
                .parse()
                .map_err(|e: std::net::AddrParseError| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("invalid UDP bind address '{bind_str}': {e}"),
                })?;
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
        // C asyn sets SO_REUSEADDR unconditionally on the bound
        // socket (drvAsynIPServerPort.c:430). No SO_REUSEPORT — that
        // was an invented extension and has been removed.
        sock.set_reuse_address(true)
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("UDP SO_REUSEADDR failed: {e}"),
            })?;
        sock.bind(&addr.into()).map_err(|e| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("UDP bind '{bind_str}' failed: {e}"),
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
        // Unconditional SO_REUSEADDR (drvAsynIPServerPort.c:430).
        socket
            .set_reuse_address(true)
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("SO_REUSEADDR failed: {e}"),
            })?;
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
        let mut cache = self.udp_cache.lock();
        if cache.is_empty() {
            return 0;
        }
        let avail = cache.data.len() - cache.pos;
        let n = avail.min(buf.len());
        buf[..n].copy_from_slice(&cache.data[cache.pos..cache.pos + n]);
        cache.pos += n;
        if cache.is_empty() {
            cache.clear();
        }
        n
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
