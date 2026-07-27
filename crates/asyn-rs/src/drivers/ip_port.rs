//! TCP/UDP port driver (drvAsynIPPort equivalent).
//!
//! Supports TCP, UDP, and Unix domain sockets, with protocol suffixes
//! matching C asyn's `drvAsynIPPortConfigure` specification format.

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::time::Duration;

use epics_libcom_rs::runtime::socket;

use super::option_parse::parse_yn_option;
use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::exception::AsynException;
use crate::interpose::com::{ComInterpose, ComPortOptions};
use crate::interpose::{EomReason, OctetInterpose, OctetNext, OctetReadResult};
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
/// - `unix://path` → Unix domain socket. Present wherever C defines
///   `HAS_AF_UNIX` — every unix but VxWorks, which `drvAsynIPPort.c:62`
///   excludes by name. RTEMS keeps it; C excludes only `_WIN32` and
///   `vxWorks`.
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
    /// `COM` — a remote serial port on a terminal server, reached over plain
    /// TCP and configured by RFC 2217 telnet COM-port-option negotiation.
    ///
    /// C parity (`drvAsynIPPort.c:364-367`): the token sets `isCom` and leaves
    /// `socketType = SOCK_STREAM`, so the transport is exactly TCP. What it
    /// changes is that `drvAsynIPPortConfigure` then installs
    /// `asynInterposeCOM` on the port (:1061) — the layer that IAC-escapes the
    /// data stream and carries baud/bits/parity/stop/flow to the server. See
    /// [`crate::interpose::com`].
    Com,
}

impl IpProtocol {
    /// C's `FLAG_BROADCAST` (drvAsynIPPort.c:372-374) — the `udp*` suffix.
    fn broadcast(self) -> bool {
        matches!(self, Self::UdpBroadcast | Self::UdpBroadcastReusePort)
    }

    /// C's `FLAG_SO_REUSEPORT` (drvAsynIPPort.c:360-363, :375-378) — the `&`
    /// suffix, on TCP or UDP.
    fn reuse_port(self) -> bool {
        matches!(
            self,
            Self::TcpReusePort | Self::UdpReusePort | Self::UdpBroadcastReusePort
        )
    }
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

/// Default TCP connect timeout.
///
/// Intentional divergence from C (DRV-13). C `drvAsynIPPort.c::connectIt`
/// (513-523) issues a plain blocking `connect()` with no application-level
/// timeout, so a connect to an unreachable host blocks on the OS-default SYN
/// timeout (~75-130 s on Linux). In the Rust port-actor model that block
/// stalls the port's reconnect cycle (the 2 s auto-reconnect throttle in
/// `port_actor.rs` cannot fire while the actor is parked inside `connect()`),
/// so we bound the connect at 5 s and let the actor fail fast into the
/// retry loop — recovering from a transiently-unreachable device sooner than
/// C's single long block. This is a deliberate robustness improvement, not a
/// copied C bug; the trade-off is a device that genuinely needs >5 s to
/// accept a TCP connection (pathological on a control LAN) would fail here
/// where C's OS-default block might eventually succeed. C exposes no connect
/// timeout option, so this is not runtime-settable either.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

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
            // Where the platform has no AF_UNIX the spec is refused *here*, in
            // parsing, which is where C refuses it — `drvAsynIPPortConfigure`
            // prints "AF_UNIX not available on this platform." and returns -1
            // from inside this same `unix://` branch (drvAsynIPPort.c:315-317).
            // That makes a startup script fail on the line that is wrong,
            // rather than accepting it and failing later on the first connect.
            #[cfg(not(asyn_af_unix))]
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("AF_UNIX not available on this platform: '{path}'"),
            });

            #[cfg(asyn_af_unix)]
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
                    connect_timeout: DEFAULT_CONNECT_TIMEOUT,
                    no_delay: false,
                });
            }
        }

        // Parse protocol suffix (case-insensitive)
        let (addr_part, proto) = split_protocol(spec)?;
        let addr_part = addr_part.trim();

        // Parse host:port[:localPort], supporting IPv6 brackets
        let (host, port, local_port) = parse_host_port(addr_part, spec)?;

        Ok(Self {
            host,
            port,
            local_port,
            protocol: proto,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            no_delay: true,
        })
    }
}

/// Split the trailing protocol token off the address part.
/// Returns `(addr_part, protocol)`.
///
/// C `drvAsynIPPort.c:348-350`: the protocol begins at the first blank
/// (`strchr(cp, ' ')`) and `sscanf(blank+1, "%5s", protocol)` takes the first
/// whitespace-delimited run, truncated to five characters (`char protocol[6]`);
/// whatever follows that run is ignored. With no blank at all, `protocol[0]` is
/// `'\0'` and C defaults to `SOCK_STREAM` (:355-357).
///
/// The token is then classified *exhaustively* — see [`protocol_from_token`].
/// A whitelist of recognized suffixes is not enough: an unrecognized token has
/// to be rejected as a protocol, not silently left glued to the port number.
fn split_protocol(spec: &str) -> AsynResult<(&str, IpProtocol)> {
    let Some(blank) = spec.find(' ') else {
        return Ok((spec, IpProtocol::Tcp));
    };
    let (addr_part, rest) = spec.split_at(blank);
    let token: String = rest
        .split_whitespace()
        .next()
        .unwrap_or("")
        .chars()
        .take(5)
        .collect();
    Ok((addr_part, protocol_from_token(&token)?))
}

/// Classify one protocol token, C `drvAsynIPPort.c:355-390` (`epicsStrCaseCmp`,
/// so case-insensitive). Every token C accepts maps to a variant; every token C
/// rejects is rejected here with C's message.
fn protocol_from_token(token: &str) -> AsynResult<IpProtocol> {
    match token.to_ascii_lowercase().as_str() {
        "" | "tcp" => Ok(IpProtocol::Tcp),
        "tcp&" => Ok(IpProtocol::TcpReusePort),
        "http" => Ok(IpProtocol::Http),
        "udp" => Ok(IpProtocol::Udp),
        "udp&" => Ok(IpProtocol::UdpReusePort),
        "udp*" => Ok(IpProtocol::UdpBroadcast),
        "udp*&" => Ok(IpProtocol::UdpBroadcastReusePort),
        // C (:364-367) accepts `com` and sets `isCom`, which makes
        // `drvAsynIPPortConfigure` install `asynInterposeCOM` (:1061).
        // `DrvAsynIPPort::new` does the same (R8-57); until that layer existed
        // this token was refused outright (R8-50), because a `COM` port run as a
        // plain TCP stream is wrong on the wire in both directions — the device
        // never receives its serial-line negotiation, and the server's
        // IAC/subnegotiation bytes reach the record as device data.
        "com" => Ok(IpProtocol::Com),
        // C prints the token as `sscanf` read it — original case, truncated to
        // five characters (:389).
        _ => Err(AsynError::Status {
            status: AsynStatus::Error,
            message: format!("Unknown protocol \"{token}\"."),
        }),
    }
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
    // C drvAsynIPPort.c::connectIt (513) never connect()s a SOCK_DGRAM
    // socket; it keeps the resolved remote (`tty->farAddr`) and uses
    // sendto/recvfrom. We mirror that: the socket is left unconnected and
    // the resolved peer is carried alongside it for `send_to`.
    Udp(UdpSocket, std::net::SocketAddr),
    #[cfg(asyn_af_unix)]
    Unix(std::os::unix::net::UnixStream),
}

/// `libc`'s RTEMS glob re-exports these two names from more than one module,
/// so a bare `libc::POLLOUT` / `libc::MSG_DONTWAIT` is ambiguous on
/// `armv7-rtems-eabihf` — the `FIONREAD_REQUEST` precedent in
/// `epics-libcom-rs/src/runtime/blocking_io.rs`. VxWorks 7 defines both
/// unambiguously and at these same values (`libc` `src/vxworks/mod.rs:1198`
/// and `:1029`).
#[cfg(target_os = "rtems")]
const POLLOUT_EVENT: libc::c_short = 0x0004;
#[cfg(target_os = "rtems")]
const SEND_DONTWAIT: libc::c_int = 0x0080;
#[cfg(all(unix, not(target_os = "rtems")))]
const POLLOUT_EVENT: libc::c_short = libc::POLLOUT;
#[cfg(all(unix, not(target_os = "rtems")))]
const SEND_DONTWAIT: libc::c_int = libc::MSG_DONTWAIT;

/// Wait until the socket will take bytes, or `deadline` passes first
/// (`Ok(false)`).
///
/// C parity, and why the write bound is not `SO_SNDTIMEO`: `drvAsynIPPort.c`
/// compiles the `setsockopt(SO_SNDTIMEO)` block only under `USE_SOCKTIMEOUT`,
/// which is `#if defined(__rtems__)` (`:71-72`). Every other target — vxWorks
/// included, through its own `FAKE_POLL` (`:75-76`) — takes `USE_POLL` (`:74`),
/// where `writeIt` polls `POLLOUT` for `writePollmsec` (`:667-686`) and then
/// sends. This port armed `SO_SNDTIMEO` on all platforms instead, and VxWorks 7
/// does not implement the option: `setsockopt` fails `ENOPROTOOPT` (errno 42,
/// measured on target), so the `?` on it failed every asyn IP write there
/// before a byte was sent.
trait WaitWritable {
    fn wait_writable(&self, deadline: std::time::Instant) -> std::io::Result<bool>;
}

/// Hand bytes to the socket without parking — the stream arms of [`IpIoInner`].
///
/// C reaches the same place by putting the socket in non-blocking mode once, at
/// connect (`setNonBlock(fd, 1)`, `drvAsynIPPort.c:511`, under `USE_POLL`). This
/// port carries `MSG_DONTWAIT` per send instead: C polls its reads too, whereas
/// this driver bounds reads with `SO_RCVTIMEO` — which VxWorks does implement,
/// and which a permanently non-blocking socket would defeat. A per-send flag
/// leaves the shared file description, and that read bound, untouched.
///
/// The wait alone is not enough to make the deadline hold: a blocking `write` on
/// a stream socket does not return a short count when the send buffer fills, it
/// waits until the whole buffer is queued.
///
/// # Known gap: Darwin ignores the flag, and C does not rely on it
///
/// XNU's `sosend` sleeps on `so_state & SS_NBIO` and its internal `MSG_NBIO`,
/// not on `MSG_DONTWAIT`, so on macOS and iOS the send parks and
/// `write_octet`'s timeout stops bounding it — measured, macOS CI 2026-07-27,
/// where a stalled 8 MiB write outlived a 240 s test timeout. C is unaffected
/// precisely because it takes the other branch above: `setNonBlock(fd, 1)` at
/// connect plus a poll on reads. Closing this means adopting that shape here,
/// which costs the `SO_RCVTIMEO` read bound VxWorks does implement.
/// `doc/darwin-send-dontwait-gap.md`.
trait SendWithoutParking {
    fn send_without_parking(&self, buf: &[u8]) -> std::io::Result<usize>;
}

#[cfg(unix)]
impl<T: std::os::fd::AsRawFd> WaitWritable for T {
    fn wait_writable(&self, deadline: std::time::Instant) -> std::io::Result<bool> {
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            let mut fds = libc::pollfd {
                fd: self.as_raw_fd(),
                events: POLLOUT_EVENT,
                revents: 0,
            };
            let ms = remaining.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
            let rc = unsafe { libc::poll(&mut fds, 1, ms) };
            if rc > 0 {
                return Ok(true);
            }
            if rc == 0 {
                return Ok(false);
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

#[cfg(unix)]
impl<T: std::os::fd::AsRawFd> SendWithoutParking for T {
    fn send_without_parking(&self, buf: &[u8]) -> std::io::Result<usize> {
        let n = unsafe {
            libc::send(
                self.as_raw_fd(),
                buf.as_ptr().cast(),
                buf.len(),
                SEND_DONTWAIT,
            )
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(n as usize)
    }
}

/// Windows implements `SO_SNDTIMEO`, so there the bound rides on the option and
/// the send may park under it — the same split `blocking_io.rs` takes.
#[cfg(not(unix))]
fn arm_send_timeout(
    set: impl FnOnce(Option<Duration>) -> std::io::Result<()>,
    deadline: std::time::Instant,
) -> std::io::Result<bool> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Ok(false);
    }
    set(Some(remaining))?;
    Ok(true)
}

#[cfg(not(unix))]
impl WaitWritable for TcpStream {
    fn wait_writable(&self, deadline: std::time::Instant) -> std::io::Result<bool> {
        arm_send_timeout(|t| self.set_write_timeout(t), deadline)
    }
}

#[cfg(not(unix))]
impl WaitWritable for UdpSocket {
    fn wait_writable(&self, deadline: std::time::Instant) -> std::io::Result<bool> {
        arm_send_timeout(|t| self.set_write_timeout(t), deadline)
    }
}

#[cfg(not(unix))]
impl SendWithoutParking for TcpStream {
    fn send_without_parking(&self, buf: &[u8]) -> std::io::Result<usize> {
        (&mut &*self).write(buf)
    }
}

/// Write `data` to the stream, retrying short writes until the deadline.
///
/// Returns the bytes the peer accepted. C parity
/// (`drvAsynIPPort.c::writeIt`, like `drvAsynSerialPort.c:849`): a write that
/// stalls part-way reports `*nbytesTransfered` **together with** its
/// `asynTimeout`/`asynError` status, so a failure here carries the accepted
/// count in [`AsynError::with_partial_write`] rather than dropping it — that
/// count is what `asynRecord` publishes as NAWT (asynRecord.c:1547).
///
/// The loop is C's shape: wait for `POLLOUT`, send, advance, repeat until the
/// buffer is gone (`drvAsynIPPort.c:666-733`). Waiting first is what bounds the
/// call — see [`WaitWritable`] for why that bound is not `SO_SNDTIMEO`.
fn write_with_retry(
    stream: &(impl WaitWritable + SendWithoutParking),
    data: &[u8],
    deadline: std::time::Instant,
) -> AsynResult<usize> {
    let mut offset = 0;
    while offset < data.len() {
        match stream.wait_writable(deadline) {
            Ok(true) => {}
            Ok(false) => {
                return Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "write timeout".into(),
                }
                .with_partial_write(offset));
            }
            Err(e) => return Err(AsynError::Io(e).with_partial_write(offset)),
        }
        match stream.send_without_parking(&data[offset..]) {
            Ok(0) => {
                return Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "write returned 0 bytes".into(),
                }
                .with_partial_write(offset));
            }
            Ok(n) => offset += n,
            // C retries both of these against `pasynUser->timeout`
            // (`drvAsynIPPort.c:694-710`); the deadline is re-read at the top
            // of the loop, so the retry is bounded without a sleep.
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(AsynError::Io(e).with_partial_write(offset)),
        }
    }
    Ok(offset)
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
        // C readRaw (drvAsynIPPort.c:736-740): reject maxchars == 0 *after*
        // the connect check and *before* touching the socket, so a zero-length
        // request never reaches `stream.read`, where an empty buffer would
        // yield Ok(0) and be misread as a peer EOF (tearing down the socket).
        if buf.is_empty() {
            return Err(maxchars_zero_error());
        }
        match inner {
            IpIoInner::Tcp(stream) => {
                // C readRaw (drvAsynIPPort.c:744-756) records a failed
                // setsockopt(SO_RCVTIMEO) but falls through to recv(); the recv
                // outcome governs teardown (:797-821). On macOS this setsockopt
                // returns EINVAL on a reset socket, so an early return (`?`) here
                // would replace the ECONNRESET the read is about to surface with a
                // synthetic "set_read_timeout failed", and the port would never
                // classify the transport as dead. Drop the error and read — the
                // status taint C keeps for a >0-byte read (:822-831) is
                // unreachable here (the EINVAL cause is a socket whose read fails).
                let _ = stream.set_read_timeout(Some(socket_poll_timeout(user.timeout)));
                match stream.read(buf) {
                    // C drvAsynIPPort.c::readRaw (815-821): recv()==0 on a
                    // SOCK_STREAM socket means the peer closed — report
                    // success with ASYN_EOM_END and zero bytes (the driver
                    // then closes the fd). Returning an error here would hide
                    // END from close-delimited protocols (HTTP/1.0) that use
                    // connection-close as the message terminator.
                    Ok(0) => Ok(OctetReadResult {
                        nbytes_transferred: 0,
                        eom_reason: EomReason::END,
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
                    Err(e) => Err(classify_read_error(e)),
                }
            }
            IpIoInner::Udp(socket, _peer) => {
                // Same C fall-through as the TCP arm: setsockopt(SO_RCVTIMEO)
                // precedes the socketType branch in readRaw (:749) and a failure
                // does not return — the recvfrom governs. Drop the error and read.
                let _ = socket.set_read_timeout(Some(socket_poll_timeout(user.timeout)));
                // C drvAsynIPPort.c::readRaw (775-789) uses recvfrom on the
                // unconnected datagram socket so it accepts replies from any
                // peer (broadcast/multi-peer); the source address is only
                // used for trace, so we discard it.
                match socket.recv_from(buf) {
                    // C drvAsynIPPort.c::readRaw: a SOCK_DGRAM recvfrom()==0
                    // is a legitimate zero-length datagram, NOT a connection
                    // close — the EOF/ASYN_EOM_END branch (line 815) is
                    // SOCK_STREAM only. Report a successful zero-byte read and
                    // leave the socket open (no teardown).
                    Ok((n, _src)) => Ok(OctetReadResult {
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
                    Err(e) => Err(classify_read_error(e)),
                }
            }
            #[cfg(asyn_af_unix)]
            IpIoInner::Unix(stream) => {
                // Same C fall-through as the TCP arm (drvAsynIPPort.c:744-756):
                // a failed set_read_timeout must not pre-empt the read that
                // surfaces the real transport error. Drop the error and read.
                let _ = stream.set_read_timeout(Some(socket_poll_timeout(user.timeout)));
                match stream.read(buf) {
                    // Unix-domain stream EOF = peer closed = END, the same
                    // stream semantics as the TCP arm above.
                    Ok(0) => Ok(OctetReadResult {
                        nbytes_transferred: 0,
                        eom_reason: EomReason::END,
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
                    Err(e) => Err(classify_read_error(e)),
                }
            }
        }
    }

    fn write(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let inner = self.inner.as_mut().ok_or_else(|| AsynError::Status {
            status: AsynStatus::Disconnected,
            message: "not connected".into(),
        })?;
        // C drvAsynIPPort.c::writeRaw (613-614): after the connection check,
        // a zero-length write returns success immediately and sends nothing —
        // in particular it must NOT emit an empty UDP datagram. (Any output
        // EOS has already been appended by the interpose above, so reaching
        // here with empty data means there is genuinely nothing to send.)
        if data.is_empty() {
            return Ok(0);
        }
        // Floor the write deadline with the same C poll-interval floor as the
        // socket send timeout: C writeRaw (615-617) floors a zero timeout to a
        // 1 ms poll and then attempts the send (poll POLLOUT then send), so a
        // `timeout == 0` write of a writable socket succeeds. Without the
        // floor the deadline would be `now`, and `write_with_retry`'s
        // top-of-loop `now > deadline` check would return Timeout *before*
        // ever attempting `write` — failing a zero-timeout write that C lets
        // through. socket_poll_timeout is a no-op for any positive timeout, so
        // this only affects the `timeout == 0` case.
        let deadline = std::time::Instant::now() + socket_poll_timeout(user.timeout);
        // C writeRaw reports what the socket took (`*nbytesTransfered`) on
        // success and on failure alike; return the real count rather than
        // assuming the whole buffer went out.
        // No arm arms `SO_SNDTIMEO`: the deadline is enforced by the wait in
        // `write_with_retry`, which is what C does everywhere but RTEMS. See
        // `WaitWritable` — VxWorks 7 rejects the option outright, so setting it
        // here failed every write on that target before a byte was sent.
        match inner {
            IpIoInner::Tcp(stream) => write_with_retry(stream, data, deadline),
            IpIoInner::Udp(socket, peer) => {
                // C polls `POLLOUT` before the datagram too — `writeIt` shares
                // one loop across `SOCK_DGRAM` and `SOCK_STREAM`
                // (`drvAsynIPPort.c:666-691`).
                if !socket.wait_writable(deadline)? {
                    return Err(AsynError::Status {
                        status: AsynStatus::Timeout,
                        message: "write timeout".into(),
                    });
                }
                // C drvAsynIPPort.c::writeIt (688): sendto the resolved
                // remote on the unconnected socket. A datagram is all-or-
                // nothing, so a failed sendto transferred zero bytes and needs
                // no partial-write carrier.
                Ok(socket.send_to(data, *peer)?)
            }
            #[cfg(asyn_af_unix)]
            IpIoInner::Unix(stream) => write_with_retry(stream, data, deadline),
        }
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
            Some(IpIoInner::Udp(socket, _peer)) => {
                let restore = socket.set_nonblocking(true);
                loop {
                    match socket.recv_from(&mut scratch) {
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
            #[cfg(asyn_af_unix)]
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
    /// Verbatim host-info spec, mirroring C `ttyController_t::IPDeviceName`
    /// (`drvAsynIPPort.c`). `parseHostInfo` stores `epicsStrDup(hostInfo)`
    /// on construction and on every `setOption("hostInfo", ...)` reparse;
    /// `getOption("hostInfo")` returns it verbatim. Kept so the IP driver's
    /// `get_option` can echo the live endpoint instead of the generic map.
    host_info: String,
    /// Both halves of C's COM interpose, present exactly when the configure
    /// string's protocol token was `COM` — C's `tty->isCom` (`drvAsynIPPort.c`
    /// :113, :364-365) and the `asynInterposeCOM` install it gates (:1061).
    com: Option<ComState>,
}

/// The two halves of C's `asynInterposeCOM`, which it interposes on the port as
/// two separate interfaces (`asynInterposeCom.c:792-824`). One `Option`, so "a
/// COM port has both halves and a non-COM port has neither" holds by type.
struct ComState {
    /// The `asynOctet` half — IAC stuffing.
    ///
    /// Deliberately **not** installed onto [`PortDriverBase::interpose_octet`]. C
    /// installs COM at :1061 and the EOS interpose at :1065, and its
    /// `interposeInterface` makes each *later* install the *outer* one — so C's
    /// chain is `EOS → COM → driver`, with COM directly above the IP driver's
    /// octet interface and below every other interpose.
    ///
    /// Making it the base link (see [`DrvAsynIPPort::with_base_link`]) puts it
    /// beneath every stack layer *by construction*, so no install order can get it
    /// wrong — rather than pinning the ordering with a convention the next person
    /// to install a layer has to know about. (The stack now follows C's rule too —
    /// [`crate::interpose::OctetInterposeStack::install`] — so COM installed first
    /// would land innermost anyway; the base link keeps the invariant whatever
    /// else is installed later.)
    octet: ComInterpose,
    /// The `asynOption` half — the negotiation and the serial settings it
    /// carries. Driven against the raw link, since C's `setOption`/`getOption`
    /// negotiate through `pasynOctetDrv`, the driver *below* the interpose
    /// (`asynInterposeCom.c:339`, :430) — so the negotiation is neither stuffed
    /// nor unstuffed by the octet half above.
    options: ComPortOptions,
}

/// The base of the interpose chain for a COM port: the socket with the IAC layer
/// wrapped around it. See [`ComState::octet`] for why COM lives here rather than
/// on the stack.
struct ComLink<'a> {
    io: &'a mut IpIoState,
    com: &'a mut ComInterpose,
}

impl OctetNext for ComLink<'_> {
    fn read(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
        self.com.read(user, buf, self.io)
    }
    fn write(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        self.com.write(user, data, self.io)
    }
    fn flush(&mut self, user: &mut AsynUser) -> AsynResult<()> {
        self.com.flush(user, self.io)
    }
}

/// True for a socket read error that C `readRaw` treats as a non-fatal
/// timeout — the socket is left intact and the caller retries. C
/// (drvAsynIPPort.c:798-800) excludes BOTH `EWOULDBLOCK` *and* `EINTR` from
/// the fatal-disconnect disjunct, so a would-block / poll timeout AND a
/// signal-interrupted read are equally non-fatal. Single owner of that rule,
/// shared by the IP client read arms (`classify_read_error`) and the IP
/// server read arms (`ip_server_port`), so "EINTR is non-fatal like
/// WouldBlock" is defined in exactly one place.
pub(crate) fn is_nonfatal_read_timeout(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::Interrupted
    )
}

/// Error for a zero-length read request. C `readRaw` (drvAsynIPPort.c:736-740)
/// rejects `maxchars == 0` with `asynError` ("maxchars %d. Why <=0?") before
/// touching the socket. Without this guard a Rust `stream.read(&mut [])`
/// returns `Ok(0)`, which the TCP read arm interprets as a peer EOF and tears
/// down a perfectly healthy connection. Single owner of the empty-buffer
/// rejection, shared by the IP client read and both IP server reads.
pub(crate) fn maxchars_zero_error() -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: "maxchars 0. Why <=0?".into(),
    }
}

/// What C `readRaw` does with a failed read (drvAsynIPPort.c:797-812) — the
/// teardown and the status it reports, as one value.
///
/// ```c
/// int should_disconnect = (((tty->disconnectOnReadTimeout) && (pasynUser->timeout > 0)) ||
///                          ((SOCKERRNO != SOCK_EWOULDBLOCK) && (SOCKERRNO != SOCK_EINTR)));
/// if (should_disconnect) {
///     epicsSnprintf(... "%s read error: %s" ...);
///     closeConnection(pasynUser,tty,"Read error");
///     status = asynError;                       /* :805 */
/// } else {
///     epicsSnprintf(... "%s timeout: %s" ...);
///     status = asynTimeout;                     /* :811 */
/// }
/// ```
///
/// C decides both in one statement: the branch that closes the socket is the
/// branch that reports `asynError`. Returning them separately is what let the
/// port drop the connection and still hand the caller the lower layer's
/// `asynTimeout` — so ERRS read `"Read error, nread=0, timeout"` where C says
/// `"error"`, and a SyncIO caller branching on the status saw a retryable
/// timeout on a link that no longer exists.
struct ReadFailure {
    /// C's `should_disconnect`: a timeout tears down only when the option is on
    /// *and* the request carried a positive timeout (a `timeout == 0` poll —
    /// floored to a 1 ms socket poll by [`socket_poll_timeout`] — expires with
    /// the socket intact), and any non-would-block, non-EINTR error tears down
    /// unconditionally.
    disconnect: bool,
    /// The error the caller reports: C's `asynError` + `"read error"` text on
    /// the teardown branch, the lower layer's failure verbatim otherwise.
    error: AsynError,
}

/// Single owner of C `readRaw`'s failed-read rule. C applies it *inside*
/// `readRaw`, which is below the interpose chain — so it governs both the data
/// path ([`DrvAsynIPPort::read_octet_core`]) and the RFC 2217 negotiation
/// ([`NegotiationLink`]), which C likewise drives through `readIt`/`readRaw`
/// (`asynInterposeCom.c:103` calls `pasynOctetDrv->read`, the driver below).
/// Both callers take the teardown *and* the reported status from here, so
/// neither can drift.
fn classify_read_failure(
    e: AsynError,
    disconnect_on_read_timeout: bool,
    timeout: Duration,
    device_name: &str,
) -> ReadFailure {
    // Classify by the carried status, not the variant: an interpose below may
    // wrap the lower-layer timeout in `PartialRead`, and a variant match would
    // miss it.
    let is_timeout = e.status() == AsynStatus::Timeout;
    let disconnect = (disconnect_on_read_timeout && is_timeout && timeout > Duration::ZERO)
        || e.is_fatal_transport();
    if !disconnect {
        return ReadFailure {
            disconnect: false,
            error: e,
        };
    }
    // C overwrites `pasynUser->errorMessage` and the status together (:801-805)
    // — while the partial transfer already in the caller's buffer stands, because
    // `*nbytesTransfered = thisRead` (:824) runs on both branches. So restamp the
    // failure and re-attach whatever the read did deliver.
    let partial = e.partial_read().cloned();
    let restamped = AsynError::Status {
        status: AsynStatus::Error,
        message: format!("{device_name} read error: {}", e.message()),
    };
    ReadFailure {
        disconnect: true,
        error: match partial {
            Some(p) => restamped.with_partial_read(p),
            None => restamped,
        },
    }
}

/// The raw link the RFC 2217 negotiation drives, and the teardown decision it
/// hands back.
///
/// C's COM interpose negotiates against `pinterposePvt->pasynOctetDrv`
/// (`asynInterposeCom.c:339`, :430, :103) — the octet driver *below* itself, so
/// the negotiation is neither IAC-stuffed nor unstuffed. But that lower driver is
/// `drvAsynIPPort`'s `readIt`/`readRaw`, which still applies the
/// disconnect-on-read-timeout / fatal-error teardown per read. Handing
/// [`ComPortOptions`] a bare `IpIoState` would have skipped it, leaving a COM
/// port with `disconnectOnReadTimeout=Y` holding a socket C would have closed.
///
/// The socket has one owner ([`DrvAsynIPPort`]), and a `&mut IpIoState` borrow is
/// live for the whole negotiation, so this cannot call `drop_connection` itself.
/// It records the decision instead and the owner applies it once the borrow ends
/// — the same "return the delta, let the owner commit it" shape the rest of the
/// driver uses.
struct NegotiationLink<'a> {
    io: &'a mut IpIoState,
    disconnect_on_read_timeout: bool,
    /// C `tty->IPDeviceName`, for the `"%s read error: %s"` text `readRaw`
    /// reports on the teardown branch (drvAsynIPPort.c:801-803).
    device_name: &'a str,
    /// Set when a read failed in a way C's `readRaw` would have closed the
    /// socket for. Read back by [`DrvAsynIPPort::with_negotiation`].
    teardown: bool,
}

impl OctetNext for NegotiationLink<'_> {
    fn read(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
        match self.io.read(user, buf) {
            Ok(r) => Ok(r),
            Err(e) => {
                let failure = classify_read_failure(
                    e,
                    self.disconnect_on_read_timeout,
                    user.timeout,
                    self.device_name,
                );
                if failure.disconnect {
                    self.teardown = true;
                }
                Err(failure.error)
            }
        }
    }

    fn write(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let r = self.io.write(user, data);
        if let Err(ref e) = r {
            // C `writeRaw` (drvAsynIPPort.c:625-640) closes the connection on any
            // failing send, which `is_fatal_transport` is the single owner of.
            if e.is_fatal_transport() {
                self.teardown = true;
            }
        }
        r
    }

    fn flush(&mut self, user: &mut AsynUser) -> AsynResult<()> {
        self.io.flush(user)
    }
}

/// Classify a failed socket read for the IP client read arms. A non-fatal
/// timeout (see [`is_nonfatal_read_timeout`]) surfaces as `asynTimeout` with
/// the socket left intact; any other error is a real transport failure
/// (`Io`) that the read path's teardown treats as fatal. Single owner of
/// read-error classification for the TCP/UDP/Unix read arms.
fn classify_read_error(e: std::io::Error) -> AsynError {
    if is_nonfatal_read_timeout(e.kind()) {
        AsynError::Status {
            status: AsynStatus::Timeout,
            message: "read timeout".into(),
        }
    } else {
        AsynError::Io(e)
    }
}

/// Map an `AsynUser` timeout to the socket-level receive/send timeout,
/// applying the C `drvAsynIPPort` poll-interval floor.
///
/// C `readRaw`/`writeRaw` compute `pollmsec = (int)(timeout * 1000)` and
/// then `if (pollmsec == 0) pollmsec = 1` (drvAsynIPPort.c:741-743 and
/// 615-617): a zero timeout (a poll / non-blocking request) becomes a 1 ms
/// poll, never an immediate-return zero interval. `std::net`'s
/// `set_read_timeout`/`set_write_timeout` likewise reject a zero `Duration`
/// (they return `InvalidInput`), so passing `user.timeout` verbatim would
/// turn a `timeout == 0` request into a hard error instead of C's 1 ms
/// poll. Flooring here is the single owner of that mapping for every
/// IP-family socket-timeout site (client read/write plus the server-mode
/// reads).
///
/// A negative "wait forever" timeout has no representation in the
/// framework's unsigned `Duration` (DRV-42), so only the zero floor
/// applies; a positive sub-millisecond timeout is passed through verbatim
/// (Rust polls at sub-ms granularity where C coarsens to whole ms —
/// strictly finer, not a regression).
pub(crate) fn socket_poll_timeout(timeout: Duration) -> Duration {
    if timeout.is_zero() {
        Duration::from_millis(1)
    } else {
        timeout
    }
}

impl DrvAsynIPPort {
    /// Tear down the live socket. C parity: `drvAsynIPPort.c::closeConnection`
    /// (205-217) destroys the fd and fires `exceptionDisconnect` — but
    /// **only** when the link is *not* connect-per-transaction
    /// (`!(flags & FLAG_CONNECT_PER_TRANSACTION)`, line 214-216).
    ///
    /// For a normal link this drops `connected` to `false` so the actor's
    /// auto-reconnect (`port_actor.rs`) re-establishes it on the next
    /// request. For HTTP (connect-per-transaction) the socket churns once
    /// per request/response, so the logical connection stays *up*: only the
    /// socket (`io.inner`) is released, `connected` is left `true`, and the
    /// `Connect` exception does not flap each transaction. The socket is
    /// reopened lazily on the next write/read (gated on `io.inner.is_none()`,
    /// not `!connected`).
    fn drop_connection(&mut self) {
        self.io.inner = None;
        if self.config.protocol != IpProtocol::Http {
            self.base.set_connected(false);
        }
    }

    /// Shared read core for [`PortDriver::read_octet`] (which drops the EOM
    /// reason) and [`PortDriver::io_read_octet_eom`] (which keeps it).
    /// Dispatches the interpose chain once and applies the C
    /// `closeConnection` teardown on a fatal error, a
    /// `disconnectOnReadTimeout` timeout, or a TCP EOF (which the base read
    /// reports as `ASYN_EOM_END`). Returning the real `eom_reason` here is
    /// what lets END/EOS reach the actor — the `usize`-only `read_octet`
    /// would otherwise discard it, so END was never emitted anywhere.
    fn read_octet_core(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
    ) -> AsynResult<(usize, EomReason)> {
        // HTTP connect-per-transaction: (re)open the socket if there isn't
        // one. Gated on socket presence (`io.inner.is_none()`), NOT on
        // `!connected`: an HTTP link stays *logically* connected across
        // transactions (drop_connection releases only the socket), so the
        // post-EOF reopen must key off the missing socket. `connect()` is
        // edge-guarded — it no-ops `set_connected(true)` when already
        // connected, so reopening does not flap the Connect exception.
        // Surfacing the connect failure cause (DNS, refused, TLS reset) here
        // rather than letting check_ready() mask it as a generic
        // "port disconnected".
        if self.config.protocol == IpProtocol::Http && self.io.inner.is_none() {
            self.connect(&AsynUser::default())?;
        }
        self.base.check_ready()?;
        let result = self.with_base_link(|link| link.read(user, buf));
        match result {
            Ok(r) => {
                asyn_trace_io!(
                    Some(self.base.trace),
                    &self.base.port_name,
                    TraceMask::IO_DRIVER,
                    &buf[..r.nbytes_transferred],
                    "read"
                );
                // C drvAsynIPPort.c::readRaw (815-819): a TCP EOF (reported
                // as ASYN_EOM_END by the base read) is the *only* thing that
                // closes the connection on a successful read. HTTP is
                // connect-per-transaction but the response still ends on the
                // server's close (HTTP/1.0 connection-close = EOF), so it
                // must drop on EOF too — and ONLY on EOF. Dropping after
                // every HTTP read (the old `|| Http`) truncated multi-segment
                // responses: a first read returning one TCP segment (no END)
                // tore the socket down before the caller could read the rest.
                // `drop_connection` keeps the logical connection up for HTTP
                // (no Connect-exception flap); the socket reopens lazily on
                // the next write.
                let eof = r.eom_reason.contains(EomReason::END);
                if eof && self.base.is_connected() {
                    self.drop_connection();
                }
                Ok((r.nbytes_transferred, r.eom_reason))
            }
            Err(e) => {
                // Classify by the carried status, not by the variant — the
                // same rule `is_fatal_transport_error` already follows. The
                // read below is dispatched through the interpose chain, and
                // `drvAsynIPPortConfigure` installs the EOS interpose by
                // default (iocsh.rs), which wraps *every* lower-layer failure
                // — including a zero-byte recv timeout — as `PartialRead`
                // (C `asynInterposeEos.c:242-253` likewise returns the
                // lower-layer `asynTimeout`, unchanged, alongside the count).
                // A variant match therefore never saw a timeout on a default
                // IP port and `disconnectOnReadTimeout` could not fire; C runs
                // its test inside readRaw, below the interpose, where the raw
                // recv timeout is visible (drvAsynIPPort.c:798-806).
                let failure = classify_read_failure(
                    e,
                    self.disconnect_on_read_timeout,
                    user.timeout,
                    &self.host_info,
                );
                if failure.disconnect && self.base.is_connected() {
                    asyn_trace!(
                        Some(self.base.trace),
                        &self.base.port_name,
                        TraceMask::FLOW,
                        "read error, disconnecting: {}",
                        failure.error
                    );
                    self.drop_connection();
                }
                Err(failure.error)
            }
        }
    }

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
        base.init_connected(false);
        base.auto_connect = true;
        // C passes `interruptProcess = 1` to `pasynOctetBase->initialize`
        // (drvAsynIPPort.c:1055): every successful octet read on this port fans out to
        // its octet interrupt users, which is what drives a SCAN="I/O Intr"
        // record. See `PortDriverBase::octet_interrupt_process`.
        base.octet_interrupt_process = true;

        // C `drvAsynIPPortConfigure` (:1061): `if (tty->isCom) asynInterposeCOM(...)`
        // — installed at configure time, before the port ever connects, so the
        // very first byte of the very first transfer is already IAC-escaped
        // (R8-57).
        let com = (config.protocol == IpProtocol::Com).then(|| ComState {
            octet: ComInterpose::new(),
            options: ComPortOptions::new(),
        });

        Ok(Self {
            base,
            config,
            io: IpIoState { inner: None },
            disconnect_on_read_timeout: false,
            // C parseHostInfo: tty->IPDeviceName = epicsStrDup(hostInfo).
            host_info: config_str.to_string(),
            com,
        })
    }

    /// Build a port configured exactly as C's `drvAsynIPPortConfigure(portName,
    /// hostInfo, priority, noAutoConnect, noProcessEos)` leaves one:
    /// [`Self::new`] plus `Self::apply_ip_port_configure` (the `registerPort`
    /// autoConnect flag and, unless `noProcessEos`, the default EOS interpose —
    /// drvAsynIPPort.c:1043-1066).
    ///
    /// The single public entry for callers outside the iocsh command that need a
    /// fully-configured IP port — e.g. a `noAutoConnect` port registered for a
    /// record to attach to without ever dialing. `priority` has no effect in the
    /// Rust runtime (see the iocsh command), so it takes no such parameter. The
    /// configure steps stay owned by `apply_ip_port_configure`; this only
    /// sequences `new` before it.
    pub fn new_configured(
        port_name: &str,
        config_str: &str,
        no_auto_connect: bool,
        no_process_eos: bool,
    ) -> AsynResult<Self> {
        let mut driver = Self::new(port_name, config_str)?;
        Self::apply_ip_port_configure(&mut driver.base, no_auto_connect, no_process_eos);
        Ok(driver)
    }

    /// Run one transfer against the port's raw octet link — the **bottom** of
    /// the port's interpose chain, which the port itself runs on top of this
    /// driver ([`crate::port::octet_read_chain`], C asynManager.c:2190-2220).
    ///
    /// Single owner of what that chain terminates at: the raw socket, or — for a
    /// COM port — the socket wrapped in the IAC layer, which is what puts COM
    /// beneath every stack layer by construction (see [`ComState::octet`]). Every
    /// octet path goes through here, so read, write and flush cannot disagree
    /// about where the chain ends.
    fn with_base_link<T>(&mut self, f: impl FnOnce(&mut dyn OctetNext) -> T) -> T {
        match self.com.as_mut() {
            Some(com) => {
                let mut link = ComLink {
                    io: &mut self.io,
                    com: &mut com.octet,
                };
                f(&mut link)
            }
            None => f(&mut self.io),
        }
    }

    /// Run one RFC 2217 exchange against the link below the interpose, then
    /// apply whatever teardown C's `readRaw`/`writeRaw` would have applied
    /// during it.
    ///
    /// The single gate for every negotiation in this driver — the connect-time
    /// handshake and each `setOption` — so the socket-teardown rule cannot be
    /// enforced on one path and missed on the other. See [`NegotiationLink`] for
    /// why the decision is carried out rather than applied in place.
    fn with_negotiation<T>(
        &mut self,
        f: impl FnOnce(&mut ComPortOptions, &mut NegotiationLink) -> T,
    ) -> Option<T> {
        let com = self.com.as_mut()?;
        let mut link = NegotiationLink {
            io: &mut self.io,
            disconnect_on_read_timeout: self.disconnect_on_read_timeout,
            device_name: &self.host_info,
            teardown: false,
        };
        let out = f(&mut com.options, &mut link);
        let teardown = link.teardown;
        if teardown && self.base.is_connected() {
            self.drop_connection();
        }
        Some(out)
    }

    /// Run the RFC 2217 handshake against the freshly-connected server.
    ///
    /// C wires this to `asynExceptionConnect` (`asynInterposeCom.c:763-774`):
    /// every time the port connects — including after a terminal server reboots
    /// — the negotiation is replayed so the serial line comes back up with the
    /// settings the IOC last asked for. Its failure is reported and *swallowed*,
    /// exactly as C's `exceptionHandler` does (`asynPrint(ASYN_TRACE_ERROR)`,
    /// :770-772): a terminal server that merely refuses the negotiation still
    /// leaves a usable TCP link, and failing the connect here would instead put
    /// the port into an endless reconnect loop. A negotiation that failed because
    /// the *socket* died is a different matter, and `with_negotiation` has
    /// already closed it by the time we get here — same as C.
    fn restore_com_settings(&mut self) {
        let Some(Err(e)) = self.with_negotiation(|com, link| com.restore_settings(link)) else {
            return;
        };
        asyn_trace!(
            Some(self.base.trace),
            &self.base.port_name,
            TraceMask::ERROR,
            "Unable to restore parameters for port {}: {}",
            self.base.port_name,
            e.message()
        );
    }

    /// Push an interpose layer onto the octet I/O stack.
    pub fn install_interpose(&mut self, layer: Box<dyn crate::interpose::OctetInterpose>) {
        self.base.install_octet_interpose(layer);
    }

    /// Everything `drvAsynIPPortConfigure` gives a port beyond constructing the
    /// driver (drvAsynIPPort.c:1043-1066):
    ///
    /// ```c
    /// pasynManager->registerPort(tty->portName, ASYN_CANBLOCK, !noAutoConnect, ...);
    /// pasynOctetBase->initialize(tty->portName, &tty->octet, 0, 0, 1);  /* interruptProcess */
    /// if (!noProcessEos) asynInterposeEosConfig(portName, -1, 1, 1);
    /// ```
    ///
    /// It takes the `PortDriverBase` rather than `&mut self` because **C's
    /// IP-server child ports are created by this very call** —
    /// `drvAsynIPServerPortConfigure` runs `drvAsynIPPortConfigure(pl->portName,
    /// tty->serverInfo, priority, 1 /*noAutoConnect*/, tty->noProcessEos)` for
    /// each slot (drvAsynIPServerPort.c:688-694). A child is not "a port plus
    /// some of what a port gets": it gets all of it. Sharing one owner is what
    /// makes that true here too — the child inherited none of it while each
    /// property lived inside `DrvAsynIPPort::new` (R19-107, R19-108).
    pub(crate) fn apply_ip_port_configure(
        base: &mut crate::port::PortDriverBase,
        no_auto_connect: bool,
        no_process_eos: bool,
    ) {
        // `pasynOctetBase->initialize(..., interruptProcess = 1)`: every
        // successful octet read on the port fans out to its octet interrupt
        // users, which is what drives a SCAN="I/O Intr" record.
        base.octet_interrupt_process = true;
        // `registerPort(..., !noAutoConnect, ...)`.
        base.auto_connect = !no_auto_connect;
        // `asynInterposeEosConfig(portName, -1, 1, 1)` unless suppressed. Without
        // it the port has NO EOS layer at all, so an IEOS set on it is accepted,
        // reads back, and terminates nothing.
        if !no_process_eos {
            base.install_octet_interpose(Box::new(crate::interpose::eos::EosInterpose::default()));
        }
    }

    /// Create the socket C's `connectIt` creates: fresh fd, then every option
    /// the protocol suffix asked for — `SO_BROADCAST` (drvAsynIPPort.c:448-459)
    /// and `SO_REUSEPORT` (:461-477) — applied **before** the socket is bound or
    /// connected, because that is the only time the kernel honours them. Binding
    /// first and setting `SO_REUSEPORT` afterwards, as this driver used to, left
    /// `udp&` with a local port failing `EADDRINUSE` where C binds.
    ///
    /// Single owner of socket creation for both transports, so a new option
    /// cannot be added to one and forgotten on the other.
    ///
    /// C picks SO_REUSEADDR when `USE_SO_REUSEADDR` is defined for the target
    /// and SO_REUSEPORT otherwise (:465-469) — exactly *one* of the pair, unlike
    /// the datagram-fanout helper `drvAsynIPServerPort` uses, which sets both.
    /// Which one is the platform's business, and
    /// [`socket::SocketOptions::reuse_port`] carries C's own
    /// `#ifndef SO_REUSEPORT` degrade rule for it.
    fn socket_options(&self) -> socket::SocketOptions {
        socket::SocketOptions {
            broadcast: self.config.protocol.broadcast(),
            reuse_port: self.config.protocol.reuse_port(),
            reuse_address: false,
        }
    }

    fn connect_tcp(&mut self) -> AsynResult<TcpStream> {
        use std::net::ToSocketAddrs;
        let addr_str = format!("{}:{}", self.config.host, self.config.port);
        // Resolve the remote first: C delays the lookup to connect time too, in
        // case the device has just appeared in DNS (drvAsynIPPort.c:479-493).
        let addrs: Vec<std::net::SocketAddr> = addr_str
            .to_socket_addrs()
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("failed to resolve '{addr_str}': {e}"),
            })?
            .collect();

        let mut last_err: Option<AsynError> = None;
        for remote_addr in &addrs {
            // C binds the local address only when one was configured — "a very
            // unusual configuration" (:495-506).
            let local_addr: Option<std::net::SocketAddr> =
                self.config.local_port.map(|local_port| {
                    if remote_addr.is_ipv6() {
                        (std::net::Ipv6Addr::UNSPECIFIED, local_port).into()
                    } else {
                        (std::net::Ipv4Addr::UNSPECIFIED, local_port).into()
                    }
                });
            let mut opts = self.socket_options();
            // Not C's: a fixed local port would otherwise be unusable for the
            // TIME_WAIT lifetime of the previous connection. Only meaningful
            // when a local bind actually happens.
            opts.reuse_address = local_addr.is_some();
            match socket::tcp_connect(*remote_addr, local_addr, opts, self.config.connect_timeout) {
                Ok(stream) => return Ok(stream),
                Err(e) => last_err = Some(AsynError::Io(e)),
            }
        }
        Err(last_err.unwrap_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("no addresses found for '{addr_str}'"),
        }))
    }

    fn connect_udp(&mut self) -> AsynResult<(UdpSocket, std::net::SocketAddr)> {
        use std::net::ToSocketAddrs;
        // C drvAsynIPPort.c::connectIt (484-493) resolves the remote name to
        // tty->farAddr but (513) does NOT connect() a SOCK_DGRAM socket.
        // Resolve the peer once, then leave the socket unconnected and bind
        // a local endpoint of the peer's address family.
        let remote = format!("{}:{}", self.config.host, self.config.port);
        let peer = remote
            .to_socket_addrs()
            .map_err(|e| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("UDP resolve '{remote}': {e}"),
            })?
            .next()
            .ok_or_else(|| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("UDP resolve '{remote}': no addresses"),
            })?;
        let local_port = self.config.local_port.unwrap_or(0);
        let local_addr: std::net::SocketAddr = if peer.is_ipv6() {
            (std::net::Ipv6Addr::UNSPECIFIED, local_port).into()
        } else {
            (std::net::Ipv4Addr::UNSPECIFIED, local_port).into()
        };
        // The options go on the fresh socket, before the bind — the whole point
        // of `udp&` is two ports sharing one local port, and the kernel only
        // honours SO_REUSEPORT on an unbound socket (C :461-477, then :495-506).
        // That ordering is the seam's contract, not this call site's.
        let socket = socket::udp_socket(local_addr, self.socket_options()).map_err(|e| {
            AsynError::Status {
                status: AsynStatus::Error,
                message: format!("UDP bind '{local_addr}' failed: {e}"),
            }
        })?;
        Ok((socket, peer))
    }

    #[cfg(asyn_af_unix)]
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

    /// C drvAsynIPPort registers asynCommon, asynOption and asynOctet
    /// (drvAsynIPPort.c:1037-1053) — no register interface. A record with
    /// IFACE=Int32/UInt32/Float64 on this port gets C's `No asyn<X> interface`
    /// (asynRecord.c:1336-1358), not a silent parameter-cache read.
    fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
        crate::interfaces::octet_transport_capabilities()
    }

    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        // C drvAsynIPPort.c::connectIt (424-427): reject a connect on an
        // already-open link ("Link already open!") rather than opening a
        // second socket and leaking the first.
        if self.io.inner.is_some() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("{}: Link already open!", self.base.port_name),
            });
        }
        match self.config.protocol {
            // C :364-367 — `com` leaves `socketType = SOCK_STREAM` and sets no
            // flags, so its transport is plain TCP; the whole of what `COM` adds
            // rides in the interpose, not the socket.
            IpProtocol::Tcp | IpProtocol::TcpReusePort | IpProtocol::Com => {
                let stream = self.connect_tcp()?;
                if self.config.no_delay {
                    stream.set_nodelay(true)?;
                }
                // `tcp&` = TCP + SO_REUSEPORT (C :360-363, :461-477). The option
                // is set by `new_socket` on the unconnected socket, which is the
                // only place it means anything.
                self.io.inner = Some(IpIoInner::Tcp(stream));
            }
            IpProtocol::Udp
            | IpProtocol::UdpReusePort
            | IpProtocol::UdpBroadcast
            | IpProtocol::UdpBroadcastReusePort => {
                // BROADCAST / SO_REUSEPORT are the socket's, applied by
                // `new_socket` before the bind — they are not a post-bind fixup.
                let (socket, peer) = self.connect_udp()?;
                self.io.inner = Some(IpIoInner::Udp(socket, peer));
            }
            #[cfg(asyn_af_unix)]
            IpProtocol::Unix => {
                let stream = self.connect_unix()?;
                self.io.inner = Some(IpIoInner::Unix(stream));
            }
            // Unreachable rather than defensive: `IpPortConfig::parse` refuses
            // a `unix://` spec outright on a platform without AF_UNIX, so no
            // config carrying this variant can exist here. The arm is present
            // because `IpProtocol::Unix` is still a variant of the enum and the
            // match must be exhaustive.
            #[cfg(not(asyn_af_unix))]
            IpProtocol::Unix => {
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: "AF_UNIX not available on this platform".into(),
                });
            }
            IpProtocol::Http => {
                // C parity: HTTP uses TCP with connect-per-transaction
                // semantics. The socket is opened here on the first connect
                // and reopened lazily by write_octet/read_octet_core after
                // each transaction's EOF (`drop_connection` releases the
                // socket but keeps the link logically connected). C opens the
                // socket lazily at the first write rather than at connect;
                // opening it eagerly here is a benign divergence (the idle
                // socket is reused by the first write, HTTP/1.0 servers wait
                // for the request).
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
        // C `asynInterposeCom.c::exceptionHandler` (:763-774) runs the RFC 2217
        // handshake on `asynExceptionConnect` — which `set_connected(true)` just
        // fired. No-op unless this is a COM port.
        self.restore_com_settings();
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
        self.read_octet_core(user, buf).map(|(n, _eom)| n)
    }

    fn io_read_octet_eom(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
    ) -> AsynResult<(usize, EomReason)> {
        // Override the synthetic default so the real END/EOS reason from the
        // base read + interpose chain reaches the actor (C reports eomReason
        // from readRaw; END marks a TCP EOF, EOS an input-EOS match).
        self.read_octet_core(user, buf)
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        // HTTP connect-per-transaction: (re)open the socket if there isn't
        // one. Like the read path, gate on socket presence — an HTTP link
        // stays logically connected across transactions, so the lazy reopen
        // keys off `io.inner.is_none()`, not `!connected` (which would never
        // fire post-EOF since `connected` is left `true`). This is the C
        // `writeRaw` lazy `connectIt` on `fd == INVALID_SOCKET`
        // (drvAsynIPPort.c:590-606). Surface the connect failure cause
        // rather than masking it.
        if self.config.protocol == IpProtocol::Http && self.io.inner.is_none() {
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
        match self.with_base_link(|link| link.write(user, data)) {
            Ok(n) => Ok(n),
            Err(e) => {
                // C parity: drvAsynIPPort.c::writeIt closes the connection
                // on a real send error (ECONNRESET/EPIPE) so the next
                // request reconnects. The read path already tears down on a
                // fatal error; without the symmetric write-side teardown a
                // wedged socket reports `connected` forever and never
                // self-heals.
                if e.is_fatal_transport() && self.base.is_connected() {
                    asyn_trace!(
                        Some(self.base.trace),
                        &self.base.port_name,
                        TraceMask::FLOW,
                        "write error, disconnecting: {e}"
                    );
                    self.drop_connection();
                }
                Err(e)
            }
        }
    }

    fn io_flush(&mut self, user: &mut AsynUser) -> AsynResult<()> {
        // The driver end of the flush: drain the OS socket's receive buffer
        // (C `drvAsynIPPort.c::flushIt`). The layers above — the EOS interpose's
        // read-ahead buffer among them, C `asynInterposeEos.c::flushIt` resetting
        // `inBufHead`/`inBufTail`/`eosInMatch` — are flushed by the port before
        // it reaches here ([`crate::port::octet_flush_chain`]), which is what
        // stops a byte buffered *inside* the interpose from leaking into the next
        // `OctetWriteRead` response.
        self.with_base_link(|link| link.flush(user))
    }

    fn set_option(&mut self, user: &mut AsynUser, key: &str, value: &str) -> AsynResult<()> {
        // C interposes `asynInterposeCOM`'s asynOption interface *above* the IP
        // driver's (`asynInterposeCom.c:809-824`), so a COM port's option writes
        // hit the negotiation first and only an unclaimed key reaches the driver
        // below (`setOption`'s trailing else, :645-652). Same order here: the
        // seven serial keys negotiate, `hostInfo`/`disconnectOnReadTimeout` fall
        // through to the IP chain — which is why this is one dispatch and not two
        // independent option maps.
        if self.com.is_some() && ComPortOptions::owns_key(key) {
            // Same teardown gate as the connect-time handshake: C's setOption
            // reads run through the same `readIt`/`readRaw` below the interpose.
            return self
                .with_negotiation(|com, link| com.set_option(user, link, key, value))
                .expect("com is Some");
        }
        // C `drvAsynIPPort.c::setOption`/`getOption` compare option keys
        // with `epicsStrCaseCmp` (case-insensitive). Match that here: the
        // asynRecord writes the IP keys lowercase (`hostinfo`, see
        // asyn_record/mod.rs), and iocsh callers may use any case, so a
        // case-sensitive `match` would silently route them to the generic
        // option map and skip the real handler — leaving the live socket
        // configured for the old endpoint. The keys form an if/else chain
        // exactly like C's `epicsStrCaseCmp` cascade.
        if key.eq_ignore_ascii_case("noDelay") {
            // `noDelay` has no counterpart in C drvAsynIPPort.c::setOption
            // (which recognizes only `disconnectOnReadTimeout`/`hostInfo`):
            // C enables TCP_NODELAY unconditionally in connectIt (525-528)
            // with no runtime toggle. This key is a deliberate Rust extension
            // (default on via IpPortConfig::parse, matching C; settable off
            // here). Its value is validated strictly Y/N (case-insensitive)
            // like every other option value, so a typo errors instead of
            // silently coercing to "off" — and it reports in C's words
            // ("Invalid <key> value."), like every other option.
            let enabled = parse_yn_option("noDelay", value)?;
            self.config.no_delay = enabled;
            if let Some(IpIoInner::Tcp(ref stream)) = self.io.inner {
                stream.set_nodelay(enabled)?;
            }
        } else if key.eq_ignore_ascii_case("disconnectOnReadTimeout") {
            // C drvAsynIPPort.c::setOption (924-935): only "Y"/"N"
            // (case-insensitive) are valid; any other value returns
            // asynError "Invalid disconnectOnReadTimeout value." rather
            // than silently coercing the unknown text to "off".
            self.disconnect_on_read_timeout = parse_yn_option("disconnectOnReadTimeout", value)?;
        } else if key.eq_ignore_ascii_case("hostInfo") {
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
            // connect_timeout / no_delay are not re-applied on a hostInfo
            // reparse: C parseHostInfo touches neither (no_delay is set
            // unconditionally in connectIt; C has no connect-timeout option
            // at all — see DEFAULT_CONNECT_TIMEOUT / DRV-13).
            //
            // C parseHostInfo replaces tty->IPDeviceName with
            // epicsStrDup(hostInfo); store the verbatim spec so a later
            // getOption("hostInfo") echoes the new endpoint, not the old.
            self.host_info = value.to_string();
        } else if !key.is_empty() {
            // C drvAsynIPPort.c::setOption (lines 941-945): any non-empty
            // unsupported key returns asynError "Unsupported key"; the empty
            // key is a silent no-op (the `epicsStrCaseCmp(key,"") != 0`
            // guard). The real handlers above own every supported key, so
            // there is no generic option store to fall into.
            return Err(AsynError::OptionNotFound(key.to_string()));
        }
        Ok(())
    }

    /// Read an IP-driver option. Mirrors C `drvAsynIPPort.c::getOption`
    /// (lines 888-913): `disconnectOnReadTimeout` -> `"Y"`/`"N"`,
    /// `hostInfo` -> the live device spec, both matched case-insensitively
    /// (`epicsStrCaseCmp`). Without this override the base default reads
    /// the generic option map, which the real `set_option` handlers above
    /// never populate, so the asynRecord could never read back the live
    /// `HOSTINFO`/`DRTO`. Unknown keys fall through to the generic map.
    fn get_option(&self, key: &str) -> AsynResult<String> {
        // Same interposed order as `set_option`. C's COM `getOption` (:657-725)
        // answers from cached state and touches the wire not at all, so this
        // needs no `&mut self` — the settings it reports are what the *server*
        // last acknowledged, refreshed on every `set_option` and reconnect.
        if let Some(com) = self.com.as_ref() {
            if ComPortOptions::owns_key(key) {
                return com.options.get_option(key);
            }
        }
        if key.eq_ignore_ascii_case("disconnectOnReadTimeout") {
            Ok(if self.disconnect_on_read_timeout {
                "Y"
            } else {
                "N"
            }
            .to_string())
        } else if key.eq_ignore_ascii_case("hostInfo") {
            Ok(self.host_info.clone())
        } else {
            self.base
                .options
                .get(key)
                .cloned()
                .ok_or_else(|| AsynError::OptionNotFound(key.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// Enter the port's octet interface the way a caller does — through the
    /// interpose chain, with the driver at the bottom of it. C `findInterface`
    /// hands the caller the *topmost* interpose (asynManager.c:1493-1501,
    /// 2190-2220); the driver's own `read`/`write`/`flush` are the base the chain
    /// ends at, so a test that wants the EOS layer must enter above it, exactly
    /// as `PortActor` does.
    fn chain_read(drv: &mut DrvAsynIPPort, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        crate::port::octet_read_chain(drv, user, buf).map(|(n, _eom)| n)
    }

    /// Gated with its only caller,
    /// `eos_pushed_after_com_sits_above_it_and_sees_unstuffed_bytes`, which
    /// needs `crate::iocsh` and so exists only under `epics`.
    #[cfg(feature = "epics")]
    fn chain_read_eom(
        drv: &mut DrvAsynIPPort,
        user: &AsynUser,
        buf: &mut [u8],
    ) -> AsynResult<(usize, EomReason)> {
        crate::port::octet_read_chain(drv, user, buf)
    }

    fn chain_flush(drv: &mut DrvAsynIPPort, user: &mut AsynUser) -> AsynResult<()> {
        crate::port::octet_flush_chain(drv, user)
    }

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
        assert!(!drv.base().is_connected());
        assert!(drv.base().auto_connect);
        assert!(drv.base().flags.can_block);
    }

    /// `new_configured(..., noAutoConnect=1, noProcessEos=0)` — the differential
    /// oracle's `drvAsynIPPortConfigure("ORACLEASYN","localhost:1",0,1,0)`
    /// equivalent — must yield a port that (1) advertises exactly the
    /// octet-transport interface set (asynCommon + asynOption + asynOctet, so
    /// OCTETIV=OPTIONIV=1 and GPIB/I32/UI32/F64 IV=0), (2) starts
    /// disconnected and never auto-dials, and (3) answers HOSTINFO="localhost:1"
    /// / DRTO="N" the way C's real IP port does. A regression on any of these
    /// reopens the asyn read fields the oracle compares.
    #[test]
    fn new_configured_no_auto_connect_matches_the_oracle_port_model() {
        let drv = DrvAsynIPPort::new_configured("ORACLEASYN", "localhost:1", true, false).unwrap();

        // (1) Interface set — the exact octet-transport capabilities, no extras.
        assert_eq!(
            drv.capabilities(),
            crate::interfaces::octet_transport_capabilities()
        );

        // (2) Disconnected, noAutoConnect — nothing arms the reconnect timer,
        // so the port stays permanently disconnected (CNCT/AUCT read 0).
        assert!(!drv.base().is_connected());
        assert!(!drv.base().auto_connect);

        // (3) The IP options the asyn record renders as HOSTINFO / DRTO.
        assert_eq!(drv.get_option("hostinfo").unwrap(), "localhost:1");
        assert_eq!(drv.get_option("disconnectOnReadTimeout").unwrap(), "N");
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
        assert!(!drv.base().is_connected());

        drv.connect(&user).unwrap();
        assert!(drv.base().is_connected());

        drv.disconnect(&user).unwrap();
        assert!(!drv.base().is_connected());
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
    fn http_multi_segment_response_not_truncated() {
        // DRV-7: HTTP is connect-per-transaction, but the response ends on
        // the server's EOF — not after the first read. The old code dropped
        // the connection after *every* HTTP read, so a response read in
        // chunks lost everything past the first chunk (the next read
        // reconnected to a fresh socket). Fixed: the socket stays up until
        // EOF, the logical connection never flaps, and the socket reopens
        // lazily for the next transaction.
        let (listener, port) = start_echo_server();
        let handle = thread::spawn(move || {
            // Transaction 1: read request, send an 8-byte response, close.
            let (mut s1, _) = listener.accept().unwrap();
            let mut req = [0u8; 64];
            let _ = s1.read(&mut req).unwrap();
            s1.write_all(b"AAAABBBB").unwrap();
            drop(s1); // server close -> client's third read sees EOF
            // Transaction 2: a fresh connection (the lazy reopen), send 4 B.
            let (mut s2, _) = listener.accept().unwrap();
            let _ = s2.read(&mut req).unwrap();
            s2.write_all(b"CCCC").unwrap();
            drop(s2);
        });

        let mut drv = DrvAsynIPPort::new("httptest", &format!("127.0.0.1:{port} HTTP")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        assert!(drv.base().is_connected());

        // --- Transaction 1: write request, read the response in 4-B chunks.
        let mut wuser = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut wuser, b"GET /1\r\n").unwrap();

        let ruser = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 4];
        let (n1, eom1) = drv.io_read_octet_eom(&ruser, &mut buf).unwrap();
        assert_eq!(&buf[..n1], b"AAAA");
        assert!(!eom1.contains(EomReason::END), "first chunk is not EOF");
        // The bug tore the socket down here; the fix keeps it up so the rest
        // of the response is still readable.
        assert!(
            drv.base().is_connected(),
            "HTTP must stay connected mid-response (no truncation)"
        );
        assert!(
            drv.io.inner.is_some(),
            "socket must remain open mid-response"
        );

        let (n2, _) = drv.io_read_octet_eom(&ruser, &mut buf).unwrap();
        assert_eq!(
            &buf[..n2],
            b"BBBB",
            "second chunk must be the rest of the response"
        );

        // Third read hits the server's EOF -> END; the socket is released but
        // the logical connection stays up (connect-per-transaction = no flap).
        let (n3, eom3) = drv.io_read_octet_eom(&ruser, &mut buf).unwrap();
        assert_eq!(n3, 0);
        assert!(eom3.contains(EomReason::END));
        assert!(
            drv.base().is_connected(),
            "HTTP logical connection must not flap on per-transaction EOF"
        );
        assert!(drv.io.inner.is_none(), "socket released after EOF");

        // --- Transaction 2: a new write lazily reopens the socket.
        drv.write_octet(&mut wuser, b"GET /2\r\n").unwrap();
        assert!(
            drv.io.inner.is_some(),
            "socket must reopen lazily for the next transaction"
        );
        let mut buf2 = [0u8; 16];
        let (n4, _) = drv.io_read_octet_eom(&ruser, &mut buf2).unwrap();
        assert_eq!(&buf2[..n4], b"CCCC");

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
        // C drvAsynIPPort.c::readRaw (815-821): a TCP EOF returns success
        // with zero bytes and ASYN_EOM_END, then closes the connection.
        let (n, eom) = drv.io_read_octet_eom(&user, &mut buf).unwrap();
        assert_eq!(n, 0);
        assert!(eom.contains(EomReason::END));
        // closeConnection ran, so the actor's reconnect can re-open it.
        assert!(!drv.base().is_connected());

        handle.join().unwrap();
    }

    #[test]
    fn test_is_fatal_transport_error_classification() {
        // DRV-5/DRV-31 family: a broken-socket error tears the connection
        // down; a timeout leaves it intact (the actor reconnects on the
        // next request only when `connected` flips to false).
        assert!(
            AsynError::Status {
                status: AsynStatus::Disconnected,
                message: "EOF".into(),
            }
            .is_fatal_transport()
        );
        assert!(
            AsynError::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "rst"
            ))
            .is_fatal_transport()
        );
        assert!(
            !AsynError::Status {
                status: AsynStatus::Timeout,
                message: "read timeout".into(),
            }
            .is_fatal_transport()
        );
    }

    #[test]
    fn test_write_error_disconnects() {
        // DRV-5: a fatal write error must tear down the connection so the
        // actor's auto-reconnect fires — symmetric with the read path,
        // which already disconnects on a fatal error.
        let (listener, port) = start_echo_server();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            drop(stream); // peer closes → our later writes get RST/EPIPE
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        assert!(drv.base().is_connected());
        handle.join().unwrap();
        thread::sleep(Duration::from_millis(50));

        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let mut last: AsynResult<usize> = Ok(0);
        for _ in 0..200 {
            last = drv.write_octet(&mut user, b"ping\n");
            if last.is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(last.is_err(), "expected a write to the dead peer to fail");
        assert!(
            !drv.base().is_connected(),
            "DRV-5: fatal write error must set connected=false"
        );
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

    /// R8-57 end-to-end over a real socket: connecting a `COM` port must run C's
    /// RFC 2217 handshake (`asynInterposeCom.c::restoreSettings`, :729-758) on
    /// the wire, and the data path that follows must be IAC-escaped in both
    /// directions (`writeIt`/`readIt`, :136-245).
    ///
    /// The fake terminal server below answers the handshake with the standard
    /// echoes, then behaves like a device: it receives one IAC-stuffed write and
    /// sends back one IAC-stuffed reply.
    #[test]
    fn com_port_negotiates_rfc2217_on_connect_and_escapes_the_data_path() {
        use crate::interpose::com::{IAC, SB, SE};

        const COM_PORT_OPTION: u8 = 44;
        const WILL_B: u8 = 251;
        const DO_B: u8 = 253;
        /// The server's acknowledgement of subcommand `n`: `IAC SB 44 n+100 …`.
        fn ack(subcmd: u8, values: &[u8]) -> Vec<u8> {
            let mut v = vec![IAC, SB, COM_PORT_OPTION, subcmd + 100];
            v.extend_from_slice(values);
            v.extend_from_slice(&[IAC, SE]);
            v
        }
        /// A client subnegotiation: `IAC SB 44 <payload> IAC SE`.
        fn sb(payload: &[u8]) -> Vec<u8> {
            let mut v = vec![IAC, SB, COM_PORT_OPTION];
            v.extend_from_slice(payload);
            v.extend_from_slice(&[IAC, SE]);
            v
        }

        let (listener, port) = start_echo_server();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();

            // Blast the whole handshake reply first — the client drives the
            // exchange step by step, reading each answer before sending the next,
            // so queueing all the answers up front is what keeps this from
            // deadlocking. In C's order: DO BINARY -> WILL, WILL BINARY -> DO,
            // WILL COM-PORT-OPTION -> DO, then SET-MODEMSTATE-MASK and the six
            // settings, each echoed back at its default.
            let mut reply = vec![IAC, WILL_B, 0, IAC, DO_B, 0, IAC, DO_B, COM_PORT_OPTION];
            reply.extend_from_slice(&ack(11, &[0])); // SET-MODEMSTATE-MASK
            reply.extend_from_slice(&ack(1, &[0x00, 0x00, 0x25, 0x80])); // 9600 baud
            reply.extend_from_slice(&ack(2, &[8])); // 8 data bits
            reply.extend_from_slice(&ack(3, &[1])); // parity none
            reply.extend_from_slice(&ack(4, &[1])); // 1 stop bit
            reply.extend_from_slice(&ack(5, &[1])); // crtscts -> NOFLOW
            reply.extend_from_slice(&ack(5, &[1])); // ixon    -> NOFLOW
            stream.write_all(&reply).unwrap();
            stream.flush().unwrap();

            // What the client must have put on the wire, byte for byte: C
            // `restoreSettings` (asynInterposeCom.c:740-756) on a default port.
            let mut want = vec![
                IAC,
                DO_B,
                0, // :740  IAC DO  BINARY
                IAC,
                WILL_B,
                0, // :742  IAC WILL BINARY
                IAC,
                WILL_B,
                COM_PORT_OPTION, // :744  IAC WILL COM-PORT-OPTION
            ];
            want.extend_from_slice(&sb(&[11, 0])); // SET-MODEMSTATE-MASK 0
            want.extend_from_slice(&sb(&[1, 0x00, 0x00, 0x25, 0x80])); // 9600, big-endian
            want.extend_from_slice(&sb(&[2, 8]));
            want.extend_from_slice(&sb(&[3, 1]));
            want.extend_from_slice(&sb(&[4, 1]));
            want.extend_from_slice(&sb(&[5, 1])); // crtscts "N" -> current mode
            want.extend_from_slice(&sb(&[5, 1])); // ixon    "N" -> current mode
            let mut got = vec![0u8; want.len()];
            stream.read_exact(&mut got).unwrap();
            assert_eq!(got, want, "connect must emit C's RFC 2217 handshake");

            // Now the device write. The caller sent 3 bytes containing one IAC,
            // so 4 must arrive: the IAC is doubled on the wire.
            let mut got = [0u8; 4];
            stream.read_exact(&mut got).unwrap();
            assert_eq!(got, [b'A', IAC, IAC, b'B'], "device write must be stuffed");

            // Reply with a stuffed IAC of our own.
            stream.write_all(&[b'X', IAC, IAC, b'Y']).unwrap();
            stream.flush().unwrap();
            thread::sleep(Duration::from_millis(200));
        });

        let mut drv = DrvAsynIPPort::new("comtest", &format!("127.0.0.1:{port} COM")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        // The handshake ran and the server's echoes landed in the cached state.
        assert_eq!(drv.get_option("baud").unwrap(), "9600");
        assert_eq!(drv.get_option("bits").unwrap(), "8");
        assert_eq!(drv.get_option("parity").unwrap(), "none");
        assert_eq!(drv.get_option("stop").unwrap(), "1");

        // Data path out: 3 caller bytes, one of them an IAC. The interpose
        // reports the caller's count, not the 4 bytes it put on the wire.
        let mut wuser = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let n = drv.write_octet(&mut wuser, &[b'A', IAC, b'B']).unwrap();
        assert_eq!(n, 3);

        // Data path in: 4 wire bytes unstuff to 3.
        let ruser = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 16];
        let n = drv.read_octet(&ruser, &mut buf).unwrap();
        assert_eq!(&buf[..n], &[b'X', IAC, b'Y']);

        server.join().unwrap();
    }

    /// R8-57: an option write on a live COM port must reach the wire as a
    /// COM-PORT-OPTION subnegotiation — C routes `setOption` through the
    /// interposed asynOption interface (asynInterposeCom.c:809-824), not into
    /// the IP driver's option map.
    #[test]
    fn set_option_on_a_live_com_port_negotiates_with_the_server() {
        use crate::interpose::com::{IAC, SB, SE};

        let (listener, port) = start_echo_server();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            // Refuse the opening handshake so connect() takes the "report and
            // carry on" path (C exceptionHandler, :770-772) and the link stays
            // up — this test is about the later setOption, not the handshake.
            stream.write_all(&[IAC, 252, 0]).unwrap(); // IAC WONT BINARY
            stream.flush().unwrap();

            // Drain the one handshake frame the client got out before the
            // refusal stopped it: IAC DO BINARY.
            let mut hs = [0u8; 3];
            stream.read_exact(&mut hs).unwrap();
            assert_eq!(hs, [IAC, 253, 0]);

            // Now answer one SET-BAUDRATE with the rate applied.
            let mut req = [0u8; 10];
            stream.read_exact(&mut req).unwrap();
            assert_eq!(
                req,
                [IAC, SB, 44, 1, 0x00, 0x01, 0xC2, 0x00, IAC, SE],
                "115200 baud must go out as a big-endian SET-BAUDRATE"
            );
            let mut ack = vec![IAC, SB, 44, 101, 0x00, 0x01, 0xC2, 0x00];
            ack.extend_from_slice(&[IAC, SE]);
            stream.write_all(&ack).unwrap();
            stream.flush().unwrap();
            thread::sleep(Duration::from_millis(200));
        });

        let mut drv = DrvAsynIPPort::new("comtest2", &format!("127.0.0.1:{port} COM")).unwrap();
        let user = AsynUser::default();
        // A refused handshake does not fail the connect — C only prints.
        drv.connect(&user).unwrap();
        assert!(drv.base.is_connected());

        drv.set_option(&mut AsynUser::default(), "baud", "115200")
            .unwrap();
        assert_eq!(drv.get_option("baud").unwrap(), "115200");

        server.join().unwrap();
    }

    /// R8-57 boundary: C runs the negotiation's reads through
    /// `readIt`/`readRaw`, so `readRaw`'s teardown gate (drvAsynIPPort.c:798-806)
    /// governs them too — a negotiation read that times out with
    /// `disconnectOnReadTimeout=Y` closes the socket. Driving the raw link would
    /// have silently skipped that.
    ///
    /// Both sides of the gate, since it is a conjunction: option OFF leaves the
    /// socket up (the handshake merely fails and is logged); option ON closes it.
    ///
    /// The server must stay alive and keep *draining* past the client's 2 s
    /// negotiation timeout. Closing early with the client's handshake bytes still
    /// unread would send an RST, and an RST tears the socket down through the
    /// other disjunct (`is_fatal_transport`) no matter how the option is set —
    /// which would pass the ON case for the wrong reason and fail the OFF case.
    #[test]
    fn a_timed_out_negotiation_read_honours_disconnect_on_read_timeout() {
        /// A server that answers nothing but keeps draining, so the client's
        /// negotiation read expires as a genuine timeout.
        fn silent_server() -> (u16, thread::JoinHandle<()>) {
            let (listener, port) = start_echo_server();
            let handle = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                let deadline = std::time::Instant::now() + Duration::from_millis(2600);
                let mut scratch = [0u8; 64];
                while std::time::Instant::now() < deadline {
                    match stream.read(&mut scratch) {
                        Ok(0) => break,
                        Ok(_) => continue,
                        Err(_) => continue, // our own 100 ms poll expiring
                    }
                }
            });
            (port, handle)
        }

        // Option OFF (C's default) — the handshake fails, the link survives.
        let (port, server) = silent_server();
        let mut drv = DrvAsynIPPort::new("comto1", &format!("127.0.0.1:{port} COM")).unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        assert!(
            drv.base.is_connected(),
            "handshake failure alone must not close the socket (C only asynPrints)"
        );
        assert!(drv.io.inner.is_some());
        server.join().unwrap();

        // Option ON — the same silent server now costs the socket, because C
        // applies readRaw's teardown to the negotiation's reads too.
        let (port, server) = silent_server();
        let mut drv = DrvAsynIPPort::new("comto2", &format!("127.0.0.1:{port} COM")).unwrap();
        drv.set_option(&mut AsynUser::default(), "disconnectOnReadTimeout", "Y")
            .unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        assert!(
            !drv.base.is_connected(),
            "a timed-out negotiation read must close the socket, as C readRaw does"
        );
        assert!(drv.io.inner.is_none());
        server.join().unwrap();
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
        drv.install_interpose(Box::new(eos));

        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 32];
        let n = chain_read(&mut drv, &user, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"OK");

        handle.join().unwrap();
    }

    /// R7-47: `disconnectOnReadTimeout Y` must tear the socket down on a read
    /// timeout even when the EOS interpose is installed — which
    /// `drvAsynIPPortConfigure` does by default (iocsh.rs), so this is the
    /// *normal* configuration, not a corner case. C runs its disconnect test
    /// inside readRaw, below the interpose, on the raw recv timeout
    /// (drvAsynIPPort.c:798-806); asyn-rs runs it above the interpose, where
    /// the same timeout arrives wrapped as `PartialRead` — so the test must
    /// key off `e.status()`, not the error variant. Matching by variant made
    /// the option inert on every default IP port.
    #[test]
    fn disconnect_on_read_timeout_fires_through_the_eos_interpose() {
        use crate::interpose::eos::{EosConfig, EosInterpose};

        let (listener, port) = start_echo_server();
        // The peer accepts and then says nothing: the read must time out with
        // zero bytes, which the EOS interpose reports as PartialRead(Timeout, 0).
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(300));
            drop(stream);
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        drv.install_interpose(Box::new(EosInterpose::new(EosConfig {
            input_eos: vec![b'\n'],
            output_eos: vec![],
        })));
        drv.set_option(&mut AsynUser::default(), "disconnectOnReadTimeout", "Y")
            .unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        assert!(drv.base().is_connected());

        let user = AsynUser::new(0).with_timeout(Duration::from_millis(50));
        let mut buf = [0u8; 32];
        let err = drv
            .read_octet(&user, &mut buf)
            .expect_err("a silent peer must time the read out");
        // R11-47: the branch that closes the socket is the branch that reports
        // asynError (drvAsynIPPort.c:805) — the lower layer's asynTimeout does
        // not survive the teardown.
        assert_eq!(
            err.status(),
            AsynStatus::Error,
            "C readRaw overwrites the status on the disconnect branch"
        );
        assert!(
            err.message().contains("read error"),
            "C's teardown text (drvAsynIPPort.c:801-803), got {:?}",
            err.message()
        );
        assert!(
            !drv.base().is_connected(),
            "disconnectOnReadTimeout must drop the connection (C drvAsynIPPort.c:798-806)"
        );

        handle.join().unwrap();
    }

    /// Boundary companion to the test above: `disconnectOnReadTimeout` is off
    /// by default, so the same timeout through the same interpose must leave
    /// the socket up.
    #[test]
    fn read_timeout_without_the_option_keeps_the_connection() {
        use crate::interpose::eos::{EosConfig, EosInterpose};

        let (listener, port) = start_echo_server();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(300));
            drop(stream);
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        drv.install_interpose(Box::new(EosInterpose::new(EosConfig {
            input_eos: vec![b'\n'],
            output_eos: vec![],
        })));
        drv.connect(&AsynUser::default()).unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_millis(50));
        let mut buf = [0u8; 32];
        let err = drv.read_octet(&user, &mut buf).unwrap_err();
        // R11-47 negative control: the non-teardown branch keeps C's asynTimeout
        // (drvAsynIPPort.c:811) — the restamp is the teardown branch's, not every
        // failed read's.
        assert_eq!(err.status(), AsynStatus::Timeout);
        assert!(
            drv.base().is_connected(),
            "a plain read timeout leaves the socket intact (C returns asynTimeout, no close)"
        );

        handle.join().unwrap();
    }

    /// R11-47: the restamp keeps the bytes the device did send. C assigns
    /// `*nbytesTransfered = thisRead` (drvAsynIPPort.c:824) *below* both branches,
    /// so a device that emits half a line and goes quiet reaches the record as
    /// asynError **plus** its partial line — asynRecord commits NORD/EOMR
    /// regardless of status (asynRecord.c:1591,1627).
    #[test]
    fn a_torn_down_read_reports_asyn_error_and_still_carries_the_partial_bytes() {
        use crate::interpose::PartialOctetRead;

        let timeout = Duration::from_millis(50);
        let partial = PartialOctetRead {
            data: b"OK\n".to_vec(),
            eom_reason: EomReason::empty(),
        };
        let timed_out = AsynError::Status {
            status: AsynStatus::Timeout,
            message: "read timeout".into(),
        }
        .with_partial_read(partial);

        let f = classify_read_failure(timed_out, true, timeout, "127.0.0.1:5001");
        assert!(f.disconnect, "disconnectOnReadTimeout + timeout > 0");
        assert_eq!(f.error.status(), AsynStatus::Error, "C :805");
        assert_eq!(
            f.error.message(),
            "127.0.0.1:5001 read error: read timeout",
            "C :801-803"
        );
        assert_eq!(
            f.error.partial_read().map(|p| p.nbytes_transferred()),
            Some(3),
            "the transfer C writes out at :824 survives the restamp"
        );
    }

    /// R11-47 boundary: the three inputs of C's `should_disconnect` disjunct, each
    /// at its own boundary, and the status each produces.
    #[test]
    fn the_read_failure_rule_matches_c_should_disconnect_at_every_boundary() {
        let timeout = || AsynError::Status {
            status: AsynStatus::Timeout,
            message: "read timeout".into(),
        };
        let fatal = || AsynError::Io(std::io::Error::other("cable yanked"));
        let dev = "host:1";

        // option off → no teardown, status unchanged, whatever the timeout.
        let f = classify_read_failure(timeout(), false, Duration::from_secs(1), dev);
        assert!(!f.disconnect);
        assert_eq!(f.error.status(), AsynStatus::Timeout);

        // option on but timeout == 0 (a poll) → C's `pasynUser->timeout > 0`
        // fails: no teardown, asynTimeout stands.
        let f = classify_read_failure(timeout(), true, Duration::ZERO, dev);
        assert!(!f.disconnect);
        assert_eq!(f.error.status(), AsynStatus::Timeout);

        // option on, timeout > 0 → teardown + asynError.
        let f = classify_read_failure(timeout(), true, Duration::from_millis(1), dev);
        assert!(f.disconnect);
        assert_eq!(f.error.status(), AsynStatus::Error);

        // a real errno tears down with the option off — C's second disjunct.
        let f = classify_read_failure(fatal(), false, Duration::ZERO, dev);
        assert!(f.disconnect);
        assert_eq!(f.error.status(), AsynStatus::Error);
        assert_eq!(f.error.message(), "host:1 read error: IO: cable yanked");
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
        drv.set_option(&mut AsynUser::default(), "noDelay", "Y")
            .unwrap();
        assert!(drv.config.no_delay);
        drv.set_option(&mut AsynUser::default(), "noDelay", "n")
            .unwrap();
        assert!(!drv.config.no_delay);
        // Value is validated strictly Y/N (case-insensitive); the old loose
        // coercion silently treated any other token (incl. "0"/"1") as off.
        assert!(
            drv.set_option(&mut AsynUser::default(), "noDelay", "1")
                .is_err()
        );
        assert!(
            drv.set_option(&mut AsynUser::default(), "noDelay", "maybe")
                .is_err()
        );
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
        assert!(drv.base().is_connected());

        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user, b"ping").unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 32];
        let n = drv.read_octet(&user, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"ping");

        handle.join().unwrap();
    }

    #[test]
    fn test_udp_accepts_reply_from_any_peer() {
        // DRV-1: the datagram socket is left unconnected (C connectIt does
        // not connect() a SOCK_DGRAM socket), so a reply may arrive from a
        // different source address than the request was sent to — a device
        // that answers from another port, or a broadcast request answered by
        // several peers. A connect()-ed socket silently drops these.
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_port = server.local_addr().unwrap().port();

        // Reserve a fixed local port for the driver so the "other peer"
        // below knows where to answer.
        let local_port = UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();

        let handle = thread::spawn(move || {
            let mut buf = [0u8; 256];
            let (n, _src) = server.recv_from(&mut buf).unwrap();
            assert_eq!(&buf[..n], b"ping");
            // Answer from a DIFFERENT socket — a peer we never sent to.
            let other = UdpSocket::bind("127.0.0.1:0").unwrap();
            other
                .send_to(b"pong", format!("127.0.0.1:{local_port}"))
                .unwrap();
        });

        let mut drv = DrvAsynIPPort::new(
            "udptest",
            &format!("127.0.0.1:{server_port}:{local_port} udp"),
        )
        .unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user, b"ping").unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 32];
        let n = drv.read_octet(&user, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"pong");

        handle.join().unwrap();
    }

    #[test]
    fn test_udp_empty_datagram_is_not_eof() {
        // DRV-4: a zero-length UDP datagram is a legitimate read of 0 bytes,
        // not a connection close. C readRaw only treats recv()==0 as EOF for
        // SOCK_STREAM; a DGRAM 0-byte recvfrom leaves the port open.
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_port = server.local_addr().unwrap().port();

        let mut drv =
            DrvAsynIPPort::new("udptest", &format!("127.0.0.1:{server_port} udp")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        // Driver sends first so the server learns its source address.
        let mut wuser = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut wuser, b"hello").unwrap();
        let mut sbuf = [0u8; 16];
        let (_n, src) = server.recv_from(&mut sbuf).unwrap();

        // Reply with an empty datagram.
        server.send_to(&[], src).unwrap();
        let ruser = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 32];
        let n = drv.read_octet(&ruser, &mut buf).unwrap();
        assert_eq!(n, 0);
        // The port must remain open — a 0-byte datagram is not EOF.
        assert!(drv.base().is_connected());

        // And it can still read a subsequent real datagram.
        server.send_to(b"world", src).unwrap();
        let n = drv.read_octet(&ruser, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"world");
    }

    #[test]
    fn test_udp_zero_length_write_sends_nothing() {
        // DRV-14: C writeRaw (613-614) returns success on a zero-length
        // write without sending — in particular no empty UDP datagram.
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let server_port = server.local_addr().unwrap().port();

        let mut drv =
            DrvAsynIPPort::new("udptest", &format!("127.0.0.1:{server_port} udp")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        let mut wuser = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        // A zero-length write succeeds but must not put a datagram on the wire.
        drv.write_octet(&mut wuser, b"").unwrap();
        let mut sbuf = [0u8; 16];
        assert!(
            server.recv_from(&mut sbuf).is_err(),
            "zero-length write must not emit a datagram"
        );

        // A real write still goes through.
        drv.write_octet(&mut wuser, b"data").unwrap();
        let (n, _src) = server.recv_from(&mut sbuf).unwrap();
        assert_eq!(&sbuf[..n], b"data");
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
        drv.set_option(&mut AsynUser::default(), "disconnectOnReadTimeout", "Y")
            .unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        assert!(drv.base().is_connected());

        let user = AsynUser::new(0).with_timeout(Duration::from_millis(50));
        let mut buf = [0u8; 32];
        let _ = drv.read_octet(&user, &mut buf);
        assert!(!drv.base().is_connected());
    }

    #[test]
    fn socket_poll_timeout_floors_zero_to_one_ms() {
        // C drvAsynIPPort.c::readRaw/writeRaw floor `(int)(timeout*1000)==0`
        // to a 1 ms poll; std rejects a zero Duration in set_read_timeout.
        assert_eq!(
            socket_poll_timeout(Duration::ZERO),
            Duration::from_millis(1)
        );
        // Positive timeouts pass through verbatim (incl. sub-ms — Rust is
        // strictly finer than C's whole-ms coarsening).
        assert_eq!(
            socket_poll_timeout(Duration::from_micros(500)),
            Duration::from_micros(500)
        );
        assert_eq!(
            socket_poll_timeout(Duration::from_secs(2)),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn zero_timeout_read_polls_without_disconnect() {
        // C drvAsynIPPort.c::readRaw: a `timeout == 0` read becomes a 1 ms
        // poll (line 742) and the disconnect-on-read-timeout teardown is
        // gated on `timeout > 0` (line 799). So a zero-timeout read of a
        // silent socket must return asynTimeout WITHOUT a hard
        // Duration::ZERO error and WITHOUT tearing the connection down,
        // even with disconnectOnReadTimeout enabled.
        let (listener, port) = start_echo_server();
        let _handle = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_secs(5));
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        drv.set_option(&mut AsynUser::default(), "disconnectOnReadTimeout", "Y")
            .unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        assert!(drv.base().is_connected());

        let user = AsynUser::new(0).with_timeout(Duration::ZERO);
        let mut buf = [0u8; 32];
        let err = drv.read_octet(&user, &mut buf).unwrap_err();
        match err {
            AsynError::Status {
                status: AsynStatus::Timeout,
                ..
            } => {}
            other => panic!("expected Timeout (not a Duration::ZERO error), got {other:?}"),
        }
        // timeout == 0 → C `timeout > 0` guard is false → no teardown.
        assert!(
            drv.base().is_connected(),
            "zero-timeout read must not disconnect even with disconnectOnReadTimeout=Y"
        );
    }

    #[test]
    fn classify_read_error_eintr_and_wouldblock_are_nonfatal_timeout() {
        // C readRaw (798-800) excludes EWOULDBLOCK *and* EINTR from the fatal
        // disjunct, so a signal-interrupted read must be a non-fatal timeout
        // (socket intact), identical to a would-block — never an Io→teardown.
        use std::io::{Error, ErrorKind};
        for kind in [
            ErrorKind::TimedOut,
            ErrorKind::WouldBlock,
            ErrorKind::Interrupted,
        ] {
            let err = classify_read_error(Error::from(kind));
            match err {
                AsynError::Status {
                    status: AsynStatus::Timeout,
                    ..
                } => {}
                other => panic!("{kind:?} must map to a non-fatal Timeout, got {other:?}"),
            }
            assert!(
                !classify_read_error(Error::from(kind)).is_fatal_transport(),
                "{kind:?} must not be a fatal transport error"
            );
        }
        // A genuine transport failure stays fatal (Io) → teardown.
        let reset = classify_read_error(Error::from(ErrorKind::ConnectionReset));
        assert!(matches!(reset, AsynError::Io(_)));
        assert!(reset.is_fatal_transport());
    }

    #[test]
    fn zero_timeout_write_attempts_send_not_instant_timeout() {
        // DRV-11 (write side): C writeRaw floors a zero timeout to a 1 ms
        // poll then attempts the send (615-617), so a `timeout == 0` write of
        // a writable socket succeeds. The write deadline must be floored too,
        // else write_with_retry's top-of-loop deadline check returns Timeout
        // before ever calling write().
        let (listener, port) = start_echo_server();
        let handle = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 16];
            let _ = s.read(&mut buf);
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        let mut wuser = AsynUser::new(0).with_timeout(Duration::ZERO);
        let n = drv.write_octet(&mut wuser, b"x").unwrap();
        assert_eq!(
            n, 1,
            "zero-timeout write of a writable socket must attempt the send, not instant-timeout"
        );

        handle.join().unwrap();
    }

    /// The write path bounds itself with a wait, and arms no `SO_SNDTIMEO`.
    ///
    /// VxWorks 7 does not implement `SO_SNDTIMEO` — `setsockopt` fails with
    /// `ENOPROTOOPT` (errno 42), measured on target — and this driver used to
    /// arm the option on every write and propagate that failure with `?`, so
    /// every asyn IP write there failed before a byte was sent. C never arms it
    /// on VxWorks either: `drvAsynIPPort.c` compiles the option only under
    /// `USE_SOCKTIMEOUT`, which is `#if defined(__rtems__)` (`:71-72`), while
    /// vxWorks takes `USE_POLL` (`:74-76`) and bounds `writeIt` with
    /// `poll(POLLOUT, writePollmsec)` (`:667-686`) instead.
    ///
    /// Host proxy for a target-only failure: Linux implements the option, so
    /// the pre-fix code also returned Timeout here. What fails before the fix
    /// is the `write_timeout()` assertion — arming the option AT ALL is the
    /// defect. `unix` only, because on Windows `SO_SNDTIMEO` works and the
    /// bound deliberately still rides on it.
    ///
    /// Not Darwin either, and for the opposite reason: there the option is
    /// absent *and* `MSG_DONTWAIT` is ignored, so this write really does park
    /// with nothing to end it. The exclusion marks an open defect, not a
    /// platform exemption — see [`SendWithoutParking`]'s "Known gap" and
    /// `doc/darwin-send-dontwait-gap.md`.
    #[cfg(all(unix, not(target_vendor = "apple")))]
    #[test]
    fn a_stalled_write_bounds_itself_without_arming_so_sndtimeo() {
        let (listener, port) = start_echo_server();
        // The peer accepts and never reads, so the send buffers fill and the
        // write stalls part-way. It must hold the connection open until the
        // write has run: a dropped socket would make the write fail with EPIPE
        // instead of stalling.
        let (release, wait_for_release) = std::sync::mpsc::channel::<()>();
        let handle = thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let _ = wait_for_release.recv();
            drop(sock);
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        drv.connect(&AsynUser::default()).unwrap();

        let payload = vec![0x5Au8; 8 * 1024 * 1024];
        let timeout = Duration::from_millis(200);
        let mut user = AsynUser::new(0).with_timeout(timeout);
        let start = std::time::Instant::now();
        let res = drv.write_octet(&mut user, &payload);
        let elapsed = start.elapsed();

        let err = match res {
            Err(e) => e,
            other => panic!("a peer that never reads must stall the write, got {other:?}"),
        };
        let AsynError::PartialWrite { source, nbytes } = &err else {
            panic!(
                "C reports *nbytesTransferred with the timeout status \
                 (drvAsynIPPort.c:712-719), got {err:?}"
            );
        };
        assert!(
            matches!(
                **source,
                AsynError::Status {
                    status: AsynStatus::Timeout,
                    ..
                }
            ),
            "C `writeIt` returns asynTimeout when its POLLOUT wait expires \
             (drvAsynIPPort.c:684-686), got {source:?}"
        );
        // …and some of the 8 MiB did go out before the stall, so this is the
        // send loop reaching its deadline and not an instant refusal.
        assert!(
            *nbytes > 0,
            "the send loop must have moved bytes before the deadline"
        );
        // The wait is what bounds the call. A blocking `write` on a stream
        // socket does not return short when the send buffer fills — it waits
        // until the whole buffer is queued — so a wait-then-blocking-write
        // would park here far past the deadline.
        assert!(
            elapsed < timeout * 50,
            "the write must return at its deadline ({timeout:?}), took {elapsed:?}"
        );
        // The finding: no `SO_SNDTIMEO` was armed. This is the assertion that
        // fails before the fix.
        let Some(IpIoInner::Tcp(stream)) = drv.io.inner.as_ref() else {
            panic!("the TCP arm must still hold its stream");
        };
        assert_eq!(
            stream.write_timeout().unwrap(),
            None,
            "the write path must not arm SO_SNDTIMEO — VxWorks 7 rejects it \
             with ENOPROTOOPT and C only sets it under __rtems__"
        );

        let _ = release.send(());
        handle.join().unwrap();
    }

    #[test]
    fn zero_length_read_request_rejected_not_eof_teardown() {
        // DRV-55: C readRaw (736-740) rejects maxchars == 0 with asynError
        // before touching the socket. A Rust stream.read(&mut []) returns
        // Ok(0), which the TCP arm would misread as a peer EOF and tear down a
        // healthy connection. The guard must reject the empty request *and*
        // leave the socket connected.
        let (listener, port) = start_echo_server();
        let handle = thread::spawn(move || {
            let (_s, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(50));
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        let ruser = AsynUser::new(0).with_timeout(Duration::from_millis(50));
        let mut empty: [u8; 0] = [];
        let res = drv.read_octet(&ruser, &mut empty);
        assert!(
            matches!(
                res,
                Err(AsynError::Status {
                    status: AsynStatus::Error,
                    ..
                })
            ),
            "zero-length read must be rejected with asynError, got {res:?}"
        );
        assert!(
            drv.base().is_connected(),
            "zero-length read must not tear down the connection"
        );

        handle.join().unwrap();
    }

    #[test]
    fn test_disconnect_on_read_timeout_value_validation() {
        // C drvAsynIPPort.c::setOption (924-935) accepts only "Y"/"N"
        // (case-insensitive) and returns asynError for anything else,
        // rather than silently coercing unknown text to "off".
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:5025 tcp").unwrap();

        drv.set_option(&mut AsynUser::default(), "disconnectOnReadTimeout", "Y")
            .unwrap();
        assert_eq!(drv.get_option("disconnectOnReadTimeout").unwrap(), "Y");
        drv.set_option(&mut AsynUser::default(), "disconnectOnReadTimeout", "n")
            .unwrap();
        assert_eq!(drv.get_option("disconnectOnReadTimeout").unwrap(), "N");

        // Values C never accepts must now error instead of mapping to "off".
        for bad in ["1", "yes", "true", "", "maybe"] {
            assert!(
                drv.set_option(&mut AsynUser::default(), "disconnectOnReadTimeout", bad)
                    .is_err(),
                "value {bad:?} should be rejected"
            );
        }
        // The last accepted value (N) is unchanged by the rejected sets.
        assert_eq!(drv.get_option("disconnectOnReadTimeout").unwrap(), "N");
    }

    #[test]
    fn connect_rejects_double_open() {
        // C drvAsynIPPort.c::connectIt (424-427) returns asynError
        // "Link already open!" on a connect to an already-open link,
        // rather than opening a second socket and leaking the first.
        let (listener, port) = start_echo_server();
        let _handle = thread::spawn(move || {
            let _ = listener.accept();
            thread::sleep(Duration::from_secs(1));
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        assert!(drv.base().is_connected());

        let err = drv.connect(&user).unwrap_err();
        assert!(matches!(err, AsynError::Status { .. }));
        // The original socket is left intact (still connected).
        assert!(drv.base().is_connected());
    }

    // --- hostInfo option tests ---

    #[test]
    fn test_set_option_host_info() {
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:5025").unwrap();
        drv.set_option(&mut AsynUser::default(), "hostInfo", "192.168.1.1:8080")
            .unwrap();
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
        assert!(drv.base().is_connected());

        drv.set_option(&mut AsynUser::default(), "hostInfo", "127.0.0.1:9999")
            .unwrap();
        assert!(!drv.base().is_connected());
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

        drv.set_option(&mut AsynUser::default(), "hostInfo", "127.0.0.1:5026 udp")
            .unwrap();
        assert_eq!(drv.config.protocol, IpProtocol::Udp);
        assert_eq!(drv.config.port, 5026);

        drv.set_option(&mut AsynUser::default(), "hostInfo", "127.0.0.1:5027 udp*")
            .unwrap();
        assert_eq!(drv.config.protocol, IpProtocol::UdpBroadcast);

        drv.set_option(&mut AsynUser::default(), "hostInfo", "127.0.0.1:5028 udp&")
            .unwrap();
        assert_eq!(drv.config.protocol, IpProtocol::UdpReusePort);

        drv.set_option(&mut AsynUser::default(), "hostInfo", "127.0.0.1:5029 udp*&")
            .unwrap();
        assert_eq!(drv.config.protocol, IpProtocol::UdpBroadcastReusePort);

        drv.set_option(&mut AsynUser::default(), "hostInfo", "127.0.0.1:5030 tcp&")
            .unwrap();
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
        drv.set_option(&mut AsynUser::default(), "hostInfo", "127.0.0.1:5026 tcp")
            .unwrap();
        assert_eq!(
            drv.config.local_port, None,
            "local_port must reset on hostInfo reparse"
        );
    }

    /// Regression.
    ///
    /// C `drvAsynIPPort` compares option keys with `epicsStrCaseCmp`
    /// (drvAsynIPPort.c:899/937), so the asynRecord's lowercase
    /// `hostinfo`/`disconnectOnReadTimeout` keys and any mixed-case iocsh
    /// key reach the real driver handler. Pre-fix the Rust driver matched
    /// keys case-sensitively, so the asynRecord's lowercase `hostinfo`
    /// write fell into the generic option map: `config.host`/`port`/
    /// `protocol` stayed at the old endpoint, and reads returned the map
    /// value (or nothing) instead of the live spec.
    #[test]
    fn host_info_option_key_is_case_insensitive_for_get_and_set() {
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:5025 tcp").unwrap();

        // asynRecord writes the lowercase key (asyn_record/mod.rs).
        drv.set_option(&mut AsynUser::default(), "hostinfo", "10.0.0.5:1234 udp")
            .unwrap();
        // The live driver config must reflect the reparse, not the map.
        assert_eq!(drv.config.host, "10.0.0.5");
        assert_eq!(drv.config.port, 1234);
        assert_eq!(drv.config.protocol, IpProtocol::Udp);

        // get must echo the live endpoint for either key case
        // (C getOption returns tty->IPDeviceName verbatim).
        assert_eq!(drv.get_option("hostinfo").unwrap(), "10.0.0.5:1234 udp");
        assert_eq!(drv.get_option("hostInfo").unwrap(), "10.0.0.5:1234 udp");

        // disconnectOnReadTimeout shares the same case-insensitive key
        // contract for set and get (C getOption -> "Y"/"N").
        drv.set_option(&mut AsynUser::default(), "DISCONNECTONREADTIMEOUT", "Y")
            .unwrap();
        assert_eq!(drv.get_option("disconnectonreadtimeout").unwrap(), "Y");
    }

    #[test]
    fn unsupported_option_key_is_rejected() {
        // C drvAsynIPPort.c::setOption (941-945) / getOption (902-906)
        // reject any non-empty unsupported key (asynError "Unsupported key")
        // and never store it, so a later getOption cannot echo it back; the
        // empty key is a silent no-op.
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:5025 tcp").unwrap();

        let err = drv
            .set_option(&mut AsynUser::default(), "bogusKey", "value")
            .unwrap_err();
        assert!(matches!(err, AsynError::OptionNotFound(_)));
        // R11-49: reported in C's words (drvAsynIPPort.c:903-904).
        assert_eq!(err.message(), "Unsupported key \"bogusKey\"");
        assert!(drv.get_option("bogusKey").is_err());

        // Empty key is a silent no-op (C `epicsStrCaseCmp(key,"") != 0`).
        drv.set_option(&mut AsynUser::default(), "", "ignored")
            .unwrap();
    }

    /// R11-49: the IP driver's own invalid-value texts are C's
    /// (drvAsynIPPort.c:932-933), and the Rust-only `noDelay` key follows the
    /// same shape rather than inventing a third format.
    #[test]
    fn every_ip_option_reports_cs_text() {
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:5025 tcp").unwrap();

        assert_eq!(
            drv.set_option(&mut AsynUser::default(), "disconnectOnReadTimeout", "maybe")
                .unwrap_err()
                .message(),
            "Invalid disconnectOnReadTimeout value."
        );
        assert_eq!(
            drv.set_option(&mut AsynUser::default(), "noDelay", "1")
                .unwrap_err()
                .message(),
            "Invalid noDelay value."
        );
        // Negative control: the valid values still take.
        drv.set_option(&mut AsynUser::default(), "disconnectOnReadTimeout", "y")
            .unwrap();
        assert!(drv.disconnect_on_read_timeout);
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

    // `unix://` parses on exactly the platforms where C defines `HAS_AF_UNIX`,
    // so both halves of that condition carry their own case. Ungated, the
    // accepting tests fail where the protocol does not exist, and the refusal
    // — the whole point of parsing it at configure time — ships untested on
    // the two targets it was written for.
    #[cfg(asyn_af_unix)]
    #[test]
    fn test_parse_unix_socket() {
        let cfg = IpPortConfig::parse("unix:///tmp/asyn.sock").unwrap();
        assert_eq!(cfg.protocol, IpProtocol::Unix);
        assert_eq!(cfg.host, "/tmp/asyn.sock");
        assert_eq!(cfg.port, 0);
    }

    #[cfg(asyn_af_unix)]
    #[test]
    fn test_parse_unix_empty_path() {
        assert!(IpPortConfig::parse("unix://").is_err());
    }

    #[cfg(not(asyn_af_unix))]
    #[test]
    fn test_parse_unix_refused_without_af_unix() {
        let err = IpPortConfig::parse("unix:///tmp/asyn.sock")
            .expect_err("a spec naming a protocol the platform lacks must not parse");
        assert!(
            err.to_string().contains("AF_UNIX not available"),
            "the refusal must name AF_UNIX, as `drvAsynIPPortConfigure` does: {err}"
        );
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

    /// R8-57 (superseding R8-50): `host:port COM` is now accepted, because the
    /// RFC 2217 layer it needs exists (`interpose::com`). C parses the token
    /// case-insensitively (`epicsStrCaseCmp`, drvAsynIPPort.c:364).
    ///
    /// R8-50 refused the token outright — correctly, while the layer was
    /// missing, since a COM port run as a raw TCP stream is a different
    /// conversation with the terminal server. That refusal is now gone; this
    /// test is what stops it coming back as a "safe" default.
    #[test]
    fn com_protocol_is_accepted_case_insensitively() {
        for spec in ["1.2.3.4:5000 COM", "1.2.3.4:5000 com", "host:23 Com"] {
            let cfg = IpPortConfig::parse(spec).unwrap();
            assert_eq!(cfg.protocol, IpProtocol::Com, "{spec}");
        }
        // C :364-367 leaves socketType = SOCK_STREAM, so the port number parses
        // out of the address exactly as it does for TCP.
        let cfg = IpPortConfig::parse("1.2.3.4:5000 COM").unwrap();
        assert_eq!(cfg.host, "1.2.3.4");
        assert_eq!(cfg.port, 5000);
    }

    /// R8-57: `isCom` is what gates the install (C drvAsynIPPort.c:1061), so a
    /// COM port comes up with the IAC layer already in place — before the first
    /// transfer, not lazily at connect — and a TCP port never gets one.
    ///
    /// The IAC layer is the chain's *base*, not a stack entry (see
    /// `ComState::octet`), so the interpose stack itself stays empty: that is
    /// what keeps an EOS or echo layer pushed later from landing underneath it.
    #[test]
    fn com_port_installs_the_iac_layer_at_configure_time() {
        let com = DrvAsynIPPort::new("comport", "1.2.3.4:5000 COM").unwrap();
        assert!(com.com.is_some());
        assert_eq!(
            com.base.interpose_octet.len(),
            0,
            "the IAC layer is the base link, not a reorderable stack entry"
        );

        let tcp = DrvAsynIPPort::new("tcpport", "1.2.3.4:5000 TCP").unwrap();
        assert!(tcp.com.is_none());
        assert_eq!(tcp.base.interpose_octet.len(), 0);
    }

    /// R8-57 ordering: C installs COM at :1061 and the EOS interpose at :1065,
    /// and `interposeInterface` makes each *later* install the *outer* one — so
    /// C's chain is `EOS → COM → driver`, and EOS sees bytes COM has already
    /// unstuffed.
    ///
    /// This is the regression that pushing COM onto the interpose stack caused:
    /// the stack dispatches index 0 first and `push` appends, so a COM pushed at
    /// construction sat *outside* the EOS layer `build_configured_ip_port` pushes
    /// afterwards, inverting C's chain.
    ///
    /// Most streams do not notice, because unstuffing and terminator-stripping
    /// commute — a 0xFF escape never creates or destroys a `\n`. What does not
    /// commute is the *byte budget*: the caller's buffer is filled by whichever
    /// layer is outermost. The device sends `A <IAC IAC> B \n` into a 3-byte read.
    ///
    ///   * C's order (EOS above COM): COM unstuffs first, so EOS fills the 3 bytes
    ///     with real data — `A <IAC> B`.
    ///   * inverted (COM above EOS): EOS fills the 3 bytes with the *stuffed*
    ///     stream — `A <IAC> <IAC>` — and COM then unstuffs that down to 2, so `B`
    ///     is silently dropped from the read.
    ///
    /// Assert the 3 bytes. Under the inverted chain this returns 2 and fails.
    ///
    /// Gated on `epics`: the interpose order under test is the one
    /// `crate::iocsh::build_configured_ip_port` installs, and `iocsh` is part
    /// of the EPICS surface. The driver itself needs no feature.
    #[cfg(feature = "epics")]
    #[test]
    fn eos_pushed_after_com_sits_above_it_and_sees_unstuffed_bytes() {
        use crate::interpose::com::IAC;

        let (listener, port) = start_echo_server();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            // Refuse the handshake; this test is about the data path.
            stream.write_all(&[IAC, 252, 0]).unwrap(); // IAC WONT BINARY
            stream.flush().unwrap();
            let mut hs = [0u8; 3];
            stream.read_exact(&mut hs).unwrap();
            // One escaped IAC inside a terminated line.
            stream.write_all(&[b'A', IAC, IAC, b'B', b'\n']).unwrap();
            stream.flush().unwrap();
            thread::sleep(Duration::from_millis(300));
        });

        // The iocsh path: DrvAsynIPPort::new installs COM, then the EOS interpose
        // goes on top (noProcessEos defaults off, C :1064-1065).
        let mut drv = crate::iocsh::build_configured_ip_port(
            "com_eos",
            &format!("127.0.0.1:{port} COM"),
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            drv.base.interpose_octet.len(),
            1,
            "EOS is the only stack layer; COM is the base"
        );
        drv.connect(&AsynUser::default()).unwrap();
        drv.set_input_eos(&AsynUser::default(), b"\n").unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 3];
        let (n, _eom) = chain_read_eom(&mut drv, &user, &mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            &[b'A', IAC, b'B'],
            "the 3-byte read must carry 3 bytes of device data — an escape byte \
             must not eat one of them, which is what happens when COM sits above EOS"
        );

        server.join().unwrap();
    }

    /// R8-57: C interposes the COM asynOption interface *above* the IP driver's
    /// (asynInterposeCom.c:809-824), so the seven serial keys are answered by the
    /// negotiation and everything else falls through to the driver below
    /// (:710-717). Reading `baud` off a COM port must not reach the IP driver's
    /// option map, and reading `hostInfo` must still reach it.
    #[test]
    fn com_option_interface_is_layered_over_the_ip_drivers() {
        let com = DrvAsynIPPort::new("comport", "1.2.3.4:5000 COM").unwrap();
        // Answered by the interpose, from the defaults C installs (:837-841).
        assert_eq!(com.get_option("baud").unwrap(), "9600");
        assert_eq!(com.get_option("parity").unwrap(), "none");
        assert_eq!(com.get_option("crtscts").unwrap(), "N");
        // Not the interpose's key: falls through to the IP driver.
        assert_eq!(com.get_option("hostInfo").unwrap(), "1.2.3.4:5000 COM");
        assert_eq!(com.get_option("disconnectOnReadTimeout").unwrap(), "N");

        // A plain TCP port has no COM layer, so the serial keys are unknown to it
        // — the same asynError C's IP driver returns for an unsupported key.
        let tcp = DrvAsynIPPort::new("tcpport", "1.2.3.4:5000 TCP").unwrap();
        assert!(tcp.get_option("baud").is_err());
    }

    /// R8-50: the token classification is exhaustive, so an unknown protocol is
    /// C's `Unknown protocol "%s".` (drvAsynIPPort.c:389) rather than a
    /// port-number complaint — and it is never silently accepted as TCP.
    #[test]
    fn unknown_protocol_token_is_rejected_by_name() {
        let err = IpPortConfig::parse("1.2.3.4:5000 SCTP").unwrap_err();
        assert_eq!(err.message(), "Unknown protocol \"SCTP\".");
        // C's `%5s` into `char protocol[6]` truncates the token to five
        // characters before the comparison, and the message prints what it
        // matched against.
        let err = IpPortConfig::parse("1.2.3.4:5000 tcpsocket").unwrap_err();
        assert_eq!(err.message(), "Unknown protocol \"tcpso\".");
    }

    /// R8-50: C reads exactly one token (`sscanf(blank+1, "%5s", protocol)`) and
    /// ignores whatever follows it, so a spec with trailing words still selects
    /// the first protocol rather than failing on the address.
    #[test]
    fn only_the_first_token_after_the_blank_is_the_protocol() {
        let cfg = IpPortConfig::parse("1.2.3.4:5000 UDP extra junk").unwrap();
        assert_eq!(cfg.host, "1.2.3.4");
        assert_eq!(cfg.port, 5000);
        assert_eq!(cfg.protocol, IpProtocol::Udp);
    }

    // --- Unix socket integration test ---

    #[cfg(asyn_af_unix)]
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
        assert!(drv.base().is_connected());

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
        drv.install_interpose(Box::new(EosInterpose::new(EosConfig {
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
        let n = chain_read(&mut drv, &ruser, &mut small).unwrap();
        assert_eq!(&small[..n], b"OLD_");

        // Flush must clear BOTH the socket AND the interpose buffer.
        let mut fuser = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        chain_flush(&mut drv, &mut fuser).unwrap();

        // Command + response cycle. If the interpose buffer was not
        // reset, this read returns the leftover "LINE" instead of "NEW".
        let mut wuser = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut wuser, b"CMD").unwrap();
        let ruser2 = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 64];
        let n = chain_read(&mut drv, &ruser2, &mut buf).unwrap();
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
