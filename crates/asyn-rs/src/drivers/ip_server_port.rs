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

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
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

/// Per-accepted-connection state. Either a TCP stream (one
/// long-lived socket per peer) or a UDP inbox queue (datagrams
/// sourced from one specific peer addr, drained by `read_octet`).
struct ClientSlot {
    transport: SlotTransport,
    peer: SocketAddr,
}

enum SlotTransport {
    Tcp(Mutex<Option<TcpStream>>),
    /// UDP inbox: `recv_from` worker pushes datagrams here; the
    /// `read_octet` consumer pops one per call. Condvar wakes the
    /// consumer when a datagram arrives, so reads block until data
    /// arrives instead of busy-polling.
    Udp(Mutex<UdpInbox>, Condvar),
}

struct UdpInbox {
    queue: VecDeque<Vec<u8>>,
    /// Number of datagrams dropped because the inbox was at
    /// `UDP_INBOX_DEPTH` capacity. Surfaced via [`DrvAsynIPServerPort::udp_drops`].
    /// Drop-newest matches the kernel SO_RXQ_OVFL semantics — once
    /// we're behind, dropping fresh packets keeps the queue moving
    /// rather than stalling on stale data.
    drops: u64,
}

/// Cap per-slot UDP inbox depth so a slow consumer can't exhaust
/// memory. Matches the C asyn `MAX_UDP_QUEUE` rough order of
/// magnitude; tunable later if a real workload demands.
pub const UDP_INBOX_DEPTH: usize = 256;

impl ClientSlot {
    fn new_tcp(stream: TcpStream, peer: SocketAddr) -> Self {
        Self {
            transport: SlotTransport::Tcp(Mutex::new(Some(stream))),
            peer,
        }
    }

    fn new_udp(peer: SocketAddr) -> Self {
        Self {
            transport: SlotTransport::Udp(
                Mutex::new(UdpInbox {
                    queue: VecDeque::new(),
                    drops: 0,
                }),
                Condvar::new(),
            ),
            peer,
        }
    }
}

/// Server-mode IP port driver.
pub struct DrvAsynIPServerPort {
    base: PortDriverBase,
    config: IpServerConfig,
    listener: Mutex<Option<TcpListener>>,
    /// UDP listener socket — bound when `protocol == Udp` after
    /// `connect()`. Shared with the recv thread via Arc so the
    /// thread can `recv_from` and `read_octet`/`write_octet` can
    /// `send_to`.
    udp_socket: Mutex<Option<Arc<UdpSocket>>>,
    /// UDP recv worker thread. Owns the loop that demultiplexes
    /// datagrams to per-peer slots. Joined on `disconnect`.
    udp_thread: Mutex<Option<JoinHandle<()>>>,
    /// Shutdown flag set by `disconnect` and observed by the UDP
    /// recv thread between `recv_from` calls (woken by socket
    /// read timeout — 200ms).
    udp_shutdown: Arc<AtomicBool>,
    /// UDP peer→slot index. Built lazily by the recv thread when
    /// a datagram from a previously-unseen peer arrives. Locked
    /// briefly during slot allocation; not held during recv.
    udp_peer_map: Arc<Mutex<HashMap<SocketAddr, usize>>>,
    /// Signaled by the UDP recv worker after a previously-empty slot
    /// gets assigned to a peer. Lets `read_octet(addr)` block on an
    /// empty UDP slot until the recv worker fills it — the natural
    /// "wait for the first peer to talk" semantic. Tuple is a
    /// no-state Mutex paired with the Condvar so threads can wait.
    udp_slot_assigned: Arc<(Mutex<()>, Condvar)>,
    /// Fixed-size client slot table. `slots[addr]` is `None` until a
    /// connection is assigned to that addr. Slot reuse on disconnect
    /// keeps `addr` stable for the lifetime of the connection.
    slots: Arc<Vec<Mutex<Option<Arc<ClientSlot>>>>>,
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
            udp_socket: Mutex::new(None),
            udp_thread: Mutex::new(None),
            udp_shutdown: Arc::new(AtomicBool::new(false)),
            udp_peer_map: Arc::new(Mutex::new(HashMap::new())),
            udp_slot_assigned: Arc::new((Mutex::new(()), Condvar::new())),
            slots: Arc::new(slots),
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

    fn open_udp_listener(&mut self) -> AsynResult<()> {
        let bind_str = if self.config.bind_host.contains(':') {
            format!("[{}]:{}", self.config.bind_host, self.config.bind_port)
        } else {
            format!("{}:{}", self.config.bind_host, self.config.bind_port)
        };

        let socket = self.bind_udp_with_options(&bind_str)?;
        // 200ms recv timeout so the worker thread wakes periodically
        // to observe `udp_shutdown` without paying a large shutdown
        // latency. Trade-off: this is the worst-case shutdown delay
        // a `disconnect()` call observes.
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("UDP set_read_timeout failed: {e}"),
            })?;
        let socket = Arc::new(socket);

        // Reset shutdown flag for a fresh worker.
        self.udp_shutdown.store(false, Ordering::SeqCst);

        // Spawn the recv worker. Owns Arc clones of: socket,
        // peer_map, slots vec, shutdown flag, base for exception
        // announce. No mutable state crosses the thread boundary.
        let socket_t = Arc::clone(&socket);
        let shutdown_t = Arc::clone(&self.udp_shutdown);
        let peer_map_t = Arc::clone(&self.udp_peer_map);
        let slots_t = Arc::clone(&self.slots);
        let slot_assigned_t = Arc::clone(&self.udp_slot_assigned);
        let exception_sink = self.base.exception_sink.clone();
        let max_clients = self.config.max_clients.max(1);
        let port_name = self.base.port_name.clone();

        let handle = std::thread::Builder::new()
            .name(format!("udp-server-{port_name}"))
            .spawn(move || {
                udp_recv_loop(
                    socket_t,
                    shutdown_t,
                    peer_map_t,
                    slots_t,
                    slot_assigned_t,
                    max_clients,
                    exception_sink,
                    port_name,
                );
            })
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("UDP recv thread spawn failed: {e}"),
            })?;

        *self.udp_socket.lock().unwrap() = Some(socket);
        *self.udp_thread.lock().unwrap() = Some(handle);
        self.base.connected = true;
        self.base.announce_exception(AsynException::Connect, -1);
        Ok(())
    }

    fn bind_udp_with_options(&self, bind_str: &str) -> AsynResult<UdpSocket> {
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
        let socket =
            socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))
                .map_err(|e| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("UDP socket() failed: {e}"),
                })?;
        socket
            .set_reuse_address(true)
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("UDP SO_REUSEADDR failed: {e}"),
            })?;
        if self.config.reuse_port {
            #[cfg(unix)]
            {
                if let Err(e) = socket.set_reuse_port(true) {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("UDP SO_REUSEPORT failed: {e}"),
                    });
                }
            }
        }
        socket.bind(&addr.into()).map_err(|e| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("UDP bind '{bind_str}' failed: {e}"),
        })?;
        Ok(UdpSocket::from(socket))
    }

    /// Total UDP datagrams dropped due to per-slot inbox saturation,
    /// summed across every UDP slot. Operators can poll this to size
    /// `UDP_INBOX_DEPTH` against real load.
    pub fn udp_drops(&self) -> u64 {
        let mut total = 0u64;
        for slot in self.slots.iter() {
            if let Some(arc) = slot.lock().unwrap().clone() {
                if let SlotTransport::Udp(m, _) = &arc.transport {
                    total += m.lock().unwrap().drops;
                }
            }
        }
        total
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
                *g = Some(Arc::new(ClientSlot::new_tcp(stream, peer)));
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
        let already_up = self.base.connected
            && (self.listener.lock().unwrap().is_some()
                || self.udp_socket.lock().unwrap().is_some());
        if already_up {
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
        // Stop the UDP recv worker (if any) before dropping the socket
        // so the loop observes the shutdown flag and exits cleanly
        // instead of racing against socket close.
        self.udp_shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.udp_thread.lock().unwrap().take() {
            // Join is best-effort — a panicked worker mustn't block
            // shutdown of the rest of the IOC.
            let _ = handle.join();
        }
        self.udp_peer_map.lock().unwrap().clear();
        *self.udp_socket.lock().unwrap() = None;
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
    /// Wait up to `timeout` for the UDP slot at `addr` to be assigned
    /// to a peer (i.e. the recv worker has seen at least one datagram
    /// destined for that slot). Returns `Ok(())` once the slot is
    /// populated, `Err(Timeout)` when the deadline elapses. TCP slots
    /// must already be populated by `accept_one` before read_octet, so
    /// this function is UDP-only.
    fn wait_for_slot_assignment(&self, addr: i32, timeout: Duration) -> AsynResult<()> {
        let idx = self.slot_index(addr)?;
        let deadline = std::time::Instant::now() + timeout;
        let (lock, cv) = &*self.udp_slot_assigned;
        let mut g = lock.lock().unwrap();
        loop {
            if self.slots[idx].lock().unwrap().is_some() {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: format!(
                        "UDP slot {addr} has no peer yet — timed out waiting for first datagram"
                    ),
                });
            }
            let (g2, res) = cv.wait_timeout(g, remaining).unwrap();
            g = g2;
            if res.timed_out() {
                // Loop one more iteration to re-check slot — wake
                // could be spurious; if still empty, the deadline
                // check above will return Timeout.
                continue;
            }
        }
    }

    fn base_read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
        // For UDP slots, an empty slot may simply mean "the first peer
        // hasn't sent a datagram yet" — wait up to user.timeout for
        // the recv worker to populate, then proceed with the regular
        // slot-occupied read.
        if self.config.protocol == IpServerProtocol::Udp
            && self.slots[self.slot_index(user.addr)?]
                .lock()
                .unwrap()
                .is_none()
        {
            let wait = if user.timeout > Duration::from_nanos(0) {
                user.timeout
            } else {
                Duration::from_secs(60)
            };
            self.wait_for_slot_assignment(user.addr, wait)?;
        }
        let arc = self.slot_arc(user.addr)?;
        match &arc.transport {
            SlotTransport::Tcp(stream_mu) => {
                let mut stream_guard = stream_mu.lock().unwrap();
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
            SlotTransport::Udp(inbox_mu, cv) => {
                // Block until the recv worker pushes a datagram, or
                // the user-supplied timeout elapses. Zero timeout
                // means "wait forever" (matches asyn convention for
                // unset timeouts).
                let mut g = inbox_mu.lock().unwrap();
                let dg = if user.timeout > Duration::from_nanos(0) {
                    let (mut g2, _) = cv
                        .wait_timeout_while(g, user.timeout, |x| x.queue.is_empty())
                        .unwrap();
                    g2.queue.pop_front()
                } else {
                    while g.queue.is_empty() {
                        g = cv.wait(g).unwrap();
                    }
                    g.queue.pop_front()
                };
                match dg {
                    Some(payload) => {
                        let n = payload.len().min(buf.len());
                        buf[..n].copy_from_slice(&payload[..n]);
                        Ok(OctetReadResult {
                            nbytes_transferred: n,
                            eom_reason: EomReason::END,
                        })
                    }
                    None => Err(AsynError::Status {
                        status: AsynStatus::Timeout,
                        message: "UDP slot read timeout".into(),
                    }),
                }
            }
        }
    }

    fn write_to_slot(&self, slot: &ClientSlot, data: &[u8]) -> AsynResult<()> {
        match &slot.transport {
            SlotTransport::Tcp(stream_mu) => {
                let mut g = stream_mu.lock().unwrap();
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
            SlotTransport::Udp(_, _) => {
                // UDP write goes back to the slot's source peer via
                // the shared listener socket. Hold the socket Arc
                // briefly to fire one send_to, then release.
                let socket_guard = self.udp_socket.lock().unwrap();
                let socket = socket_guard.as_ref().ok_or_else(|| AsynError::Status {
                    status: AsynStatus::Error,
                    message: "UDP socket gone".into(),
                })?;
                socket
                    .send_to(data, slot.peer)
                    .map_err(|e| AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("UDP send_to {} failed: {e}", slot.peer),
                    })?;
                Ok(())
            }
        }
    }
}

/// UDP recv worker — bound to the listener socket via `Arc`. Loops
/// `recv_from`, demultiplexes by source addr to a slot index (allocating
/// on first contact), and pushes the datagram into the slot's UDP inbox.
/// Exits when `shutdown` is set; the socket's 200ms read timeout bounds
/// shutdown latency.
fn udp_recv_loop(
    socket: Arc<UdpSocket>,
    shutdown: Arc<AtomicBool>,
    peer_map: Arc<Mutex<HashMap<SocketAddr, usize>>>,
    slots: Arc<Vec<Mutex<Option<Arc<ClientSlot>>>>>,
    slot_assigned: Arc<(Mutex<()>, Condvar)>,
    max_clients: usize,
    exception_sink: Option<Arc<crate::exception::ExceptionManager>>,
    port_name: String,
) {
    // 64 KiB matches the IPv4 max datagram payload — sufficient for
    // every well-formed UDP packet we'd receive. Allocate once, reuse.
    let mut buf = vec![0u8; 65_535];
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        match socket.recv_from(&mut buf) {
            Ok((n, peer)) => {
                let (slot_idx, newly_assigned) =
                    match resolve_or_assign_udp_slot(&peer_map, &slots, peer, max_clients) {
                        Some(pair) => pair,
                        None => {
                            tracing::debug!(
                                target: "asyn_rs::ip_server_port",
                                peer = %peer,
                                port = %port_name,
                                "UDP datagram dropped: no free slot for new peer"
                            );
                            continue;
                        }
                    };
                if let Some(slot_arc) = slots[slot_idx].lock().unwrap().clone() {
                    if let SlotTransport::Udp(inbox_mu, cv) = &slot_arc.transport {
                        let mut inbox = inbox_mu.lock().unwrap();
                        if inbox.queue.len() >= UDP_INBOX_DEPTH {
                            inbox.drops += 1;
                        } else {
                            inbox.queue.push_back(buf[..n].to_vec());
                            cv.notify_one();
                        }
                    }
                }
                if newly_assigned {
                    // Wake any `read_octet` callers blocked on a
                    // previously-empty slot — they re-check whether
                    // their slot just got assigned.
                    let (lock, cv) = &*slot_assigned;
                    let _g = lock.lock().unwrap();
                    cv.notify_all();
                }
                // Announce Connect exception only on first-contact
                // (newly assigned). Avoids spamming the sink on
                // every datagram from a known peer.
                if newly_assigned {
                    if let Some(ref sink) = exception_sink {
                        sink.announce(&crate::exception::ExceptionEvent {
                            port_name: port_name.clone(),
                            exception: AsynException::Connect,
                            addr: slot_idx as i32,
                        });
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
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

/// Resolve a UDP source peer to a slot index, allocating one on
/// first contact. Returns `(slot_idx, newly_assigned)` so the caller
/// can distinguish "known peer reusing its slot" from "first-contact
/// peer — wake any waiters and announce a Connect exception". `None`
/// when no free slot is available for a new peer.
fn resolve_or_assign_udp_slot(
    peer_map: &Arc<Mutex<HashMap<SocketAddr, usize>>>,
    slots: &Arc<Vec<Mutex<Option<Arc<ClientSlot>>>>>,
    peer: SocketAddr,
    max_clients: usize,
) -> Option<(usize, bool)> {
    let mut map = peer_map.lock().unwrap();
    if let Some(&idx) = map.get(&peer) {
        return Some((idx, false));
    }
    for i in 0..max_clients {
        let mut g = slots[i].lock().unwrap();
        if g.is_none() {
            *g = Some(Arc::new(ClientSlot::new_udp(peer)));
            map.insert(peer, i);
            return Some((i, true));
        }
    }
    None
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

    /// End-to-end UDP server: bind ephemeral, two clients send
    /// datagrams from distinct source ports, each lands in its own
    /// slot, server writes back to each slot, clients receive.
    #[test]
    fn udp_server_two_peer_round_trip() {
        let mut srv = DrvAsynIPServerPort::new("udp_srv", "127.0.0.1:0 UDP").unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let server_addr = srv
            .udp_socket
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .local_addr()
            .unwrap();

        // Two client sockets — each gets its own ephemeral source port,
        // so the server sees two distinct peers.
        let c1 = UdpSocket::bind("127.0.0.1:0").unwrap();
        let c2 = UdpSocket::bind("127.0.0.1:0").unwrap();
        c1.send_to(b"from-c1", server_addr).unwrap();
        c2.send_to(b"from-c2", server_addr).unwrap();

        // Read each slot — the recv worker will have demultiplexed
        // the two datagrams to slots 0 and 1 (allocation order).
        let user0 = AsynUser::new(0)
            .with_addr(0)
            .with_timeout(Duration::from_secs(2));
        let user1 = AsynUser::new(0)
            .with_addr(1)
            .with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 64];
        let n0 = srv.read_octet(&user0, &mut buf).unwrap();
        // Source-order is not guaranteed by the kernel for UDP from
        // distinct sockets; classify by payload rather than slot.
        let s0 = std::str::from_utf8(&buf[..n0]).unwrap().to_string();
        let mut buf = [0u8; 64];
        let n1 = srv.read_octet(&user1, &mut buf).unwrap();
        let s1 = std::str::from_utf8(&buf[..n1]).unwrap().to_string();
        let mut got = vec![s0, s1];
        got.sort();
        assert_eq!(got, vec!["from-c1".to_string(), "from-c2".to_string()]);

        // Server writes back to slot 0 — datagram lands at the client
        // that owns slot 0 (whichever it was). We verify the response
        // by reading whichever client got it.
        let mut user_w = AsynUser::new(0)
            .with_addr(0)
            .with_timeout(Duration::from_secs(2));
        srv.write_octet(&mut user_w, b"reply-to-0").unwrap();
        let mut rx = [0u8; 64];
        c1.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        c2.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        // One of the two clients receives — whichever was assigned slot 0.
        let recv = c1.recv_from(&mut rx).map(|(n, _)| n).or_else(|_| {
            c2.recv_from(&mut rx).map(|(n, _)| n)
        });
        let n = recv.unwrap();
        assert_eq!(&rx[..n], b"reply-to-0");

        srv.disconnect(&AsynUser::default()).unwrap();
    }

    /// UDP recv worker must demultiplex datagrams from the same peer
    /// into one slot — repeated datagrams from one source go into one
    /// slot's queue, not new slots.
    #[test]
    fn udp_server_repeated_peer_uses_one_slot() {
        let mut srv = DrvAsynIPServerPort::new("udp_srv2", "127.0.0.1:0 UDP").unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let server_addr = srv
            .udp_socket
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .local_addr()
            .unwrap();

        let c1 = UdpSocket::bind("127.0.0.1:0").unwrap();
        c1.send_to(b"d1", server_addr).unwrap();
        c1.send_to(b"d2", server_addr).unwrap();
        c1.send_to(b"d3", server_addr).unwrap();

        let user = AsynUser::new(0)
            .with_addr(0)
            .with_timeout(Duration::from_secs(2));
        let mut got = Vec::new();
        for _ in 0..3 {
            let mut buf = [0u8; 64];
            let n = srv.read_octet(&user, &mut buf).unwrap();
            got.push(std::str::from_utf8(&buf[..n]).unwrap().to_string());
        }
        assert_eq!(got, vec!["d1", "d2", "d3"]);
        // Slot 1 must remain empty — only one peer ever wrote.
        assert!(srv.peer(1).is_none());

        srv.disconnect(&AsynUser::default()).unwrap();
    }

    /// Disconnect must stop the recv worker cleanly (join returns).
    /// Verified indirectly: reconnecting on a fresh socket works
    /// (the previous thread released its socket Arc).
    #[test]
    fn udp_server_disconnect_stops_worker() {
        let mut srv = DrvAsynIPServerPort::new("udp_srv3", "127.0.0.1:0 UDP").unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        srv.disconnect(&AsynUser::default()).unwrap();
        // Re-connect — fresh socket, fresh worker. If the previous
        // worker hadn't exited, the ephemeral port could collide
        // (or the JoinHandle would leak).
        srv.connect(&AsynUser::default()).unwrap();
        srv.disconnect(&AsynUser::default()).unwrap();
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
