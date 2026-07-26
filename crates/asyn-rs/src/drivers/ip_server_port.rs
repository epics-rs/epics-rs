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
use std::time::{Duration, SystemTime};

// parking_lot::Mutex — consistent with the rest of asyn-rs and
// poison-tolerant: a panic in a worker thread cannot poison the lock
// and take out the port (std::sync::Mutex would).
use epics_libcom_rs::runtime::socket;
use parking_lot::Mutex;

use crate::asyn_trace;
use crate::drivers::ip_port::{
    DrvAsynIPPort, is_nonfatal_read_timeout, maxchars_zero_error, socket_poll_timeout,
};
use crate::drivers::option_parse::parse_yn_option;
use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::exception::AsynException;
use crate::interfaces::InterfaceType;
use crate::interpose::{EomReason, OctetReadResult};
use crate::interrupt::{InterruptManager, InterruptValue, OctetFanOut};
use crate::param::ParamValue;
use crate::port::{ExceptionAnnouncer, PortDriver, PortDriverBase, PortFlags};
use crate::trace::{TraceManager, TraceMask};
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
    /// C `tty->noProcessEos` — the sixth `drvAsynIPServerPortConfigure`
    /// argument, whose ONLY use in C is to be handed to each child port's
    /// `drvAsynIPPortConfigure` (drvAsynIPServerPort.c:688-694). It lives on the
    /// server's config because that is what it governs: whether the ports the
    /// server creates get C's default EOS interpose. A server therefore cannot
    /// exist without having decided its children's EOS policy.
    pub no_process_eos: bool,
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
            // C's default: `drvAsynIPServerPortConfigure` with a zero/omitted
            // noProcessEos gives every child the EOS interpose.
            no_process_eos: false,
            read_timeout: None,
        })
    }
}

/// A socket failure on a client slot, and what it did to the slot.
///
/// The `closed` half is not advice: [`ClientSlot::read_or_close`] /
/// [`ClientSlot::write_or_close`] have *already* dropped the socket when it is
/// set — C decides the teardown and the reported status in one statement
/// (drvAsynIPPort.c:797-806), so no caller can take the error and forget the
/// `closeConnection`. What is left to the owner is the disconnect exception
/// fan-out, which differs by port shape: the parent announces on the client's
/// `addr`, the child port announces on its own single device and drops its
/// port-level connected flag.
struct SlotFailure {
    closed: bool,
    error: AsynError,
}

impl SlotFailure {
    /// The socket survives — C's `asynTimeout` branch, and the pre-socket
    /// refusals (`maxchars == 0`, no client in the slot).
    fn kept(error: AsynError) -> Self {
        Self {
            closed: false,
            error,
        }
    }

    /// C ran `closeConnection`: the slot is free and the port owes the
    /// disconnect exception.
    fn closed(error: AsynError) -> Self {
        Self {
            closed: true,
            error,
        }
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
    /// The slot's occupancy, and — because the child port shares this very cell
    /// ([`PortDriverBase::share_connection`]) — the child port's `connected` flag.
    /// They are one bit, so "the slot holds a live client while `parent:N` reports
    /// `asynDisconnected`" cannot be constructed (R13-50).
    ///
    /// C reaches the same invariant by message: on slot reuse the listener sets
    /// `pl->fd = clientFd` and calls `pasynCommonSyncIO->connectDevice(pl->pasynUser)`
    /// on the child (drvAsynIPServerPort.c:357-367), and it is the child's own
    /// `isConnected` that the listener's free-slot scan then reads (:342-350).
    /// Assignment and connectivity are not allowed to disagree in C either.
    occupied: Arc<AtomicBool>,
    /// C `tty->disconnectOnReadTimeout` on this slot's child port
    /// (drvAsynIPPort.c:924-935, set through `asynSetOption`). It lives on the
    /// slot, not on a client, because C's child port outlives the connections it
    /// serves: the option a startup script sets on `srv:0` still holds for the
    /// next client that lands in slot 0.
    disconnect_on_read_timeout: AtomicBool,
}

impl ClientSlot {
    fn new_empty() -> Self {
        Self {
            stream: Mutex::new(None),
            peer: Mutex::new(None),
            occupied: Arc::new(AtomicBool::new(false)),
            disconnect_on_read_timeout: AtomicBool::new(false),
        }
    }

    /// C `tty->disconnectOnReadTimeout` for this slot's child port.
    fn set_disconnect_on_read_timeout(&self, yes: bool) {
        self.disconnect_on_read_timeout
            .store(yes, Ordering::Release);
    }

    fn disconnect_on_read_timeout(&self) -> bool {
        self.disconnect_on_read_timeout.load(Ordering::Acquire)
    }

    fn is_occupied(&self) -> bool {
        self.occupied.load(Ordering::Acquire)
    }

    /// The cell the child port binds its `connected` flag to.
    fn connection_cell(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.occupied)
    }

    fn assign(&self, stream: TcpStream, peer: SocketAddr) {
        *self.stream.lock() = Some(stream);
        *self.peer.lock() = Some(peer);
        // Publish last: the socket and the peer are in place before any observer
        // (the child port's actor, the parent's free-slot scan) can see the slot
        // as occupied.
        self.occupied.store(true, Ordering::Release);
    }

    fn clear(&self) {
        *self.stream.lock() = None;
        *self.peer.lock() = None;
        self.occupied.store(false, Ordering::Release);
    }

    fn peer_addr(&self) -> Option<SocketAddr> {
        *self.peer.lock()
    }

    /// Read from this slot, applying C's failed-read rule and performing the
    /// `closeConnection` half of it — the single owner of "a client slot dies on
    /// a read", shared by the parent's addressed read
    /// ([`DrvAsynIPServerPort::base_read_octet`]) and the child port's
    /// ([`DrvAsynIPSubport::read_octet`]).
    ///
    /// C serves each accepted connection with a full `drvAsynIPPort` child
    /// (drvAsynIPServerPort.c:681-708), so the read that runs is
    /// `drvAsynIPPort`'s `readIt`: it calls `closeConnection` on a TCP EOF
    /// (:815-820) **and** on every errno that is not `EWOULDBLOCK`/`EINTR`
    /// (:797-806), and `closeConnection` destroys the socket and issues
    /// `exceptionDisconnect` (:206-217).
    ///
    /// That teardown is what frees the slot: the listener reuses a slot exactly
    /// when its child port reports `isConnected() == false`
    /// (drvAsynIPServerPort.c:344-350). The port tore down on EOF only, so a
    /// client whose socket died mid-read — `ECONNRESET`, `EPIPE`, a NAT drop —
    /// kept its slot forever, and once the table filled no client could
    /// reconnect.
    fn read_or_close(
        &self,
        buf: &mut [u8],
        timeout: Duration,
        device: &str,
    ) -> Result<usize, SlotFailure> {
        // C readRaw (drvAsynIPPort.c:736-740): reject maxchars == 0 before
        // touching the socket; an empty buffer would otherwise read Ok(0) and be
        // misclassified as a peer EOF, tearing down a healthy connection.
        if buf.is_empty() {
            return Err(SlotFailure::kept(maxchars_zero_error()));
        }
        let mut guard = self.stream.lock();
        let Some(stream) = guard.as_mut() else {
            return Err(SlotFailure::kept(AsynError::Status {
                status: AsynStatus::Disconnected,
                message: format!("{device} has no client"),
            }));
        };
        // C parity: the child `drvAsynIPPort`'s `readRaw` floors a zero request
        // timeout to a 1 ms poll (drvAsynIPPort.c:741-743) and re-applies it on
        // every read. `socket_poll_timeout` is the shared owner of that mapping;
        // setting it unconditionally (rather than skipping on `timeout == 0`)
        // keeps a poll request a 1 ms poll instead of blocking on the accept-time
        // timeout.
        // C readRaw (drvAsynIPPort.c:744-756): under USE_SOCKTIMEOUT a failed
        // setsockopt(SO_RCVTIMEO) records asynError but does NOT return — it
        // falls through to recv() (:791), and the recv outcome governs teardown
        // (:797-821). This is load-bearing on macOS: setsockopt(SO_RCVTIMEO)
        // returns EINVAL on a socket whose peer sent RST (Darwin marks the reset
        // socket invalid), so returning here — as this used to — skips the recv
        // that would see ECONNRESET, `clear()` the slot and issue the disconnect.
        // The slot then leaks on every abortive close, and once the table fills
        // no client can ever reconnect. Mirror C's fall-through: the setsockopt
        // failure only taints status — which C returns as asynError-with-bytes
        // for a >0-byte read (:822-831), an unreachable branch here since the
        // EINVAL cause is a reset socket whose read cannot succeed — so drop it
        // and let the read below classify the outcome and own the teardown.
        let _ = stream.set_read_timeout(Some(socket_poll_timeout(timeout)));
        let res = stream.read(buf);
        // `clear()` re-locks the stream, so the read guard must go first.
        drop(guard);
        match res {
            Ok(0) => {
                self.clear();
                Err(SlotFailure::closed(AsynError::Status {
                    status: AsynStatus::Disconnected,
                    message: format!("{device} peer closed"),
                }))
            }
            Ok(n) => Ok(n),
            Err(e) => Err(self.classify_read_error(e, device, timeout)),
        }
    }

    /// Write to this slot, applying the same rule to C's `writeIt`, which calls
    /// `closeConnection(pasynUser, tty, "Write error")` on a fatal errno
    /// (drvAsynIPPort.c:694-698) and reports `asynTimeout` — socket intact — for
    /// the `EWOULDBLOCK`/`EINTR` class it retries (:661-672). Single owner of "a
    /// client slot dies on a write", shared by the parent's addressed and
    /// broadcast writes and the child port's.
    fn write_or_close(&self, data: &[u8], device: &str) -> Result<(), SlotFailure> {
        let mut guard = self.stream.lock();
        let Some(stream) = guard.as_mut() else {
            return Err(SlotFailure::kept(AsynError::Status {
                status: AsynStatus::Disconnected,
                message: format!("{device} has no client"),
            }));
        };
        let res = stream.write_all(data).and_then(|()| stream.flush());
        drop(guard);
        match res {
            Ok(()) => Ok(()),
            Err(e) => Err(self.classify_io_error(e, device, "write")),
        }
    }

    /// C's `should_disconnect` for a failed *read* on a slot, both disjuncts
    /// (drvAsynIPPort.c:797-799):
    ///
    /// ```c
    /// int should_disconnect = (((tty->disconnectOnReadTimeout) && (pasynUser->timeout > 0)) ||
    ///                          ((SOCKERRNO != SOCK_EWOULDBLOCK) && (SOCKERRNO != SOCK_EINTR)));
    /// ```
    ///
    /// The second disjunct is the fatal-errno rule ([`is_nonfatal_read_timeout`],
    /// shared with the IP client port). The first is the per-child-port option
    /// `disconnectOnReadTimeout`: with it on, a read that merely *times out* tears
    /// the connection down — a live-looking socket whose peer has gone silent
    /// stops holding a slot the server needs. C's child here is a real
    /// `drvAsynIPPort` (drvAsynIPServerPort.c:690), so `asynSetOption("srv:0", 0,
    /// "disconnectOnReadTimeout", "Y")` reaches exactly this statement (W10-D5).
    ///
    /// A zero-timeout request is a *poll*, not a wait, and C exempts it
    /// (`pasynUser->timeout > 0`): expiring a 1 ms socket poll must not kill a
    /// healthy client.
    fn classify_read_error(
        &self,
        e: std::io::Error,
        device: &str,
        timeout: Duration,
    ) -> SlotFailure {
        if is_nonfatal_read_timeout(e.kind())
            && !(self.disconnect_on_read_timeout() && timeout > Duration::ZERO)
        {
            return SlotFailure::kept(AsynError::Status {
                status: AsynStatus::Timeout,
                message: "read timeout".to_string(),
            });
        }
        self.clear();
        SlotFailure::closed(AsynError::Status {
            status: AsynStatus::Error,
            message: format!("{device} read error: {e}"),
        })
    }

    /// C's `should_disconnect` for a failed *write* (drvAsynIPPort.c:694-698):
    /// the fatal-errno disjunct only — `writeIt` has no `disconnectOnReadTimeout`
    /// term, it retries `EWOULDBLOCK`/`EINTR` (:661-672) and reports
    /// `asynTimeout` with the socket intact. The branch that closes the socket is
    /// the branch that reports `asynError`.
    fn classify_io_error(&self, e: std::io::Error, device: &str, what: &str) -> SlotFailure {
        if is_nonfatal_read_timeout(e.kind()) {
            return SlotFailure::kept(AsynError::Status {
                status: AsynStatus::Timeout,
                message: format!("{what} timeout"),
            });
        }
        self.clear();
        SlotFailure::closed(AsynError::Status {
            status: AsynStatus::Error,
            message: format!("{device} {what} error: {e}"),
        })
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

/// The accept half of the server port, detached from the driver so the listener
/// thread can own it — C's `connectionListener` (drvAsynIPServerPort.c:290-384),
/// which `drvAsynIPServerPortConfigure` starts at configure time (:711-714) and
/// which is the *only* thing in C that accepts.
///
/// Everything an accept touches lives here, which is what lets the loop be the
/// single acceptor: the slots (shared with the child subports through their
/// connection cells), the announcement capability, and the parent's interrupt
/// source. Nothing else may call [`Self::accept_one`] — a second acceptor would
/// steal connections from the loop.
struct Acceptor {
    listener: TcpListener,
    slots: Vec<Arc<ClientSlot>>,
    read_timeout: Option<Duration>,
    max_clients: usize,
    port_name: String,
    announcer: ExceptionAnnouncer,
    /// The parent port's interrupt source, shared with the driver's own
    /// `base.interrupts`. C fires the octet callbacks that carry the new child
    /// port's *name* from inside the listener thread (:374-383) — that is how
    /// an IOC learns which child port a client landed on.
    interrupts: InterruptManager,
    trace: Option<Arc<TraceManager>>,
    /// Set to stop the loop. The listener is non-blocking and the loop polls it,
    /// so the flag is observed within one poll interval — the portable stand-in
    /// for C's `tty->fd = INVALID_SOCKET`, which unblocks its blocking `accept`
    /// (:326-331).
    shutdown: Arc<AtomicBool>,
}

impl Acceptor {
    /// Copy the parent's trace masks onto the child port that just took a
    /// connection — C `drvAsynIPServerPort.c:367-369`.
    fn seed_child_trace(&self, child: &str) {
        let Some(trace) = &self.trace else { return };
        trace.set_trace_mask(Some(child), trace.get_trace_mask(Some(&self.port_name)));
        trace.set_trace_io_mask(Some(child), trace.get_trace_io_mask(Some(&self.port_name)));
    }

    /// Accept one pending connection and assign it to a free slot. Returns the
    /// slot index used, or `None` when no connection is pending.
    ///
    /// C `connectionListener` (:326-383): accept, find the first disconnected
    /// child port, hand it the fd, and fire the octet interrupt callbacks with
    /// that child's port name. A connection arriving with every slot occupied is
    /// destroyed ("too many clients", :351-355) — dropping the stream here is
    /// that `epicsSocketDestroy`.
    fn accept_one(&self) -> AsynResult<Option<usize>> {
        let (stream, peer) = match self.listener.accept() {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => {
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("accept failed: {e}"),
                });
            }
        };
        // C's connectionListener accepts on a BLOCKING listener
        // (epicsSocketAccept, :326), so its child fds are blocking and
        // SO_RCVTIMEO governs every slot read. This listener is non-blocking
        // (the poll loop needs it), and macOS/BSD accepted sockets INHERIT
        // O_NONBLOCK from the listener (Linux resets it) — an inherited
        // non-blocking child turns every timed slot read into an instant
        // EWOULDBLOCK poll and SO_RCVTIMEO into a no-op. Restore blocking
        // explicitly so the child matches C on every platform.
        stream
            .set_nonblocking(false)
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("set_nonblocking(false) on accepted client failed: {e}"),
            })?;
        if let Some(t) = self.read_timeout {
            stream
                .set_read_timeout(Some(t))
                .map_err(|e| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("set_read_timeout failed: {e}"),
                })?;
        }
        // First-fit slot scan — C's "search for a port which is disconnected"
        // loop (:342-350).
        for (i, slot) in self.slots.iter().enumerate() {
            if !slot.is_occupied() {
                slot.assign(stream, peer);
                self.announcer.announce(AsynException::Connect, i as i32);
                let child = format!("{}:{}", self.port_name, i);
                // C :367-369 — "Set the new port to initially have the same
                // trace mask that we have". The connection is what makes the
                // child a live port, so the accept is where it inherits the
                // parent's tracing; `asynSetTraceMask SERVER -1 0x9` in an
                // st.cmd, before any client exists, must therefore reach the
                // client that later connects. Initially: a later change to the
                // parent does not follow (C copies once, here too).
                self.seed_child_trace(&child);
                asyn_trace!(
                    Some(self.trace),
                    &self.port_name,
                    TraceMask::FLOW,
                    "new connection from {peer} on {child}"
                );
                // C :374-383 — the octet callbacks carry the child port name,
                // not the payload. The listener walks the interrupt list and
                // calls EVERY node unconditionally: there is no addr test here
                // (unlike asynOctetBase.c:203-215, which does test addr). A
                // client landing in slot 3 must be announced to a listener that
                // registered on addr 0, because "which slot" is the news.
                self.interrupts.notify_octet(
                    OctetFanOut::EveryUser,
                    InterruptValue {
                        reason: 0,
                        addr: i as i32,
                        value: ParamValue::Octet(child),
                        timestamp: SystemTime::now(),
                        iface: Some(InterfaceType::Octet),
                        ..Default::default()
                    },
                );
                return Ok(Some(i));
            }
        }
        // Slot table full: C destroys the socket and keeps listening.
        drop(stream);
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: format!(
                "no free client slot (max_clients={}); dropped connection from {peer}",
                self.max_clients
            ),
        })
    }

    /// C `connectionListener`'s `while (tty->fd != INVALID_SOCKET)` loop
    /// (:308-384). An accept error is traced and the loop continues (:335-340);
    /// only teardown ends it.
    fn run(&self) {
        asyn_trace!(
            Some(self.trace),
            &self.port_name,
            TraceMask::FLOW,
            "started listening for connections on {}",
            self.port_name
        );
        while !self.shutdown.load(Ordering::SeqCst) {
            match self.accept_one() {
                Ok(Some(_)) => {}
                Ok(None) => std::thread::sleep(ACCEPT_POLL_INTERVAL),
                Err(e) => asyn_trace!(
                    Some(self.trace),
                    &self.port_name,
                    TraceMask::ERROR,
                    "accept error on {}: {}",
                    self.port_name,
                    e.message()
                ),
            }
        }
        asyn_trace!(
            Some(self.trace),
            &self.port_name,
            TraceMask::FLOW,
            "terminating connection thread for {}",
            self.port_name
        );
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

    // ----- TCP accept loop -----
    /// Stops the accept loop. Shared with the [`Acceptor`] the thread owns.
    accept_shutdown: Arc<AtomicBool>,
    /// The listener thread — C's `connectionListener`
    /// (drvAsynIPServerPort.c:711-714). Joined on disconnect/shutdown.
    accept_thread: Mutex<Option<JoinHandle<()>>>,
}

/// How long the accept loop waits between polls of a non-blocking listener.
/// Bounds both accept latency and how long teardown waits for the thread.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(10);

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
        base.init_connected(false);
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
            accept_shutdown: Arc::new(AtomicBool::new(false)),
            accept_thread: Mutex::new(None),
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
        self.start_accept_loop(&listener)?;
        *self.listener.lock() = Some(listener);
        self.base.set_connected(true);
        Ok(())
    }

    /// Start the listener thread. Called from the one place that binds the TCP
    /// listener, which is what makes "the socket is listening ⟹ something is
    /// accepting on it" true by construction — in C the two are equally
    /// inseparable, `drvAsynIPServerPortConfigure` creating the socket and
    /// starting `connectionListener` in the same breath
    /// (drvAsynIPServerPort.c:640-714). The port used to bind and then never
    /// accept: a client sat in the backlog until it timed out while the port
    /// reported Connected.
    fn start_accept_loop(&mut self, listener: &TcpListener) -> AsynResult<()> {
        // NOT RTEMS-SAFE if this crate is ever built for RTEMS: `try_clone`
        // is `fcntl(F_DUPFD*)`, which fails on any libbsd socket — see
        // `epics-ca-rs/src/server/blocking.rs::handle_client_blocking`. The
        // fix there is `Arc<TcpStream>` + `impl Read/Write for &TcpStream`.
        let accept_listener = listener.try_clone().map_err(|e| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("listener try_clone failed: {e}"),
        })?;
        accept_listener
            .set_nonblocking(true)
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("set_nonblocking failed: {e}"),
            })?;

        self.accept_shutdown.store(false, Ordering::SeqCst);
        let acceptor = Acceptor {
            listener: accept_listener,
            slots: self.slots.clone(),
            read_timeout: self.config.read_timeout,
            max_clients: self.config.max_clients,
            port_name: self.base.port_name.clone(),
            announcer: self.base.exception_announcer(),
            interrupts: InterruptManager::from_shared_state(self.base.interrupts.shared_state()),
            trace: self.base.trace.clone(),
            shutdown: Arc::clone(&self.accept_shutdown),
        };
        let port_name = self.base.port_name.clone();
        let handle = std::thread::Builder::new()
            .name(format!("ipserver-accept-{port_name}"))
            .spawn(move || acceptor.run())
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("accept thread spawn failed: {e}"),
            })?;
        *self.accept_thread.lock() = Some(handle);
        Ok(())
    }

    /// Stop the listener thread and wait for it. Every teardown path runs this —
    /// C registers `ttyCleanup` with `epicsAtExit` for the same reason (:717).
    fn stop_accept_loop(&mut self) {
        self.accept_shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.accept_thread.lock().take() {
            let _ = handle.join();
        }
    }

    /// UDP-mode bind: open the datagram socket, spawn the recv
    /// worker. Mirrors C asyn's `connectIt` SOCK_DGRAM branch
    /// (drvAsynIPServerPort.c lines ~440-470).
    fn open_udp_listener(&mut self) -> AsynResult<()> {
        let addr = self.resolve_bind_addr()?;
        // C enables datagram fanout on the UDP server socket
        // (drvAsynIPServerPort.c:426-429): for SOCK_DGRAM it calls
        // `epicsSocketEnableAddressUseForDatagramFanout`, which sets
        // SO_REUSEPORT (where available) followed by SO_REUSEADDR — so
        // multiple IOCs can bind the same UDP port and the kernel fans
        // each datagram out to them. The TCP listener gets only
        // SO_REUSEADDR (:430); the fanout helper is SOCK_DGRAM-only. That
        // pairing is `SocketOptions::FANOUT`, and the seam is what keeps the
        // options ahead of the bind on both hosted and embedded targets.
        let socket = socket::udp_socket(addr, socket::SocketOptions::FANOUT).map_err(|e| {
            AsynError::Status {
                status: AsynStatus::Error,
                message: format!("UDP bind '{addr}' failed: {e}"),
            }
        })?;
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
        // The worker fires this port's octet interrupts (C :312-321), so it
        // needs the same interrupt source the driver publishes on.
        let interrupts_t = InterruptManager::from_shared_state(self.base.interrupts.shared_state());
        let handle = std::thread::Builder::new()
            .name(format!("udp-server-{port_name}"))
            .spawn(move || udp_recv_loop(socket_t, cache_t, shutdown_t, port_name, interrupts_t))
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
        // Unconditional SO_REUSEADDR and no SO_REUSEPORT
        // (drvAsynIPServerPort.c:430) — that is `SocketOptions::REUSE_ADDRESS`.
        //
        // Backlog independent of `max_clients` — the slot cap bounds
        // *concurrent* accepted clients, not the kernel's pending-
        // connection queue. A small backlog (= max_clients) caused
        // third-party connect() to block in tests when 2 prior
        // connections were already queued. 128 mirrors the typical
        // SOMAXCONN on Linux/macOS while staying portable.
        socket::tcp_listener(addr, socket::SocketOptions::REUSE_ADDRESS, 128).map_err(|e| {
            AsynError::Status {
                status: AsynStatus::Error,
                message: format!("bind '{addr}' failed: {e}"),
            }
        })
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
        Ok(DrvAsynIPSubport::new(
            name,
            Arc::clone(&self.slots[idx]),
            self.config.no_process_eos,
        ))
    }
}

impl PortDriver for DrvAsynIPServerPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    /// C drvAsynIPServerPort registers asynCommon and asynOctet, plus asynInt32
    /// only when the socket is not SOCK_DGRAM (drvAsynIPServerPort.c:621-661) —
    /// the Int32 interface carries the per-connection file descriptor. It
    /// registers no asynOption.
    fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
        use crate::interfaces::Capability::*;
        let mut caps = vec![OctetRead, OctetWrite, Flush, Connect];
        if self.config.protocol == IpServerProtocol::Tcp {
            caps.push(Int32Read);
            caps.push(Int32Write);
        }
        caps
    }

    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        let already_up = self.base.is_connected()
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
        self.stop_accept_loop();
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
        self.stop_accept_loop();
        self.stop_udp_worker();
        *self.udp_socket.lock() = None;
        self.udp_cache.lock().clear();
        *self.listener.lock() = None;
        Ok(())
    }

    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        if self.config.protocol == IpServerProtocol::Udp {
            // C readIt (drvAsynIPServerPort.c:180-184) rejects maxchars == 0
            // with asynError at the top of the UDP server read, before
            // draining the datagram cache (this is also what shields C from
            // its own (int)maxchars-1 underflow at :196). The TCP server read
            // floors maxchars in base_read_octet; the UDP path bypasses it, so
            // guard it here too — uniform with the TCP and client reads.
            if buf.is_empty() {
                return Err(maxchars_zero_error());
            }
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
            // C readIt (drvAsynIPServerPort.c:180-184) rejects maxchars == 0
            // before draining the datagram cache; mirror it on this entry too
            // (see read_octet above for the rationale).
            if buf.is_empty() {
                return Err(maxchars_zero_error());
            }
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
                if let Err(f) = self.write_to_slot(slot, i as i32, data) {
                    tracing::debug!(
                        target: "asyn_rs::ip_server_port",
                        addr = i,
                        error = %f.error,
                        "broadcast write to slot failed"
                    );
                    // The slot is torn down by `write_or_close` on exactly C's
                    // fatal-errno branch (drvAsynIPPort.c:694-698) — and *only*
                    // there: a send timeout is C's `asynTimeout` with the socket
                    // intact (:661-672), so a slow peer no longer loses its slot.
                    if f.closed {
                        self.base
                            .announce_exception(AsynException::Connect, i as i32);
                    }
                }
            }
            return Ok(data.len());
        }
        let arc = self.slot_arc(user.addr)?;
        match self.write_to_slot(&arc, user.addr, data) {
            Ok(()) => Ok(data.len()),
            Err(f) => Err(self.finish_slot_failure(user.addr, f)),
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
        let device = self.slot_device_name(user.addr);
        match arc.read_or_close(buf, user.timeout, &device) {
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
            Err(f) => Err(self.finish_slot_failure(user.addr, f)),
        }
    }

    /// The disconnect fan-out the parent owes when [`ClientSlot`] closed a slot —
    /// C's `exceptionDisconnect` on the child port (drvAsynIPPort.c:216), which
    /// is what makes the listener's `isConnected` scan see the slot as free
    /// (drvAsynIPServerPort.c:344-350). The socket is already gone; this is the
    /// announcement half only.
    fn finish_slot_failure(&mut self, addr: i32, failure: SlotFailure) -> AsynError {
        if failure.closed {
            self.base.announce_exception(AsynException::Connect, addr);
        }
        failure.error
    }

    /// C `tty->IPDeviceName` for a client slot — the name that goes into
    /// `"%s read error: %s"` (drvAsynIPPort.c:801-803). C's child ports are named
    /// `parent:N` (drvAsynIPServerPort.c:686).
    fn slot_device_name(&self, addr: i32) -> String {
        format!("{}:{}", self.base.port_name, addr)
    }

    fn write_to_slot(&self, slot: &ClientSlot, addr: i32, data: &[u8]) -> Result<(), SlotFailure> {
        slot.write_or_close(data, &self.slot_device_name(addr))
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
    interrupts: InterruptManager,
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
                {
                    let mut c = cache.lock();
                    c.data.clear();
                    c.data.extend_from_slice(&buf[..n]);
                    c.pos = 0;
                }
                // C :312-321 — every registered octet callback gets the
                // datagram, unconditionally (no addr test, exactly as for the
                // TCP announcement at :372-383). A datagram is a whole message,
                // so C passes ASYN_EOM_END; InterruptValue carries no eom
                // because no consumer reads one (C's own octet consumer,
                // devAsynOctet.c:476-478, ignores the eomReason argument).
                //
                // Fired with the cache lock released: a subscriber's callback
                // runs inline here, and a callback that reads the port back
                // would otherwise deadlock against the lock we still held.
                interrupts.notify_octet(
                    OctetFanOut::EveryUser,
                    InterruptValue {
                        reason: 0,
                        addr: 0,
                        value: ParamValue::Octet(String::from_utf8_lossy(&buf[..n]).into_owned()),
                        timestamp: SystemTime::now(),
                        iface: Some(InterfaceType::Octet),
                        ..Default::default()
                    },
                );
            }
            // DRV-56: a non-fatal recv error (200 ms read-timeout wake, or an
            // EINTR signal interruption) must NOT exit the worker — loop and
            // re-check shutdown. C's UDP worker (drvAsynIPServerPort.c:308-323)
            // assigns `UDPbufferSize = recvfrom(...)` with no error check, so
            // on EINTR it stores -1 and silently stops receiving; that is a C
            // bug, not a contract — recover here by retrying instead of copying
            // it. is_nonfatal_read_timeout is the shared owner of the
            // EINTR/WouldBlock/TimedOut "retry, not fatal" set.
            Err(e) if is_nonfatal_read_timeout(e.kind()) => {
                continue;
            }
            Err(e) => {
                // A genuine hard recv error: reception is dead either way (C
                // would 1 ms-busy-spin a worker that never recvfrom's again
                // once UDPbufferSize is stuck at -1). Cleanly exit the thread
                // instead of spinning.
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
    fn new(port_name: String, slot: Arc<ClientSlot>, no_process_eos: bool) -> Self {
        let mut base = PortDriverBase::new(
            &port_name,
            1,
            PortFlags {
                multi_device: false,
                can_block: true,
                destructible: true,
            },
        );
        // The child does not own its link — the slot does, and the parent's accept
        // loop assigns and clears it without this port's actor ever running. Bind
        // `connected` to the slot's own cell rather than caching a copy of it, so
        // a reused slot cannot leave the child stuck at `asynDisconnected`
        // (R13-50). C keeps the same two facts in agreement by having the listener
        // call `connectDevice` on the child the moment it hands it the socket
        // (drvAsynIPServerPort.c:357-367).
        base.share_connection(slot.connection_cell());
        // C's child IS a `drvAsynIPPortConfigure`d port — the listener creates it
        // with exactly that call (drvAsynIPServerPort.c:688-694), passing
        // noAutoConnect=1 and the server's own noProcessEos. So it gets the
        // configure-time shape through the same owner the client port does:
        // interruptProcess=1 (so an I/O-Intr record on `<parent>:<N>` processes)
        // and, unless suppressed, the EOS interpose (so a `\n`-terminated line
        // from a dialled-in device actually terminates a read). Building the
        // child's base by hand is how it came to have neither (R19-107, R19-108).
        DrvAsynIPPort::apply_ip_port_configure(
            &mut base,
            /* noAutoConnect */ true,
            no_process_eos,
        );
        Self { base, slot }
    }

    /// Peer address currently bound to this subport's slot, if any.
    pub fn peer(&self) -> Option<SocketAddr> {
        self.slot.peer_addr()
    }

    /// The disconnect fan-out this child port owes when [`ClientSlot`] closed its
    /// slot — C `closeConnection`'s `exceptionDisconnect` (drvAsynIPPort.c:216),
    /// which is exactly what makes the listener see the slot as reusable
    /// (drvAsynIPServerPort.c:344-350). The socket is already gone.
    ///
    /// Per-addr Connect carries addr=0 (this is the subport's single device
    /// slot); the port-level `set_connected(false)` fires the port-level
    /// transition once thanks to its edge guard. Both fan-outs are needed because
    /// observers can listen at either granularity.
    fn finish_slot_failure(&mut self, failure: SlotFailure) -> AsynError {
        if failure.closed {
            self.base.announce_exception(AsynException::Connect, 0);
            self.base.set_connected(false);
        }
        failure.error
    }
}

impl PortDriver for DrvAsynIPSubport {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    /// C creates each accepted connection as a plain drvAsynIPPort
    /// (`drvAsynIPPortConfigure`, drvAsynIPServerPort.c:690) — so a subport has
    /// the byte-transport interface set: asynCommon + asynOption + asynOctet.
    fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
        crate::interfaces::octet_transport_capabilities()
    }

    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        // The child has no link of its own to dial: its socket is the slot's, and
        // the parent's accept loop is what puts one there. C's child `connectIt`
        // is likewise driven by the listener (`connectDevice`,
        // drvAsynIPServerPort.c:357-367) and does no dialling either. So this
        // publishes the slot's state — it cannot change it.
        self.base.sync_connection_edge();
        if !self.base.is_connected() {
            return Err(AsynError::Status {
                status: AsynStatus::Disconnected,
                message: "no client assigned to this subport slot yet".into(),
            });
        }
        Ok(())
    }

    /// C's child port is a full `drvAsynIPPort` (drvAsynIPServerPort.c:690), so
    /// `asynSetOption("<parent>:<n>", 0, "disconnectOnReadTimeout", "Y")` lands in
    /// `drvAsynIPPort::setOption` (:924-935), which accepts only `"Y"`/`"N"`
    /// (case-insensitive) and errors on anything else. The option then arms the
    /// first disjunct of `readIt`'s `should_disconnect` (:797-799) — see
    /// [`ClientSlot::classify_read_error`].
    ///
    /// The other key C's `setOption` takes, `hostInfo`, re-dials an outbound
    /// connection; an accepted socket has nothing to re-dial, so it is not
    /// modelled here.
    fn set_option(&mut self, _user: &mut AsynUser, key: &str, value: &str) -> AsynResult<()> {
        if key.eq_ignore_ascii_case("disconnectOnReadTimeout") {
            let yes = parse_yn_option("disconnectOnReadTimeout", value)?;
            self.slot.set_disconnect_on_read_timeout(yes);
            return Ok(());
        }
        Err(AsynError::OptionNotFound(key.to_string()))
    }

    fn get_option(&self, key: &str) -> AsynResult<String> {
        if key.eq_ignore_ascii_case("disconnectOnReadTimeout") {
            // C prints the flag as "Y"/"N" (drvAsynIPPort.c:906-910).
            return Ok(if self.slot.disconnect_on_read_timeout() {
                "Y".to_string()
            } else {
                "N".to_string()
            });
        }
        Err(AsynError::OptionNotFound(key.to_string()))
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
        let device = self.base.port_name.clone();
        match self.slot.read_or_close(buf, user.timeout, &device) {
            Ok(n) => Ok(n),
            Err(f) => Err(self.finish_slot_failure(f)),
        }
    }

    fn write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let device = self.base.port_name.clone();
        match self.slot.write_or_close(data, &device) {
            Ok(()) => Ok(data.len()),
            Err(f) => Err(self.finish_slot_failure(f)),
        }
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

    /// The listener thread is the only acceptor — C `connectionListener`
    /// (drvAsynIPServerPort.c:290-384) — so a test observes an accept by waiting
    /// for the slot to fill, not by driving the accept itself. Returns `idx`.
    fn wait_for_slot(srv: &DrvAsynIPServerPort, idx: usize) -> usize {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while srv.peer(idx as i32).is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "slot {idx} never filled — the accept loop is not running"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        idx
    }

    /// Re-drive slot reads until the peer's abortive close (RST) becomes
    /// **visible** to the server read, then return the teardown failure. Each
    /// `step` performs one read and reports whether the slot is now torn down
    /// (occupied cell cleared / child disconnected — the two are the same shared
    /// cell).
    ///
    /// The retry is for *RST visibility*, not for teardown timing:
    ///
    /// * The RST from `SO_LINGER(0)` + `close()` is not always visible on the
    ///   server's first read — on macOS/BSD it can take an extra poll interval —
    ///   so a `read()` may first return a retryable `asynTimeout` (C's caller
    ///   likewise re-drives `readIt` rather than treating one `EWOULDBLOCK` as a
    ///   disconnect, drvAsynIPPort.c:807-812). Never accept that `Timeout` as
    ///   teardown; keep polling.
    /// * The close does not surface as the *same* status on every platform.
    ///   C runs `closeConnection` — which frees the slot (`exceptionDisconnect`,
    ///   :216 → drvAsynIPServerPort.c:344-350) — on BOTH a fatal errno
    ///   (`asynError`, :805) and a stream EOF `recv()==0` (`asynSuccess`+END,
    ///   :819). Linux delivers an abortive close as `ECONNRESET` (the port's
    ///   `asynError`); macOS/BSD can deliver the very same close as a plain
    ///   `recv()==0` EOF (the port's `asynDisconnected`). So accept either.
    ///
    /// Once the RST surfaces the read returns a fatal status, and by then
    /// `read_or_close` has already `clear()`ed the slot on *this* thread — inside
    /// `classify_read_error`, before `read_octet` returns — so the freed slot is
    /// observed together with the status on every platform. The helper asserts
    /// exactly that. (This is what the production fall-through fix restored: before
    /// it, macOS `setsockopt(SO_RCVTIMEO)` failed `EINVAL` on the reset socket and
    /// `read_or_close` returned early on every read, so the RST was never `recv`ed
    /// and the slot never tore down — this loop then spun to its deadline.)
    ///
    /// The bounded deadline means a slot whose RST never surfaces still fails
    /// loudly rather than hanging.
    fn read_until_slot_torn_down(mut step: impl FnMut() -> (AsynResult<usize>, bool)) -> AsynError {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let (res, torn_down) = step();
            match res {
                Ok(n) => panic!("a reset peer cannot deliver {n} bytes"),
                // A retryable asynTimeout means the RST is not yet visible and the
                // socket is still intact (C readRaw :807-811). Never teardown.
                Err(e) if e.status() == AsynStatus::Timeout => {}
                // Any other status is C's teardown — fatal-errno asynError, or a
                // recv()==0 EOF asynDisconnected. `read_or_close` clears the slot
                // on this thread before returning, so it MUST be torn down now.
                Err(e) => {
                    assert!(
                        torn_down,
                        "the read returned teardown status {e:?} but the slot is \
                         still occupied — read_or_close must clear() before returning"
                    );
                    return e;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the peer's abortive close never surfaced to the server read"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

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
            no_process_eos: false,
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
            no_process_eos: false,
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
        let idx = wait_for_slot(&srv, 0);
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

    /// R11-C9: a client whose socket dies mid-read must lose its slot.
    ///
    /// C serves each accepted connection with a full `drvAsynIPPort` child
    /// (drvAsynIPServerPort.c:681-708), whose `readIt` calls `closeConnection` on
    /// *any* errno outside `EWOULDBLOCK`/`EINTR` (drvAsynIPPort.c:797-806), not
    /// just on EOF — and `closeConnection`'s `exceptionDisconnect` (:216) is
    /// exactly what makes the listener's `isConnected` scan hand the slot to the
    /// next client (drvAsynIPServerPort.c:344-350).
    ///
    /// The port tore down on EOF only, so a peer that vanished with an RST
    /// (`ECONNRESET` — a crashed client, a NAT drop) kept its slot forever, and
    /// once the table filled no client could reconnect.
    #[test]
    fn a_fatal_read_error_frees_the_client_slot() {
        use socket2::{Domain, Socket, Type};

        // max_clients = 1: the slot is the whole table, so "the slot was freed"
        // and "the next client can connect" are the same assertion.
        let mut config = IpServerConfig::parse("127.0.0.1:0 TCP").unwrap();
        config.max_clients = 1;
        let mut srv = DrvAsynIPServerPort::with_config("rst_frees_slot", config).unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let port = srv.local_port();

        // A client that dies with an RST rather than a FIN: SO_LINGER(0) makes
        // close() send RST, so the server's next read gets ECONNRESET — a fatal
        // errno, not the EOF the port already handled.
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let client = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        client.connect(&addr.into()).unwrap();
        let idx = wait_for_slot(&srv, 0);
        assert_eq!(idx, 0);
        assert!(srv.slots[0].is_occupied(), "the client owns the only slot");

        client.set_linger(Some(Duration::ZERO)).unwrap();
        drop(client); // RST
        std::thread::sleep(Duration::from_millis(50));

        let read_user = AsynUser::default()
            .with_addr(0)
            .with_timeout(Duration::from_millis(200));
        let mut buf = [0u8; 64];
        // Re-drive the read past a not-yet-visible RST until the abortive close
        // surfaces. Each step reads then reports whether the slot is now free. The
        // helper accepts C's fatal-errno asynError (Linux ECONNRESET) or recv==0
        // EOF asynDisconnected (macOS), never a retryable Timeout, and asserts the
        // slot is torn down in the same step the fatal status appears (read_or_close
        // clears it on this thread before returning).
        let err = read_until_slot_torn_down(|| {
            let res = srv.read_octet(&read_user, &mut buf);
            (res, !srv.slots[0].is_occupied())
        });
        assert!(
            matches!(err.status(), AsynStatus::Error | AsynStatus::Disconnected),
            "an abortive close is C's fatal-errno asynError or a recv==0 EOF, got {err:?}"
        );
        // Guaranteed by the helper's exit condition; kept as an explicit statement
        // of the C invariant (closeConnection frees the slot, not just on EOF).
        assert!(
            !srv.slots[0].is_occupied(),
            "C's readIt closeConnection frees the slot on a fatal errno, not just on EOF"
        );

        // …and the slot really is reusable: a second client takes it.
        let _client2 = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        wait_for_slot(&srv, 0);

        srv.disconnect(&AsynUser::default()).unwrap();
    }

    /// W10-D5: the child port honours `disconnectOnReadTimeout`.
    ///
    /// C serves each accepted connection with a full `drvAsynIPPort`
    /// (drvAsynIPServerPort.c:690), so `asynSetOption("<parent>:<n>", 0,
    /// "disconnectOnReadTimeout", "Y")` reaches `drvAsynIPPort::setOption`
    /// (:924-935) and arms the *first* disjunct of `readIt`'s `should_disconnect`
    /// (:797-799): a read that merely times out closes the connection. The port
    /// modelled only the second (fatal-errno) disjunct, so a silent peer held its
    /// slot forever and the server could not reclaim it.
    ///
    /// C's `pasynUser->timeout > 0` term is checked too: a zero-timeout *poll*
    /// must not kill a healthy client.
    #[test]
    fn the_child_port_honours_disconnect_on_read_timeout() {
        let mut config = IpServerConfig::parse("127.0.0.1:0 TCP").unwrap();
        config.max_clients = 1;
        let mut srv = DrvAsynIPServerPort::with_config("srv_drto", config).unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let port = srv.local_port();
        let mut child = srv.make_subport(0).unwrap();

        // Default is off (C `tty->disconnectOnReadTimeout` is 0 unless set): a
        // silent peer times out and keeps its slot.
        let _client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        wait_for_slot(&srv, 0);
        let user = AsynUser::default().with_timeout(Duration::from_millis(50));
        let mut buf = [0u8; 64];
        let err = child.read_octet(&user, &mut buf).unwrap_err();
        assert_eq!(err.status(), AsynStatus::Timeout, "got {err:?}");
        assert!(
            srv.slots[0].is_occupied(),
            "with the option off a read timeout leaves the socket intact (C :810-811)"
        );

        // asynSetOption("srv_drto:0", 0, "disconnectOnReadTimeout", "Y").
        let mut opt_user = AsynUser::default();
        child
            .set_option(&mut opt_user, "disconnectOnReadTimeout", "Y")
            .unwrap();
        assert_eq!(child.get_option("disconnectOnReadTimeout").unwrap(), "Y");

        // A zero-timeout poll is exempt (C's `pasynUser->timeout > 0`).
        let poll_user = AsynUser::default().with_timeout(Duration::ZERO);
        let err = child.read_octet(&poll_user, &mut buf).unwrap_err();
        assert_eq!(err.status(), AsynStatus::Timeout, "got {err:?}");
        assert!(
            srv.slots[0].is_occupied(),
            "a zero-timeout poll is not a read timeout C tears down on (:797)"
        );

        // …but a real timed read now closes the connection, and C reports that
        // branch as asynError, never as a retryable asynTimeout (:801-805).
        let err = child.read_octet(&user, &mut buf).unwrap_err();
        assert_eq!(err.status(), AsynStatus::Error, "got {err:?}");
        assert!(
            !srv.slots[0].is_occupied(),
            "disconnectOnReadTimeout=Y frees the slot on a read timeout \
             (drvAsynIPPort.c:797-799, first disjunct)"
        );
        assert!(!child.base().is_connected());

        // The freed slot takes the next client (the point of the option).
        let _client2 = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        wait_for_slot(&srv, 0);
        assert!(child.base().is_connected());
        // …and the option survives the slot reuse, as C's child port does.
        assert_eq!(child.get_option("disconnectOnReadTimeout").unwrap(), "Y");

        srv.disconnect(&AsynUser::default()).unwrap();
    }

    /// R13-50: a reused slot must revive the child subport.
    ///
    /// C's listener does it explicitly: when the free-slot scan hands slot `i` to
    /// a new client it sets `pl->fd = clientFd` and calls
    /// `pasynCommonSyncIO->connectDevice(pl->pasynUser)` on the child port
    /// (drvAsynIPServerPort.c:357-367), which drives the child's `connectIt` and
    /// its `exceptionConnect`. It has to: the child's own `isConnected` is what
    /// the *next* free-slot scan reads (:342-350), so C cannot leave a child
    /// disconnected while its slot holds a live socket.
    ///
    /// The port cached the child's `connected` in its own `PortDriverBase`, which
    /// only the child's actor could write and which the parent's accept loop never
    /// reached. Any teardown — EOF, and after R11-C9 any fatal errno — latched it
    /// false, and since the child is `noAutoConnect` nothing ever set it back: the
    /// next client accepted into that slot was served by a port that refused every
    /// read and write with `asynDisconnected`, forever, over a live socket.
    #[test]
    fn a_reused_client_slot_revives_the_child_subport() {
        use socket2::{Domain, Socket, Type};

        let mut config = IpServerConfig::parse("127.0.0.1:0 TCP").unwrap();
        config.max_clients = 1;
        let mut srv = DrvAsynIPServerPort::with_config("slot_revive", config).unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let port = srv.local_port();
        let mut child = srv.make_subport(0).unwrap();

        // First client, killed with an RST so the read tears the slot down through
        // the fatal-errno path (C drvAsynIPPort.c:797-806).
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let victim = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        victim.connect(&addr.into()).unwrap();
        wait_for_slot(&srv, 0);
        // An explicit connect on the child for the *first* client — C's
        // `connectDevice`, and the only route by which a cached flag could ever
        // have become true. Handing it to the first client isolates the defect to
        // the *reuse*: the second client gets no extra call here, exactly as in C,
        // where the listener's own `connectDevice` on assignment is all there is.
        child.connect(&AsynUser::default()).unwrap();
        assert!(
            child.base().is_connected(),
            "the child serves the new client"
        );

        victim.set_linger(Some(Duration::ZERO)).unwrap();
        drop(victim); // RST
        std::thread::sleep(Duration::from_millis(50));

        let read_user = AsynUser::default().with_timeout(Duration::from_millis(200));
        let mut buf = [0u8; 64];
        // Same abortive-close portability as `a_fatal_read_error_frees_the_client_slot`:
        // re-drive the read past a not-yet-visible RST until the close surfaces
        // (the child's `is_connected` is the slot's own shared cell), and accept
        // either C teardown status (fatal-errno asynError on Linux, recv==0 EOF
        // asynDisconnected on macOS) — never a retryable Timeout.
        let err = read_until_slot_torn_down(|| {
            let res = child.read_octet(&read_user, &mut buf);
            (res, !child.base().is_connected())
        });
        assert!(
            matches!(err.status(), AsynStatus::Error | AsynStatus::Disconnected),
            "an abortive close is C's fatal-errno asynError or a recv==0 EOF, got {err:?}"
        );
        // Guaranteed by the helper's exit condition; kept as an explicit statement
        // of the C invariant (closeConnection + exceptionDisconnect on the child).
        assert!(
            !child.base().is_connected(),
            "the child's own read tore the slot down (C closeConnection + exceptionDisconnect)"
        );
        assert!(!srv.slots[0].is_occupied());

        // The next client takes the freed slot. C connects the child on assignment,
        // so the child must serve it — the socket is live and the port must say so.
        let mut client2 = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        wait_for_slot(&srv, 0);
        assert!(
            child.base().is_connected(),
            "a reused slot revives the child: C calls connectDevice on it \
             (drvAsynIPServerPort.c:357-367)"
        );

        // …and the revival is real I/O, not just a flag: the child must talk to the
        // new client over the slot's socket.
        let mut write_user = AsynUser::default();
        child
            .write_octet(&mut write_user, b"hello\n")
            .expect("the revived child must write to the new client's socket");
        let mut got = [0u8; 6];
        client2
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        client2.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello\n");

        client2.write_all(b"world\n").unwrap();
        let n = child
            .read_octet(&read_user, &mut buf)
            .expect("the revived child must read from the new client's socket");
        assert_eq!(&buf[..n], b"world\n");

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

    /// The canonical `drvAsynIPServerPort` use, end to end: a device dials into
    /// the IOC and sends `\n`-terminated lines. C's child port is a real
    /// `drvAsynIPPort` with the EOS interpose, so the read terminates on the
    /// terminator and fans the raw chunk out to the port's I/O-Intr users.
    ///
    /// Before the child went through `apply_ip_port_configure` it had an empty
    /// interpose chain and `octet_interrupt_process == false`: the read ran to
    /// the buffer bound (here: it would have returned "line1\nline2\n" in one
    /// go), and no interrupt user ever heard from it.
    #[test]
    fn a_child_port_terminates_a_read_on_the_terminator_and_fans_it_out() {
        use crate::interrupt::{InterruptFilter, InterruptValue};
        use std::io::Write as _;
        use std::net::TcpStream as ClientStream;
        use std::sync::Mutex as StdMutex;

        let mut srv = DrvAsynIPServerPort::new("srv_eos", "127.0.0.1:0").unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let port = srv.local_port();

        let mut client = ClientStream::connect(("127.0.0.1", port)).unwrap();
        let idx = wait_for_slot(&srv, 0);
        let mut sub = srv.make_subport(idx).unwrap();
        sub.connect(&AsynUser::default()).unwrap();

        let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let seen_cb = seen.clone();
        let _sub_cb = sub.base().interrupts.register_sync_callback(
            InterruptFilter::default(),
            move |iv: &InterruptValue| {
                if let ParamValue::Octet(s) = &iv.value {
                    seen_cb.lock().unwrap().push(s.clone());
                }
            },
        );

        // The IEOS an st.cmd sets on the child port: `asynOctetSetInputEos
        // srv_eos:0 0 "\n"`.
        sub.set_input_eos(&AsynUser::default(), b"\n").unwrap();

        client.write_all(b"line1\nline2\n").unwrap();
        client.flush().unwrap();
        std::thread::sleep(Duration::from_millis(50));

        let user = AsynUser::default().with_timeout(Duration::from_millis(500));
        let mut buf = [0u8; 64];
        // First read stops at the terminator — it does NOT run to the buffer
        // bound and swallow line2.
        let (n, eom) = crate::port::octet_read_chain(&mut sub, &user, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"line1");
        assert!(eom.contains(EomReason::EOS), "eom = {eom:?}");
        // Second read returns the buffered second line, no further socket read.
        let (n, eom) = crate::port::octet_read_chain(&mut sub, &user, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"line2");
        assert!(eom.contains(EomReason::EOS), "eom = {eom:?}");

        // The interrupt user saw the RAW chunk the socket delivered, terminators
        // included — C's octetBase fan-out sits below the EOS layer.
        let got = seen.lock().unwrap().concat();
        assert_eq!(got, "line1\nline2\n", "raw chunks fanned out, got {got:?}");

        srv.disconnect(&AsynUser::default()).unwrap();
    }

    /// R19-109 boundary: the new-connection announcement reaches EVERY octet
    /// interrupt user, whatever addr it registered on.
    ///
    /// C `connectionListener` (drvAsynIPServerPort.c:372-383) walks the octet
    /// interrupt list and calls every node with the child port's name — there
    /// is no addr test, unlike `asynOctetBase.c:203-215`. The boundary is
    /// "announcement addr == subscriber addr" (slot 0) vs "announcement addr !=
    /// subscriber addr" (slot 1): both must be delivered. An addr filter here
    /// would silently hide every client after the first.
    #[test]
    fn every_octet_user_hears_a_new_connection_whatever_slot_it_lands_in() {
        use crate::interrupt::{InterruptFilter, InterruptValue};
        use std::net::TcpStream as ClientStream;
        use std::sync::Mutex as StdMutex;

        let mut srv = DrvAsynIPServerPort::with_config(
            "srv_announce",
            IpServerConfig {
                max_clients: 2,
                ..IpServerConfig::parse("127.0.0.1:0").unwrap()
            },
        )
        .unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let port = srv.local_port();

        let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let seen_cb = seen.clone();
        // A device-support user bound to addr 0 — C registers on the SERVER
        // port, and the payload it wants is which child port to talk to.
        let _cb = srv.base().interrupts.register_sync_callback(
            InterruptFilter {
                addr: Some(0),
                ..InterruptFilter::default()
            },
            move |iv: &InterruptValue| {
                if let ParamValue::Octet(s) = &iv.value {
                    seen_cb.lock().unwrap().push(s.clone());
                }
            },
        );

        let _c0 = ClientStream::connect(("127.0.0.1", port)).unwrap();
        wait_for_slot(&srv, 0);
        let _c1 = ClientStream::connect(("127.0.0.1", port)).unwrap();
        wait_for_slot(&srv, 1);

        let got = seen.lock().unwrap().clone();
        assert_eq!(
            got,
            vec!["srv_announce:0".to_string(), "srv_announce:1".to_string()],
            "the addr-0 user must hear about the slot-1 client too"
        );

        srv.disconnect(&AsynUser::default()).unwrap();
    }

    /// R19-121 boundary: a child port inherits the parent's trace masks at the
    /// accept, and only at the accept.
    ///
    /// C `drvAsynIPServerPort.c:367-369` copies both masks onto the child when
    /// the connection lands — *"Set the new port to initially have the same
    /// trace mask that we have"*. The two boundaries: a mask set on the parent
    /// BEFORE the client connects (the st.cmd case: `asynSetTraceMask SERVER -1
    /// 0x9`) must reach the child; a mask set AFTER it connects must not (C
    /// copies once, it does not track).
    #[test]
    fn a_child_port_inherits_the_parents_trace_masks_at_the_accept() {
        use crate::services::PortServices;
        use crate::trace::TraceIoMask;
        use std::net::TcpStream as ClientStream;

        let trace = Arc::new(TraceManager::new());
        let services = PortServices::new(trace.clone());
        let mut srv = DrvAsynIPServerPort::new("srv_trace", "127.0.0.1:0").unwrap();
        services.bind(srv.base_mut());

        // st.cmd, before any client exists.
        let parent_mask = TraceMask::ERROR | TraceMask::FLOW | TraceMask::IO_DRIVER;
        let parent_io = TraceIoMask::ASCII | TraceIoMask::HEX;
        trace.set_trace_mask(Some("srv_trace"), parent_mask);
        trace.set_trace_io_mask(Some("srv_trace"), parent_io);

        srv.connect(&AsynUser::default()).unwrap();
        let port = srv.local_port();
        let _client = ClientStream::connect(("127.0.0.1", port)).unwrap();
        wait_for_slot(&srv, 0);

        assert_eq!(trace.get_trace_mask(Some("srv_trace:0")), parent_mask);
        assert_eq!(trace.get_trace_io_mask(Some("srv_trace:0")), parent_io);

        // A later change to the parent does not follow the already-connected
        // child: C copies at the accept, it does not keep them linked.
        trace.set_trace_mask(Some("srv_trace"), TraceMask::ERROR);
        assert_eq!(trace.get_trace_mask(Some("srv_trace:0")), parent_mask);

        srv.disconnect(&AsynUser::default()).unwrap();
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
        let idx = wait_for_slot(&srv, 0);
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

    /// R19-110 boundary: a datagram fans out to every octet interrupt user,
    /// with no consumer polling `read_octet`.
    ///
    /// C's UDP branch (drvAsynIPServerPort.c:309-322) calls every registered
    /// octet callback with the payload straight from `recvfrom` — that is the
    /// only delivery path a UDP server port has for an I/O-Intr scanned record.
    /// The boundary that matters is the same one as R19-109: the subscriber's
    /// addr (here 3) does not match the emission's, and C tests neither.
    #[test]
    fn a_udp_datagram_fans_out_to_every_octet_user() {
        use crate::interrupt::{InterruptFilter, InterruptValue};
        use std::net::UdpSocket as ClientSock;
        use std::sync::Mutex as StdMutex;

        let mut srv = DrvAsynIPServerPort::new("udp_intr", "127.0.0.1:0 UDP").unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let server_addr = srv
            .udp_socket
            .lock()
            .as_ref()
            .unwrap()
            .local_addr()
            .unwrap();

        let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let seen_cb = seen.clone();
        let _cb = srv.base().interrupts.register_sync_callback(
            InterruptFilter {
                addr: Some(3),
                ..InterruptFilter::default()
            },
            move |iv: &InterruptValue| {
                if let ParamValue::Octet(s) = &iv.value {
                    seen_cb.lock().unwrap().push(s.clone());
                }
            },
        );

        let client = ClientSock::bind("127.0.0.1:0").unwrap();
        client.send_to(b"telemetry", server_addr).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if !seen.lock().unwrap().is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no octet interrupt for the datagram (C: drvAsynIPServerPort.c:312-321)"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(seen.lock().unwrap().as_slice(), ["telemetry".to_string()]);

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

    /// DRV-55 (UDP-server sibling): C readIt (drvAsynIPServerPort.c:180-184)
    /// rejects maxchars == 0 with asynError before draining the cache. The
    /// UDP read entries (`read_octet`/`io_read_octet_eom`) bypass
    /// `base_read_octet`, so they must carry the guard themselves — an empty
    /// buffer must error, NOT silently return a zero-byte read that leaves the
    /// cached datagram in place.
    #[test]
    fn udp_server_zero_length_read_rejected() {
        let mut srv = DrvAsynIPServerPort::new("udp_srv_maxchars", "127.0.0.1:0 UDP").unwrap();
        {
            let mut cache = srv.udp_cache.lock();
            cache.data = b"keepme".to_vec();
            cache.pos = 0;
        }
        let user = AsynUser::default().with_addr(0);

        let mut empty: [u8; 0] = [];
        assert!(
            matches!(
                srv.read_octet(&user, &mut empty),
                Err(AsynError::Status {
                    status: AsynStatus::Error,
                    ..
                })
            ),
            "UDP read_octet with maxchars==0 must return asynError"
        );
        let mut empty_eom: [u8; 0] = [];
        assert!(
            matches!(
                srv.io_read_octet_eom(&user, &mut empty_eom),
                Err(AsynError::Status {
                    status: AsynStatus::Error,
                    ..
                })
            ),
            "UDP io_read_octet_eom with maxchars==0 must return asynError"
        );

        // The cached datagram must be untouched by the rejected reads.
        let mut buf = [0u8; 16];
        let n = srv.read_octet(&user, &mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            b"keepme",
            "rejected reads must not drain the cache"
        );
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

        // Server side: the listener thread accepts; read, write reply.
        wait_for_slot(&srv, 0);
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 32];
        let n = srv.read_octet(&user, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello-server");
        srv.write_octet(&mut user, b"hello-client").unwrap();

        let reply = client_handle.join().unwrap();
        assert_eq!(reply, b"hello-client");
    }

    /// The slot table caps concurrent clients: C's listener accepts the
    /// connection anyway (POSIX `accept(2)` has no peek), finds no free child
    /// port, prints "too many clients" and calls `epicsSocketDestroy(clientFd)`
    /// (drvAsynIPServerPort.c:351-355). The peer therefore sees its connection
    /// closed rather than hanging in the backlog.
    #[test]
    fn slot_table_caps_concurrent_clients() {
        let cfg = IpServerConfig {
            bind_host: "127.0.0.1".into(),
            bind_port: 0,
            protocol: IpServerProtocol::Tcp,
            max_clients: 2,
            no_process_eos: false,
            read_timeout: None,
        };
        let mut srv = DrvAsynIPServerPort::with_config("srv2", cfg).unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let port = srv.local_port();

        let _c1 = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        wait_for_slot(&srv, 0);
        let _c2 = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        wait_for_slot(&srv, 1);

        // Third client: both slots are taken, so the server destroys the socket.
        let mut c3 = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        c3.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(
            c3.read(&mut buf).unwrap(),
            0,
            "a client over max_clients must see its connection closed (C: \"too many \
             clients\" + epicsSocketDestroy), not sit in the backlog"
        );
    }

    /// `drop_client` frees a slot for reuse: the running listener lands the next
    /// client in it.
    #[test]
    fn drop_client_releases_slot() {
        let cfg = IpServerConfig {
            bind_host: "127.0.0.1".into(),
            bind_port: 0,
            protocol: IpServerProtocol::Tcp,
            max_clients: 1,
            no_process_eos: false,
            read_timeout: None,
        };
        let mut srv = DrvAsynIPServerPort::with_config("srv3", cfg).unwrap();
        srv.connect(&AsynUser::default()).unwrap();
        let port = srv.local_port();

        let _c1 = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        wait_for_slot(&srv, 0);
        assert!(srv.peer(0).is_some());

        // Free the slot first (operator action), then connect a new
        // client — the next accept lands it.
        srv.drop_client(0).unwrap();
        assert!(srv.peer(0).is_none());

        let _c2 = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        wait_for_slot(&srv, 0);
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
            no_process_eos: false,
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

    /// A child port IS a `drvAsynIPPortConfigure`d port (C creates it with
    /// exactly that call, drvAsynIPServerPort.c:688-694), so it carries the same
    /// configure-time shape as the client IP port the same command builds:
    ///
    /// - `octet_interrupt_process` — `pasynOctetBase->initialize(..., 1)`
    ///   (drvAsynIPPort.c:1055): without it a `stringin` with SCAN="I/O Intr" on
    ///   `<parent>:<N>` never processes (R19-108).
    /// - one EOS interpose — `asynInterposeEosConfig` unless `noProcessEos`
    ///   (drvAsynIPPort.c:1065-1066): without it an IEOS on the child terminates
    ///   nothing (R19-107).
    /// - `auto_connect == false` — C passes `noAutoConnect = 1` for the child;
    ///   the listener connects it by handing it the accepted fd.
    ///
    /// Boundary: `noProcessEos` 0 vs 1 — the ONLY thing that may change is the
    /// EOS layer.
    #[test]
    fn a_child_port_has_the_shape_drv_asyn_ip_port_configure_gives_it() {
        let build = |name: &str, no_process_eos: bool| {
            let cfg = IpServerConfig {
                bind_host: "127.0.0.1".into(),
                bind_port: 0,
                protocol: IpServerProtocol::Tcp,
                max_clients: 2,
                no_process_eos,
                read_timeout: None,
            };
            DrvAsynIPServerPort::with_config(name, cfg).unwrap()
        };

        let srv = build("child_eos_on", false);
        let child = srv.make_subport(1).unwrap();
        assert_eq!(child.base().port_name, "child_eos_on:1");
        assert!(
            child.base().octet_interrupt_process,
            "a child fans its reads out to interrupt users, like every drvAsynIPPort"
        );
        assert_eq!(
            child.base().interpose_octet.len(),
            1,
            "a child gets the default EOS interpose"
        );
        assert!(!child.base().auto_connect, "C passes noAutoConnect=1");

        // noProcessEos=1 suppresses the EOS layer and NOTHING else.
        let srv = build("child_eos_off", true);
        let child = srv.make_subport(0).unwrap();
        assert!(child.base().octet_interrupt_process);
        assert_eq!(
            child.base().interpose_octet.len(),
            0,
            "noProcessEos must suppress the EOS interpose"
        );
        assert!(!child.base().auto_connect);
    }

    /// The parent's own EOS state is not the child's: C's parent server port
    /// registers `pasynOctetBase->initialize(..., 0)` — no EOS processing, no
    /// interruptProcess flag on its own octet interface
    /// (drvAsynIPServerPort.c:655-661) — and it is the CHILDREN that are real
    /// IP ports. Pins the asymmetry so a future "make them consistent" does not
    /// hand the parent an EOS layer C never gives it.
    #[test]
    fn the_server_port_itself_gets_no_eos_interpose() {
        let cfg = IpServerConfig {
            bind_host: "127.0.0.1".into(),
            bind_port: 0,
            protocol: IpServerProtocol::Tcp,
            max_clients: 1,
            no_process_eos: false,
            read_timeout: None,
        };
        let srv = DrvAsynIPServerPort::with_config("srv_no_eos", cfg).unwrap();
        assert_eq!(srv.base().interpose_octet.len(), 0);
    }

    /// make_subport on an out-of-range index errors rather than panic.
    #[test]
    fn make_subport_rejects_out_of_range_idx() {
        let cfg = IpServerConfig {
            bind_host: "127.0.0.1".into(),
            bind_port: 0,
            protocol: IpServerProtocol::Tcp,
            max_clients: 2,
            no_process_eos: false,
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
            no_process_eos: false,
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

        wait_for_slot(&srv, 0);
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
