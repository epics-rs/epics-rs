//! TCP/UDP port driver (drvAsynIPPort equivalent).
//!
//! Supports TCP, UDP, and Unix domain sockets, with protocol suffixes
//! matching C asyn's `drvAsynIPPortConfigure` specification format.

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::time::Duration;

use epics_libcom_rs::runtime::socket;

use super::option_parse::{invalid_option_value, parse_yn_option, sscanf_int};
use super::wait_millis;
use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::exception::AsynException;
use crate::interpose::com::{ComInterpose, ComPortOptions};
use crate::interpose::{EomReason, OctetInterpose, OctetNext, OctetReadResult};
use crate::port::InterfaceMask;
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
///   `HAS_AF_UNIX` (`drvAsynIPPort.c:63-65`) — every unix, RTEMS included;
///   C excludes only `_WIN32` and `vxWorks` by name.
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
    /// `asynInterposeCOM` on the port (:1115) — the layer that IAC-escapes the
    /// data stream and carries baud/bits/parity/stop/flow to the server. See
    /// [`crate::interpose::com`].
    Com,
}

impl IpProtocol {
    /// C's `FLAG_BROADCAST` (drvAsynIPPort.c:379-382) — the `udp*` suffix.
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
            // Where the platform has no AF_UNIX the spec is refused *here*, in
            // parsing, which is where C refuses it — `drvAsynIPPortConfigure`
            // prints "AF_UNIX not available on this platform." and returns -1
            // from inside this same `unix://` branch (drvAsynIPPort.c:316-317).
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
        })
    }
}

/// Split the trailing protocol token off the address part.
/// Returns `(addr_part, protocol)`.
///
/// C `drvAsynIPPort.c:349-351`: the protocol begins at the first blank
/// (`strchr(cp, ' ')`) and `sscanf(blank+1, "%5s", protocol)` takes the first
/// whitespace-delimited run, truncated to five characters (`char protocol[6]`);
/// whatever follows that run is ignored. With no blank at all, `protocol[0]` is
/// `'\0'` and C defaults to `SOCK_STREAM` (:356-358).
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
        // `drvAsynIPPortConfigure` install `asynInterposeCOM` (:1115).
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
        let port = scan_port(parts[0], "port number")?;
        let local_port = match parts.get(1) {
            Some(p) => Some(scan_port(p, "local port")?),
            None => None,
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

    let port = scan_port(parts[1], "port number")?;
    let local_port = match parts.get(2) {
        Some(p) => Some(scan_port(p, "local port")?),
        None => None,
    };

    Ok((host, port, local_port))
}

/// C `sscanf(cp, ":%d", &port)` (drvAsynIPPort.c:334, :340), after the colon.
///
/// C defect not copied: C hands the `int` to `htons`, so port 70000 connects
/// to 4464 and -1 to 65535. A value outside `u16` is refused here.
fn scan_port(text: &str, what: &str) -> AsynResult<u16> {
    sscanf_int(text)
        .and_then(|v| u16::try_from(v).ok())
        .ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("invalid {what}: '{text}'"),
        })
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

/// The libbsd value RTEMS's own headers give, pinned here rather than taken
/// from `libc` — the `SEND_DONTWAIT` precedent in
/// `epics-libcom-rs/src/runtime/blocking_io.rs`, which carries the full
/// reason. That is a choice, not a workaround: the `newlib/arm` versus
/// `newlib/rtems` glob collision that once made a bare `libc::MSG_DONTWAIT`
/// ambiguous on `armv7-rtems-eabihf` was removed by the pinned `libc` fork in
/// `6f64e70d`, so the bare name compiles and is correct there today.
#[cfg(target_os = "rtems")]
const SEND_DONTWAIT: libc::c_int = 0x0080;
#[cfg(all(unix, not(target_os = "rtems")))]
const SEND_DONTWAIT: libc::c_int = libc::MSG_DONTWAIT;

/// Wait until the socket will take bytes, or the poll interval expires
/// (`Ok(false)`).
///
/// `Ok(false)` is C's `asynTimeout`; it does NOT prove `deadline` has passed.
/// The interval given to `poll` is [`wait_millis`] of the budget still left,
/// so a sub-millisecond remainder truncates and the wait can end up to 1 ms
/// early. That is C's own arithmetic — `writePollmsec = (int)(timeout *
/// 1000.0)` (`drvAsynIPPort.c:662-664`) — and C likewise returns `asynTimeout`
/// straight off `pollstatus == 0` without re-reading any deadline (`:697`).
/// Reproducing the early edge is deliberate; a caller needing the deadline
/// held exactly must re-check it, not this trait.
///
/// C parity, and why the write bound is not `SO_SNDTIMEO`: `drvAsynIPPort.c`
/// compiles the `setsockopt(SO_SNDTIMEO)` block only under `USE_SOCKTIMEOUT`,
/// which is `#if defined(__rtems__)` (`:71-72`). Every other target takes
/// `USE_POLL` (`:74`), where `writeIt` polls `POLLOUT` for `writePollmsec`
/// (`:682-701`) and then sends. The poll is the bound on every target here, so
/// the write path has one rule rather than an RTEMS branch.
pub(super) trait WaitWritable {
    /// `None` is a wait with no deadline — C's negative `pasynUser->timeout`,
    /// which reaches `poll` as its own `-1` (drvAsynIPPort.c:662-664).
    fn wait_writable(&self, deadline: Option<std::time::Instant>) -> std::io::Result<bool>;
}

/// Hand bytes to the socket without parking — the stream arms of [`IpIoInner`].
///
/// C reaches the same place by putting the socket in non-blocking mode once, at
/// connect (`setNonBlock(fd, 1)`, `drvAsynIPPort.c:516`, under `USE_POLL`), and
/// so does this driver — [`OWNED_NONBLOCKING`]. The per-send `MSG_DONTWAIT`
/// below is kept on top of it, not instead of it.
///
/// It was instead of it, on the reasoning that a per-send flag leaves the file
/// description alone and so leaves the `SO_RCVTIMEO` read bound in place. That
/// traded a bound every target honours for a socket option; the read side now
/// polls as C's does ([`BoundNextRead`]), which is the price, and it is the
/// smaller one.
///
/// The wait alone is not enough to make the deadline hold: a blocking `write` on
/// a stream socket does not return a short count when the send buffer fills, it
/// waits until the whole buffer is queued.
///
/// # The flag is the fast path, not the guarantee
///
/// XNU's `sosend` sleeps on `so_state & SS_NBIO` and its internal `MSG_NBIO`,
/// not on `MSG_DONTWAIT`, so while the flag was the only thing holding the send
/// out of a park, a stalled 8 MiB write on macOS outlived a 240 s test timeout
/// (measured, macOS CI 2026-07-27). C never
/// had the exposure because it takes the other branch described above, and this
/// driver now takes it too — [`OWNED_NONBLOCKING`], applied at connect. The
/// flag stays because where it is honoured it saves the loop a `poll`.
pub(super) trait SendWithoutParking {
    fn send_without_parking(&self, buf: &[u8]) -> std::io::Result<usize>;
}

/// The blocking mode this driver owns for a live socket, applied at the two
/// points a socket becomes one: [`IpIoInner::own_blocking_mode`] for a client
/// connect, and `ip_server_port`'s `ClientSlot::assign` for a served
/// connection.
///
/// Non-blocking, which is C's `setNonBlock(fd, 1)` (`drvAsynIPPort.c:516`)
/// under `USE_POLL` — one statement covering both, since the server-accepted
/// fd reaches `connectIt` through `pasynUser->reason` (:436-437) and passes the
/// same line. With the mode owned, neither direction can park whatever the
/// target does with `MSG_DONTWAIT`, `SO_SNDTIMEO` or `SO_RCVTIMEO`, and the
/// bound is the poll in [`WaitWritable`] / [`BoundNextRead`] everywhere. WIN32
/// is `USE_POLL` too, polling through `WSAPoll` (:77-79), so a read that
/// times out there reports `WSAEWOULDBLOCK` as C's does, not the
/// `WSAETIMEDOUT` an `SO_RCVTIMEO` bound answers. RTEMS differs from C:
/// `__rtems__` selects `USE_SOCKTIMEOUT` (:71-72), so C never calls
/// `setNonBlock` there and bounds each transfer by `SO_*TIMEO`.
pub(super) const OWNED_NONBLOCKING: bool = true;

/// [`wait_millis`] as `poll`'s interval; `None` is its own `-1`.
fn poll_millis(timeout: Option<Duration>) -> i32 {
    match timeout {
        Some(t) => wait_millis(t).min(i32::MAX as u128) as i32,
        None => -1,
    }
}

/// Make the `read`/`recv_from` that follows return within `timeout`.
///
/// The read-side counterpart to [`WaitWritable`], and it moved here for the
/// same reason the write bound did: the descriptor is non-blocking, so
/// `SO_RCVTIMEO` no longer applies to the call and a `POLLIN` wait does. C is
/// again the same shape — under `USE_POLL` it polls its reads too, which is
/// what makes the option's absence from that path unremarkable rather than a
/// regression.
///
/// Returns nothing on purpose. A poll error, and the expiry itself, both fall
/// through to the read, which reports the real outcome — expiry as `WouldBlock`
/// on the non-blocking socket, the same `asynTimeout` `SO_RCVTIMEO` produced
/// (see [`classify_read_error`]). That is C's own fall-through: `readIt`
/// records a failed `setsockopt(SO_RCVTIMEO)` and proceeds to `recv` anyway
/// (`drvAsynIPPort.c:795-808`), letting the recv outcome govern teardown
/// (`:851-875`).
pub(super) trait BoundNextRead {
    fn bound_next_read(&self, timeout: Option<Duration>);
}

impl<T: socket::Pollable> BoundNextRead for T {
    fn bound_next_read(&self, timeout: Option<Duration>) {
        // `None` is C's negative timeout: `readPollmsec = -1`
        // (drvAsynIPPort.c:794), which is `poll`'s own "no timeout".
        let _ = socket::poll_ready(self, socket::Readiness::Readable, poll_millis(timeout));
    }
}

impl<T: socket::Pollable> WaitWritable for T {
    fn wait_writable(&self, deadline: Option<std::time::Instant>) -> std::io::Result<bool> {
        loop {
            let remaining = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        return Ok(false);
                    }
                    Some(remaining)
                }
                None => None,
            };
            match socket::poll_ready(self, socket::Readiness::Writable, poll_millis(remaining)) {
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                outcome => return outcome,
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

/// Windows has no `MSG_DONTWAIT`; the owned non-blocking mode alone keeps the
/// send out of a park, as it does C's `send` there.
#[cfg(windows)]
impl SendWithoutParking for TcpStream {
    fn send_without_parking(&self, buf: &[u8]) -> std::io::Result<usize> {
        (&mut &*self).write(buf)
    }
}

/// How [`write_with_retry`] gave up. C `writeIt` either returns from inside
/// its loop — a poll that failed or expired (drvAsynIPPort.c:686-701) — or
/// breaks out of it — `send` gave 0, the retry bound ran out, a write error
/// (:710-748) — and only a break reaches the `wrote N` end line (:750-753).
#[derive(Debug)]
pub(super) enum WriteFailure {
    Returned(AsynError),
    Broke(AsynError),
}

impl WriteFailure {
    pub(super) fn into_error(self) -> AsynError {
        match self {
            Self::Returned(e) | Self::Broke(e) => e,
        }
    }
}

/// Write `data` to the stream, retrying short writes until the deadline.
///
/// Returns the bytes the peer accepted. C parity
/// (`drvAsynIPPort.c::writeIt`, like `drvAsynSerialPort.c:843`): a write that
/// stalls part-way reports `*nbytesTransferred` **together with** its
/// `asynTimeout`/`asynError` status, so a failure here carries the accepted
/// count in [`AsynError::with_partial_write`] rather than dropping it — that
/// count is what `asynRecord` publishes as NAWT (asynRecord.c:1547).
///
/// The loop is C's shape: wait for `POLLOUT`, send, advance, repeat until the
/// buffer is gone (`drvAsynIPPort.c:680-750`). Waiting first is what bounds the
/// call — see [`WaitWritable`] for why that bound is not `SO_SNDTIMEO`.
///
/// `poll` is C's `writePollmsec` (`:662-663`) and it bounds **one** wait for
/// `POLLOUT`, re-armed on every pass exactly as C re-arms it (`:686-696`,
/// `asynTimeout` only when that single poll expires, `:697-701`). A peer that
/// keeps taking bytes therefore never times out, however long the transfer
/// runs — which is what C does, and what one deadline built at write-start did
/// not: it expired mid-transfer and handed the caller `asynTimeout` with a
/// partial count where C returns success, so an `asynOctetWrite` record raised
/// a WRITE alarm and published NAWT below NOWT (DRV-58).
///
/// `cap` is not C's. It is the outer budget a caller may impose over the whole
/// transfer — the server port's broadcast states one timeout for all its slots,
/// so slot N+1 inherits what slot N left rather than restarting the clock. A
/// caller that wants C's rule alone passes `None`.
///
/// Shared with [`super::ip_server_port::ClientSlot`], whose accepted sockets
/// need the same bound and the same partial count. C shares it the same way:
/// each accepted connection is served by a real `drvAsynIPPort` child
/// (`drvAsynIPServerPort.c:711-715`), so both ports write through this one loop
/// there too.
pub(super) fn write_with_retry(
    stream: &(impl WaitWritable + SendWithoutParking),
    data: &[u8],
    poll: Option<Duration>,
    cap: Option<std::time::Instant>,
) -> Result<usize, WriteFailure> {
    let mut offset = 0;
    // C's `haveStartTime`/`startTime` (drvAsynIPPort.c:679, 711-723): stamped at
    // the FIRST retryable `send` and never re-armed, not even by a pass that
    // moved bytes. It is what bounds a `poll` that keeps reporting writable
    // against a `send` that keeps answering EWOULDBLOCK — a cycle the per-pass
    // deadline below cannot bound, because that deadline is a fresh
    // `now + poll` every time round. Darwin runs exactly that cycle: its
    // `poll(POLLOUT)` reports writable at `SO_SNDLOWAT` rather than at an empty
    // buffer, so a 1 MiB write to a peer that had stopped reading never
    // returned at all, on any deadline the caller gave it.
    let mut retry_since: Option<std::time::Instant> = None;
    while offset < data.len() {
        // `poll == None` is C's negative timeout — an unbounded wait, which
        // only the caller's own budget (`cap`) can cut short.
        let deadline = match (poll.map(|p| std::time::Instant::now() + p), cap) {
            (Some(pass), Some(cap)) => Some(pass.min(cap)),
            (Some(pass), None) => Some(pass),
            (None, cap) => cap,
        };
        match stream.wait_writable(deadline) {
            Ok(true) => {}
            Ok(false) => {
                return Err(WriteFailure::Returned(
                    AsynError::Status {
                        status: AsynStatus::Timeout,
                        message: "write timeout".into(),
                    }
                    .with_partial_write(offset),
                ));
            }
            Err(e) => {
                return Err(WriteFailure::Returned(
                    AsynError::Io(e).with_partial_write(offset),
                ));
            }
        }
        match stream.send_without_parking(&data[offset..]) {
            Ok(0) => {
                return Err(WriteFailure::Broke(
                    AsynError::Status {
                        status: AsynStatus::Timeout,
                        message: "write returned 0 bytes".into(),
                    }
                    .with_partial_write(offset),
                ));
            }
            Ok(n) => offset += n,
            // C retries both of these against `pasynUser->timeout`
            // (`drvAsynIPPort.c:710-724`) and sleeps `SEND_RETRY_DELAY`
            // between attempts; the wait at the top of this loop is that
            // sleep, so the retry needs none of its own — but it is not the
            // bound, because it re-arms.
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted =>
            {
                // C compares against `pasynUser->timeout`; `poll` is that value
                // with C's own zero-to-1 ms floor already applied
                // (`socket_poll_timeout`), which is the whole difference.
                // `None` is C's negative timeout, whose retry burst C leaves
                // unbounded too (`else if (pasynUser->timeout >= 0)`).
                match retry_since {
                    None => retry_since = Some(std::time::Instant::now()),
                    Some(since) => {
                        // C's `thisWrite = 0; break;` (:719-721): the
                        // `send() returned 0` exit.
                        if poll.is_some_and(|p| since.elapsed() > p) {
                            return Err(WriteFailure::Broke(
                                AsynError::Status {
                                    status: AsynStatus::Timeout,
                                    message: "write timeout".into(),
                                }
                                .with_partial_write(offset),
                            ));
                        }
                    }
                }
            }
            Err(e) => {
                return Err(WriteFailure::Broke(
                    AsynError::Io(e).with_partial_write(offset),
                ));
            }
        }
    }
    Ok(offset)
}

struct IpIoState {
    inner: Option<IpIoInner>,
    /// C `tty->IPDeviceName`: the verbatim host-info spec `parseHostInfo`
    /// stores (`epicsStrDup(hostInfo)`), which every readIt/writeIt/flushIt
    /// trace and refusal names.
    host_info: String,
    /// C `tty->nRead` / `tty->nWritten`: bytes the socket moved since the last
    /// `parseHostInfo`, the only place C zeroes them (:287-288).
    n_read: u64,
    n_written: u64,
}

impl IpIoState {
    fn new(host_info: String) -> Self {
        Self {
            inner: None,
            host_info,
            n_read: 0,
            n_written: 0,
        }
    }

    /// C `tty->fd` as `asynCommonReport` prints it: `INVALID_SOCKET` reads -1.
    fn fd(&self) -> i64 {
        #[cfg(unix)]
        fn raw(s: &impl std::os::fd::AsRawFd) -> i64 {
            i64::from(s.as_raw_fd())
        }
        #[cfg(windows)]
        fn raw(s: &impl std::os::windows::io::AsRawSocket) -> i64 {
            s.as_raw_socket() as i64
        }
        match &self.inner {
            None => -1,
            Some(IpIoInner::Tcp(stream)) => raw(stream),
            Some(IpIoInner::Udp(socket, _)) => raw(socket),
            #[cfg(asyn_af_unix)]
            Some(IpIoInner::Unix(stream)) => raw(stream),
        }
    }
}

impl IpIoInner {
    /// Put the socket into the mode this driver owns ([`OWNED_NONBLOCKING`]),
    /// which is what both bounds are built on.
    ///
    /// One site for all four transports, so a transport added later cannot
    /// reach the read and write paths without it. It takes the transport by
    /// value-borrow rather than reading it back out of [`IpIoState`] because
    /// it must run *before* the socket is installed — see
    /// [`DrvAsynIPPort::publish_transport`]. C sets the mode earlier, before
    /// `connect()` (`drvAsynIPPort.c:516` vs `:534`), because its bounded
    /// connect is that non-blocking socket polled for `POLLOUT` (:541-566);
    /// [`socket::tcp_connect`] makes the bounded connect itself, so the mode is
    /// owned once it returns. Both own it before `tty->fd = fd` (`:593`).
    fn own_blocking_mode(&self, device: &str) -> AsynResult<()> {
        match self {
            IpIoInner::Tcp(stream) => stream.set_nonblocking(OWNED_NONBLOCKING),
            IpIoInner::Udp(socket, _peer) => socket.set_nonblocking(OWNED_NONBLOCKING),
            #[cfg(asyn_af_unix)]
            IpIoInner::Unix(stream) => stream.set_nonblocking(OWNED_NONBLOCKING),
        }
        .map_err(|e| AsynError::Status {
            status: AsynStatus::Error,
            message: nonblock_failure_text(device, &e),
        })
    }
}

impl IpIoState {
    fn recv(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<(usize, EomReason)> {
        // C readIt (drvAsynIPPort.c:776-785) asks the socket, not the manager:
        // the enabled/connected gate is `queueRequest`'s, and a reader under
        // `lockPort` never passes it.
        let inner = self.inner.as_mut().ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("{} disconnected:", self.host_info),
        })?;
        // C readIt (drvAsynIPPort.c:787-791): reject maxchars == 0 *after*
        // the connect check and *before* touching the socket, so a zero-length
        // request never reaches `stream.read`, where an empty buffer would
        // yield Ok(0) and be misread as a peer EOF (tearing down the socket).
        if buf.is_empty() {
            return Err(maxchars_zero_error());
        }
        match inner {
            IpIoInner::Tcp(stream) => {
                // C readIt (drvAsynIPPort.c:795-808) records a failed bound but
                // falls through to recv(); the recv outcome governs teardown
                // (:851-875). Keeping that shape is what lets an ECONNRESET the
                // read is about to surface reach the caller, instead of a
                // synthetic "arming the bound failed" that would leave the port
                // never classifying the transport as dead — the status taint C
                // keeps for a >0-byte read (:876-885) is unreachable here.
                stream.bound_next_read(socket_poll_timeout(user.timeout));
                let r = stream.read(buf);
                if let Ok(n) = r {
                    trace_read(user, &self.host_info, None, &buf[..n]);
                }
                match r {
                    // C drvAsynIPPort.c::readIt (815-821): recv()==0 on a
                    // SOCK_STREAM socket means the peer closed — report
                    // success with ASYN_EOM_END and zero bytes (the driver
                    // then closes the fd). Returning an error here would hide
                    // END from close-delimited protocols (HTTP/1.0) that use
                    // connection-close as the message terminator.
                    Ok(0) => Ok((0, EomReason::END)),
                    Ok(n) => {
                        // C parity: CNT means the requested count was
                        // reached. A short read leaves the reason empty
                        // so the EOS interpose keeps reading.
                        let eom = if n >= buf.len() {
                            EomReason::CNT
                        } else {
                            EomReason::empty()
                        };
                        Ok((n, eom))
                    }
                    Err(e) => Err(classify_read_error(e)),
                }
            }
            IpIoInner::Udp(socket, _peer) => {
                // Same C fall-through as the TCP arm: the bound precedes the
                // socketType branch in readIt (:828) and a failure does not
                // return — the recvfrom governs.
                socket.bound_next_read(socket_poll_timeout(user.timeout));
                // C drvAsynIPPort.c::readIt (775-789) uses recvfrom on the
                // unconnected datagram socket so it accepts replies from any
                // peer (broadcast/multi-peer); the source address is only
                // used for trace, so we discard it.
                let r = socket.recv_from(buf);
                if let Ok((n, src)) = r {
                    trace_read(user, &self.host_info, Some(src), &buf[..n]);
                }
                match r {
                    // C drvAsynIPPort.c::readIt: a SOCK_DGRAM recvfrom()==0
                    // is a legitimate zero-length datagram, NOT a connection
                    // close — the EOF/ASYN_EOM_END branch (line 815) is
                    // SOCK_STREAM only. Report a successful zero-byte read and
                    // leave the socket open (no teardown).
                    Ok((n, _src)) => {
                        // C parity: CNT means the requested count was
                        // reached. A short read leaves the reason empty
                        // so the EOS interpose keeps reading.
                        let eom = if n >= buf.len() {
                            EomReason::CNT
                        } else {
                            EomReason::empty()
                        };
                        Ok((n, eom))
                    }
                    Err(e) => Err(classify_read_error(e)),
                }
            }
            #[cfg(asyn_af_unix)]
            IpIoInner::Unix(stream) => {
                // Same C fall-through as the TCP arm (drvAsynIPPort.c:800-806):
                // a failed bound must not pre-empt the read that surfaces the
                // real transport error.
                stream.bound_next_read(socket_poll_timeout(user.timeout));
                let r = stream.read(buf);
                if let Ok(n) = r {
                    trace_read(user, &self.host_info, None, &buf[..n]);
                }
                match r {
                    // Unix-domain stream EOF = peer closed = END, the same
                    // stream semantics as the TCP arm above.
                    Ok(0) => Ok((0, EomReason::END)),
                    Ok(n) => {
                        // C parity: CNT means the requested count was
                        // reached. A short read leaves the reason empty
                        // so the EOS interpose keeps reading.
                        let eom = if n >= buf.len() {
                            EomReason::CNT
                        } else {
                            EomReason::empty()
                        };
                        Ok((n, eom))
                    }
                    Err(e) => Err(classify_read_error(e)),
                }
            }
        }
    }

    fn send(&mut self, user: &mut AsynUser, data: &[u8]) -> Result<usize, WriteFailure> {
        // C writeIt (drvAsynIPPort.c:645-657).
        let inner = self.inner.as_mut().ok_or_else(|| {
            WriteFailure::Returned(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("{} disconnected", self.host_info),
            })
        })?;
        // C drvAsynIPPort.c::writeIt (613-614): after the connection check,
        // a zero-length write returns success immediately and sends nothing —
        // in particular it must NOT emit an empty UDP datagram. (Any output
        // EOS has already been appended by the interpose above, so reaching
        // here with empty data means there is genuinely nothing to send.)
        if data.is_empty() {
            return Ok(0);
        }
        // Floor the write poll with the same C poll-interval floor as the
        // socket send timeout: C `writeIt` (:662-663) floors a zero timeout to a
        // 1 ms poll and then attempts the send (poll POLLOUT then send), so a
        // `timeout == 0` write of a writable socket succeeds. Without the
        // floor the wait would expire at `now` and return Timeout *before*
        // ever attempting `write` — failing a zero-timeout write that C lets
        // through. socket_poll_timeout is a no-op for any positive timeout, so
        // this only affects the `timeout == 0` case.
        //
        // The wait bound is per pass, and this port imposes no outer cap; the
        // total is C's own `haveStartTime` retry bound inside
        // `write_with_retry`, which is what stops a writable-but-EWOULDBLOCK
        // cycle from running forever.
        let poll = socket_poll_timeout(user.timeout);
        // C writeIt reports what the socket took (`*nbytesTransferred`) on
        // success and on failure alike; return the real count rather than
        // assuming the whole buffer went out.
        // No arm arms `SO_SNDTIMEO`: the deadline is enforced by the wait in
        // `write_with_retry`, which is what C does everywhere but RTEMS. See
        // `WaitWritable`.
        match inner {
            IpIoInner::Tcp(stream) => write_with_retry(stream, data, poll, None),
            IpIoInner::Udp(socket, peer) => {
                // C polls `POLLOUT` before the datagram too — `writeIt` shares
                // one loop across `SOCK_DGRAM` and `SOCK_STREAM`
                // (`drvAsynIPPort.c:680-702`).
                let deadline = poll.map(|p| std::time::Instant::now() + p);
                match socket.wait_writable(deadline) {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(WriteFailure::Returned(AsynError::Status {
                            status: AsynStatus::Timeout,
                            message: "write timeout".into(),
                        }));
                    }
                    Err(e) => return Err(WriteFailure::Returned(e.into())),
                }
                // C drvAsynIPPort.c::writeIt (688): sendto the resolved
                // remote on the unconnected socket. A datagram is all-or-
                // nothing, so a failed sendto transferred zero bytes and needs
                // no partial-write carrier.
                socket
                    .send_to(data, *peer)
                    .map_err(|e| WriteFailure::Broke(e.into()))
            }
            #[cfg(asyn_af_unix)]
            IpIoInner::Unix(stream) => write_with_retry(stream, data, poll, None),
        }
    }
}

/// C readIt's `ASYN_TRACEIO_DRIVER` line (drvAsynIPPort.c:837-847): a datagram
/// names its source, as `ipAddrToDottedIP` prints it.
fn trace_read(user: &AsynUser, device: &str, from: Option<std::net::SocketAddr>, data: &[u8]) {
    let label = match from {
        Some(src) => format!("{device} (from {src}) read {}\n", data.len()),
        None => format!("{device} read {}\n", data.len()),
    };
    user.print_io(TraceMask::IO_DRIVER, data, &label, file!(), line!());
}

impl OctetNext for IpIoState {
    fn read(&mut self, user: &AsynUser, buf: &mut [u8]) -> OctetReadResult {
        user.print(
            TraceMask::FLOW,
            file!(),
            line!(),
            format_args!("{} read.\n", self.host_info),
        );
        let r = OctetReadResult::from(self.recv(user, buf));
        self.n_read += r.nbytes_transferred as u64;
        r
    }

    fn write(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        user.print(
            TraceMask::FLOW,
            file!(),
            line!(),
            format_args!("{} write.\n", self.host_info),
        );
        let label = format!("{} write {}\n", self.host_info, data.len());
        user.print_io(TraceMask::IO_DRIVER, data, &label, file!(), line!());
        let sent = self.send(user, data);
        // C prints the outcome only on leaving the send loop (:750-753): not on
        // the disconnected refusal or a zero-length write, which return before
        // it.
        let left_loop = match &sent {
            Ok(_) => !data.is_empty(),
            Err(f) => matches!(f, WriteFailure::Broke(_)),
        };
        let result = sent.map_err(WriteFailure::into_error);
        // C counts each partial `send` as it goes (:728), so a write that
        // fails part-way still counts what left.
        let sent = match &result {
            Ok(n) => *n,
            Err(e) => e.partial_write().unwrap_or(0),
        };
        self.n_written += sent as u64;
        if left_loop {
            let status = result
                .as_ref()
                .map_or_else(|e| e.status(), |_| AsynStatus::Success);
            user.print(
                TraceMask::FLOW,
                file!(),
                line!(),
                format_args!(
                    "wrote {sent} to {}, return {}.\n",
                    self.host_info,
                    status.str_status()
                ),
            );
        }
        result
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
    fn flush(&mut self, user: &mut AsynUser) -> AsynResult<()> {
        user.print(
            TraceMask::FLOW,
            file!(),
            line!(),
            format_args!("{} flush\n", self.host_info),
        );
        let mut scratch = [0u8; 4096];
        let mut total = 0usize;
        match self.inner.as_mut() {
            Some(IpIoInner::Tcp(stream)) => {
                let restore = stream.set_nonblocking(true);
                loop {
                    match stream.read(&mut scratch) {
                        Ok(0) => break, // EOF — nothing left to drain
                        Ok(n) => total += n,
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break, // reset/other — write/read will report it
                    }
                }
                if restore.is_ok() {
                    // Back to the mode the driver owns, not to blocking: on
                    // unix that mode already *is* non-blocking, and restoring
                    // blocking here would silently disarm both bounds for the
                    // rest of the connection.
                    let _ = stream.set_nonblocking(OWNED_NONBLOCKING);
                }
            }
            Some(IpIoInner::Udp(socket, _peer)) => {
                let restore = socket.set_nonblocking(true);
                loop {
                    match socket.recv_from(&mut scratch) {
                        Ok((n, _)) => total += n,
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                if restore.is_ok() {
                    let _ = socket.set_nonblocking(OWNED_NONBLOCKING);
                }
            }
            #[cfg(asyn_af_unix)]
            Some(IpIoInner::Unix(stream)) => {
                let restore = stream.set_nonblocking(true);
                loop {
                    match stream.read(&mut scratch) {
                        Ok(0) => break,
                        Ok(n) => total += n,
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                if restore.is_ok() {
                    // Back to the mode the driver owns, not to blocking: on
                    // unix that mode already *is* non-blocking, and restoring
                    // blocking here would silently disarm both bounds for the
                    // rest of the connection.
                    let _ = stream.set_nonblocking(OWNED_NONBLOCKING);
                }
            }
            None => {}
        }
        if total > 0 {
            user.print(
                TraceMask::IO_DRIVER,
                file!(),
                line!(),
                format_args!("{} flushed {total} bytes\n", self.host_info),
            );
        }
        Ok(())
    }
}

/// TCP/UDP port driver.
pub struct DrvAsynIPPort {
    base: PortDriverBase,
    /// The parsed hostInfo; `None` is C's `FLAG_SHUTDOWN` after a failed
    /// `parseHostInfo` (drvAsynIPPort.c:286, :399-400), which `connectIt`
    /// refuses.
    config: Option<HostInfo>,
    /// The lookup C's `hostToIPAddr` makes; a test answers its own.
    resolver: Resolver,
    /// `TCP_NODELAY` for the next TCP connect: C sets it unconditionally
    /// (connectIt :580-589); the rs-only `noDelay` option can turn it off, and
    /// a hostInfo reparse leaves it alone.
    no_delay: bool,
    io: IpIoState,
    /// Auto-disconnect when read times out (default: false).
    disconnect_on_read_timeout: bool,
    /// Both halves of C's COM interpose, present exactly when the configure
    /// string's protocol token was `COM` — C's `tty->isCom` (`drvAsynIPPort.c`
    /// :113, :364-365) and the `asynInterposeCOM` install it gates (:1115).
    com: Option<ComState>,
}

/// A name lookup: every answer, in the resolver's order; none on a failure.
type Resolver = Box<dyn Fn(&str, u16) -> Vec<std::net::SocketAddr> + Send + Sync>;

/// The system resolver, asked as `hostToIPAddr` asks it.
fn system_lookup(host: &str, port: u16) -> Vec<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    (host, port)
        .to_socket_addrs()
        .map(Iterator::collect)
        .unwrap_or_default()
}

/// The parsed hostInfo and the address its lookup kept: one value, so a new
/// hostInfo owes a lookup by construction, as C `parseHostInfo` sets
/// `FLAG_NEED_LOOKUP` beside each new `farAddr` (drvAsynIPPort.c:351-355).
struct HostInfo {
    config: IpPortConfig,
    /// C's `farAddr` once looked up (`FLAG_DONE_LOOKUP`, :488-497); `None`
    /// while a lookup is owed (`FLAG_NEED_LOOKUP`).
    kept: Option<std::net::SocketAddr>,
}

/// The two halves of C's `asynInterposeCOM`, which it interposes on the port as
/// two separate interfaces (`asynInterposeCom.c:792-824`). One `Option`, so "a
/// COM port has both halves and a non-COM port has neither" holds by type.
struct ComState {
    /// The `asynOctet` half — IAC stuffing.
    ///
    /// Deliberately **not** installed onto [`PortDriverBase::interpose_octet`]. C
    /// installs COM at :1115 and the EOS interpose at :1119, and its
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
    /// (`asynInterposeCom.c:339`, :443) — so the negotiation is neither stuffed
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
    fn read(&mut self, user: &AsynUser, buf: &mut [u8]) -> OctetReadResult {
        self.com.read(user, buf, self.io)
    }
    fn write(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        self.com.write(user, data, self.io)
    }
    fn flush(&mut self, user: &mut AsynUser) -> AsynResult<()> {
        self.com.flush(user, self.io)
    }
}

/// True for a socket read error that C `readIt` treats as a non-fatal
/// timeout — the socket is left intact and the caller retries. C
/// (drvAsynIPPort.c:851-854) excludes BOTH `EWOULDBLOCK` *and* `EINTR` from
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

/// Error for a zero-length read request. C `readIt` (drvAsynIPPort.c:787-791)
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

/// What C `readIt` does with a failed read (drvAsynIPPort.c:851-867) — the
/// teardown and the status it reports, as one value.
///
/// ```c
/// int should_disconnect = (((tty->disconnectOnReadTimeout) && (pasynUser->timeout > 0)) ||
///                          ((SOCKERRNO != SOCK_EWOULDBLOCK) && (SOCKERRNO != SOCK_EINTR)));
/// if (should_disconnect) {
///     epicsSnprintf(... "%s read error: %s" ...);
///     closeConnection(pasynUser,tty,"Read error");
///     status = asynError;                       /* :859 */
/// } else {
///     epicsSnprintf(... "%s timeout: %s" ...);
///     status = asynTimeout;                     /* :865 */
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
    /// The error the caller reports: C's `asynError` + `"%s read error: %s"` on
    /// the teardown branch, `asynTimeout` + `"%s timeout: %s"` on the other, and
    /// any failure that is neither the lower layer's verbatim.
    error: AsynError,
}

/// Single owner of C `readIt`'s failed-read rule. C applies it *inside*
/// `readIt`, which is below the interpose chain — so it governs both the data
/// path ([`DrvAsynIPPort::read_octet_core`]) and the RFC 2217 negotiation
/// ([`NegotiationLink`]), which C likewise drives through `readIt`
/// (`asynInterposeCom.c:103` calls `pasynOctetDrv->read`, the driver below).
/// Both callers take the teardown *and* the reported status from here, so
/// neither can drift.
fn classify_read_failure(
    e: AsynError,
    disconnect_on_read_timeout: bool,
    timeout: Option<Duration>,
    device_name: &str,
) -> ReadFailure {
    // Classify by the carried status, not the variant: a wrapped lower-layer
    // timeout would miss a variant match.
    let is_timeout = e.status() == AsynStatus::Timeout;
    // C tests the *sign*: `(tty->disconnectOnReadTimeout) && (pasynUser->timeout
    // > 0)` (drvAsynIPPort.c:853). A negative timeout — `None` here — is not
    // `> 0`, so a forever-wait never tears the socket down.
    let disconnect =
        (disconnect_on_read_timeout && is_timeout && timeout.is_some_and(|t| t > Duration::ZERO))
            || e.is_fatal_transport();
    if !disconnect && !is_timeout {
        return ReadFailure {
            disconnect: false,
            error: e,
        };
    }
    // C writes `pasynUser->errorMessage` and the status together (:852-866);
    // the count beside it (:878) stands, and rides in the read's result.
    let cause = sock_err_cause(&e);
    let restamped = if disconnect {
        AsynError::Status {
            status: AsynStatus::Error,
            message: format!("{device_name} read error: {cause}"),
        }
    } else {
        AsynError::Status {
            status: AsynStatus::Timeout,
            message: format!("{device_name} timeout: {cause}"),
        }
    };
    ReadFailure {
        disconnect,
        error: restamped,
    }
}

/// The raw link the RFC 2217 negotiation drives, and the teardown decision it
/// hands back.
///
/// C's COM interpose negotiates against `pinterposePvt->pasynOctetDrv`
/// (`asynInterposeCom.c:339`, :443, :103) — the octet driver *below* itself, so
/// the negotiation is neither IAC-stuffed nor unstuffed. But that lower driver is
/// `drvAsynIPPort`'s `readIt`, which still applies the
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
    /// `closeConnection`'s reason, set when a transfer failed in a way C's
    /// `readIt`/`writeIt` would have closed the socket for. Read back by
    /// [`DrvAsynIPPort::with_negotiation`].
    teardown: Option<&'static str>,
}

impl OctetNext for NegotiationLink<'_> {
    fn read(&mut self, user: &AsynUser, buf: &mut [u8]) -> OctetReadResult {
        let mut r = self.io.read(user, buf);
        if let Err(e) = r.status {
            let failure = classify_read_failure(
                e,
                self.disconnect_on_read_timeout,
                user.timeout,
                &self.io.host_info,
            );
            if failure.disconnect {
                self.teardown = Some("Read error");
            }
            r.status = Err(failure.error);
        }
        r
    }

    fn write(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let r = self.io.write(user, data);
        if let Err(ref e) = r {
            // C `writeIt` (drvAsynIPPort.c:741-748) closes the connection on any
            // failing send, which `is_fatal_transport` is the single owner of.
            if e.is_fatal_transport() {
                self.teardown = Some("Write error");
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
            message: super::errno_text(&e),
        }
    } else {
        AsynError::Io(e)
    }
}

/// C's `"Can't connect to %s: %s"` (drvAsynIPPort.c:568-571), or the setup
/// call's own text when the socket never reached `connect()`. C saves the
/// `sockerrmsg` from `connect()` itself (:534-539), before `poll` can clobber
/// `errno`, so a connect the poll then failed — timed out or refused — reports
/// the in-progress errno.
fn connect_failure_text(device: &str, e: &socket::TcpConnectError) -> String {
    match e {
        socket::TcpConnectError::Setup(setup) => setup_failure_text(device, setup),
        _ => format!(
            "Can't connect to {device}: {}",
            super::errno_text(e.sockerr())
        ),
    }
}

/// C `connectIt`'s text for each setup call that fails before `connect()`
/// (drvAsynIPPort.c:443-524); `device` is C's `IPDeviceName`, the hostInfo.
fn setup_failure_text(device: &str, e: &socket::SetupError) -> String {
    let err = super::errno_text(&e.error);
    match e.step {
        socket::SetupStep::Create => format!("Can't create socket: {err}"),
        socket::SetupStep::Bind => format!("unable to bind to local port: {err}"),
        socket::SetupStep::Broadcast => {
            format!("Can't set {device} socket BROADCAST option: {err}")
        }
        socket::SetupStep::ReusePort => {
            format!("Can't set {device} socket SO_REUSEPORT option: {err}")
        }
        // Not C's, and no client socket requests SO_REUSEADDR alone; worded
        // as C words its options.
        socket::SetupStep::ReuseAddress => {
            format!("Can't set {device} socket SO_REUSEADDR option: {err}")
        }
        socket::SetupStep::NonBlocking => nonblock_failure_text(device, &e.error),
        // Only `tcp_listener` reports it, and no client port listens.
        socket::SetupStep::Listen => format!("Can't listen on {device}: {err}"),
    }
}

/// Write to `sock` until the kernel refuses, leaving it stalled.
///
/// The stalled-write tests of both IP ports need a peer whose pipe cannot take
/// another byte, and they used to infer one from a size — a fixed 1 MiB payload
/// against buffers pinned to the minimum, later the measured capacity of a
/// throwaway connection. Neither inference holds: how much a "minimum"
/// buffer accepts is the kernel's own business, and on Darwin it is not
/// even the same answer for two sockets set up the same way. The fixed
/// 1 MiB fitted, and so did four times a sibling socket's measured
/// capacity — the slot write returned in 156 µs against the 200 ms
/// deadline it was supposed to spend.
///
/// "Until this socket says `WouldBlock`" needs no size and is the same
/// statement on every kernel — Winsock's loopback took a whole 8 MiB write in
/// one call — so each test stalls the socket it is about to write to and none
/// of them names a byte count.
#[cfg(test)]
pub(crate) fn fill_until_refused(sock: &TcpStream) {
    use std::io::Write;

    sock.set_nonblocking(true)
        .expect("nonblocking for the fill");
    let block = vec![0xa5u8; 64 * 1024];
    let mut total = 0usize;
    // Only a runaway guard: 256 MiB is far past any socket buffer, and a
    // kernel that never refused would otherwise hang the suite.
    while total < 256 << 20 {
        let taken = match (&*sock).write(&block) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // A socket that will not take another byte is refusing, which
            // is the condition this function exists to find. `Ok(0)` says
            // the same thing without an error, so both leave the same way.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
            Err(e) => panic!("filling the pipe failed: {e}"),
        };
        if taken == 0 {
            assert!(total > 0, "the socket refused the very first byte");
            // A refusal is the pipe being full only once it stays: in-flight
            // bytes reaching the peer, or Darwin's buffer auto-tuning, can
            // open room after it, and the stalled write would then fit.
            match socket::poll_ready(sock, socket::Readiness::Writable, 50) {
                Ok(false) => return,
                Ok(true) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => panic!("polling the filled socket failed: {e}"),
            }
        }
        total += taken;
    }
    panic!("the socket accepted {total} bytes without ever refusing");
}

/// What C's `epicsSocketConvertErrnoToString` prints for the errno a timed-out
/// read leaves on the non-blocking socket: `EWOULDBLOCK`'s `strerror`, or
/// `WSAEWOULDBLOCK`'s system message on WIN32.
#[cfg(test)]
pub(crate) fn would_block_text() -> String {
    #[cfg(unix)]
    let errno = libc::EWOULDBLOCK;
    #[cfg(windows)]
    let errno = windows_sys::Win32::Networking::WinSock::WSAEWOULDBLOCK;
    super::errno_text(&std::io::Error::from_raw_os_error(errno))
}

/// The errno C's non-blocking `connect()` leaves for a pending handshake, which
/// is the one its "Can't connect" text names (drvAsynIPPort.c:534-541):
/// `EINPROGRESS`, and `WSAEWOULDBLOCK` on WIN32.
#[cfg(test)]
fn connect_in_progress_text() -> String {
    #[cfg(unix)]
    let errno = libc::EINPROGRESS;
    #[cfg(windows)]
    let errno = windows_sys::Win32::Networking::WinSock::WSAEWOULDBLOCK;
    super::errno_text(&std::io::Error::from_raw_os_error(errno))
}

/// C's `TCP_NODELAY` failure after the connect (drvAsynIPPort.c:580-589).
fn nodelay_failure_text(device: &str, e: &std::io::Error) -> String {
    format!(
        "Can't set {device} socket NODELAY option: {}",
        super::errno_text(e)
    )
}

/// C's `setNonBlock` failure (drvAsynIPPort.c:516-523), for libcom's switch
/// around the bounded connect and for [`IpIoInner::own_blocking_mode`] alike.
pub(crate) fn nonblock_failure_text(device: &str, e: &std::io::Error) -> String {
    format!(
        "Can't set {device} O_NONBLOCK option: {}",
        super::errno_text(e)
    )
}

/// The `sockerrmsg` a failed read carries up to [`classify_read_failure`].
fn sock_err_cause(e: &AsynError) -> String {
    match e {
        AsynError::Io(io) => super::errno_text(io),
        other => other.message(),
    }
}

/// Map an `AsynUser` timeout to the socket-level receive/send timeout,
/// applying the C `drvAsynIPPort` poll-interval floor.
///
/// C `readIt`/`writeIt` compute `pollmsec = (int)(timeout * 1000.0)` and
/// then `if (pollmsec == 0) pollmsec = 1` (drvAsynIPPort.c:792-794 and
/// :662-664): a zero timeout (a poll / non-blocking request) becomes a 1 ms
/// poll, never an immediate-return zero interval. `std::net`'s
/// `set_read_timeout`/`set_write_timeout` likewise reject a zero `Duration`
/// (they return `InvalidInput`), so passing `user.timeout` verbatim would
/// turn a `timeout == 0` request into a hard error instead of C's 1 ms
/// poll. Flooring here is the single owner of that mapping for every
/// IP-family socket-timeout site (client read/write plus the server-mode
/// reads).
///
/// A `None` timeout is C's negative "wait forever" (`pollmsec = -1`,
/// :794 and :664) and passes straight through: there is no interval to
/// floor. A positive sub-millisecond timeout passes through verbatim,
/// which is what the socket-option sites want; the sites that then hand the
/// budget to `poll` round it up to a whole tick through
/// [`super::wait_millis`], because truncating it there would turn the wait
/// into a non-blocking probe.
pub(crate) fn socket_poll_timeout(timeout: Option<Duration>) -> Option<Duration> {
    match timeout {
        Some(t) if t.is_zero() => Some(Duration::from_millis(1)),
        other => other,
    }
}

impl DrvAsynIPPort {
    /// Tear down the live socket. C parity: `drvAsynIPPort.c::closeConnection`
    /// (205-217) destroys the fd and fires `exceptionDisconnect` unless the
    /// link is connect-per-transaction and not being shut down
    /// (`FLAG_SHUTDOWN`, line 214-216).
    ///
    /// For a normal link this drops `connected` to `false` so the actor's
    /// auto-reconnect (`port_actor.rs`) re-establishes it on the next
    /// request. For HTTP (connect-per-transaction) the socket churns once
    /// per request/response, so the logical connection stays *up*: only the
    /// socket (`io.inner`) is released, `connected` is left `true`, and the
    /// `Connect` exception does not flap each transaction. The socket is
    /// reopened lazily on the next write/read (gated on `io.inner.is_none()`,
    /// not `!connected`). `shutdown` is C's `FLAG_SHUTDOWN`, which only
    /// `parseHostInfo` sets before it closes (line 280-281).
    fn drop_connection(&mut self, user: &AsynUser, why: &str, shutdown: bool) {
        self.trace_close(user, why);
        self.io.inner = None;
        if !self.per_transaction() || shutdown {
            self.set_link_connected(user, false);
        }
    }

    /// C `FLAG_CONNECT_PER_TRANSACTION`, which a shut-down port has lost.
    fn per_transaction(&self) -> bool {
        matches!(&self.config, Some(h) if h.config.protocol == IpProtocol::Http)
    }

    /// The driver's one connection-edge owner. C's COM interpose restores its
    /// settings on every `asynExceptionConnect` (asynInterposeCom.c:777-788),
    /// and exceptionDisconnect raises that exception too, so a COM port's drop
    /// also tries the restore, which fails on the closed link and says so.
    fn set_link_connected(&mut self, user: &AsynUser, connected: bool) {
        if self.base.set_connected(connected) {
            self.restore_com_settings(user);
        }
    }

    /// `closeConnection`'s line, printed whether or not a socket is open.
    fn trace_close(&self, user: &AsynUser, why: &str) {
        user.print(
            TraceMask::FLOW,
            file!(),
            line!(),
            format_args!(
                "Closing {} connection (fd {}): {why}\n",
                self.io.host_info,
                self.io.fd()
            ),
        );
    }

    /// The read core behind [`PortDriver::io_read_octet`].
    /// Dispatches the interpose chain once and applies the C
    /// `closeConnection` teardown on a fatal error, a
    /// `disconnectOnReadTimeout` timeout, or a TCP EOF (which the base read
    /// reports as `ASYN_EOM_END`).
    fn read_octet_core(&mut self, user: &AsynUser, buf: &mut [u8]) -> OctetReadResult {
        // HTTP connect-per-transaction: open the socket if there isn't one
        // (C readIt, drvAsynIPPort.c:776-780). The link stays logically
        // connected, so this keys off the socket, not `connected`.
        if self.per_transaction() && self.io.inner.is_none() {
            if let Err(e) = self.connect_it(user) {
                return OctetReadResult::failed(e);
            }
        }
        let mut r = self.with_base_link(|link| link.read(user, buf));
        if let Err(e) = r.status {
            // C runs its test inside readIt, below the interpose, where the
            // raw recv timeout is visible (drvAsynIPPort.c:851-860).
            let failure = classify_read_failure(
                e,
                self.disconnect_on_read_timeout,
                user.timeout,
                &self.io.host_info,
            );
            if failure.disconnect && self.base.is_connected() {
                self.drop_connection(user, "Read error", false);
            }
            r.status = Err(failure.error);
        } else if r.eom_reason.contains(EomReason::END) && self.base.is_connected() {
            // C drvAsynIPPort.c::readIt (815-819): a TCP EOF (reported
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
            self.drop_connection(user, "Read from broken connection", false);
        }
        r
    }

    /// Create a new IP port driver.
    ///
    /// The driver starts disconnected with `auto_connect = true` and `can_block = true`.
    pub fn new(port_name: &str, config_str: &str) -> AsynResult<Self> {
        let config = IpPortConfig::parse(config_str)?;
        let mut base = PortDriverBase::new_exact_flags(
            port_name,
            1,
            PortFlags {
                multi_device: false,
                can_block: true,
                ..PortFlags::default()
            },
            // asynCommon, asynOption and asynOctet, no register interface
            // (drvAsynIPPort.c:1091-1109): a record with IFACE=Int32/UInt32/
            // Float64 gets C's `No asyn<X> interface` (asynRecord.c:1336-1358).
            InterfaceMask::COMMON | InterfaceMask::OPTION | InterfaceMask::OCTET,
            InterfaceMask::OCTET,
        );
        base.init_auto_connect(true);
        // C passes `interruptProcess = 1` to `pasynOctetBase->initialize`
        // (drvAsynIPPort.c:1109): every successful octet read on this port fans out to
        // its octet interrupt users, which is what drives a SCAN="I/O Intr"
        // record. See `PortDriverBase::octet_interrupt_process`.
        base.octet_interrupt_process = true;

        // C `drvAsynIPPortConfigure` (:1115): `if (tty->isCom) asynInterposeCOM(...)`
        // — installed at configure time, before the port ever connects, so the
        // very first byte of the very first transfer is already IAC-escaped
        // (R8-57).
        let com = (config.protocol == IpProtocol::Com).then(|| ComState {
            octet: ComInterpose::new(),
            options: ComPortOptions::new(),
        });

        Ok(Self {
            base,
            config: Some(HostInfo { config, kept: None }),
            resolver: Box::new(system_lookup),
            no_delay: true,
            io: IpIoState::new(config_str.to_string()),
            disconnect_on_read_timeout: false,
            // C parseHostInfo: tty->IPDeviceName = epicsStrDup(hostInfo).
            com,
        })
    }

    /// Build a port configured exactly as C's `drvAsynIPPortConfigure(portName,
    /// hostInfo, priority, noAutoConnect, noProcessEos)` leaves one:
    /// [`Self::new`] plus `Self::apply_ip_port_configure` (the `registerPort`
    /// autoConnect flag and, unless `noProcessEos`, the default EOS interpose —
    /// drvAsynIPPort.c:1082-1120).
    ///
    /// The single public entry for callers outside the iocsh command that need a
    /// fully-configured IP port — e.g. a `noAutoConnect` port registered for a
    /// record to attach to without ever dialing. `priority` belongs to the
    /// port's thread, [`crate::runtime::RuntimeConfig::priority`], not to the
    /// driver. The configure steps stay owned by `apply_ip_port_configure`;
    /// this only sequences `new` before it.
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
    /// driver ([`crate::port::octet_read_chain`], C asynManager.c:2192-2222).
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
    /// apply whatever teardown C's `readIt`/`writeIt` would have applied
    /// during it.
    ///
    /// The single gate for every negotiation in this driver — the connect-time
    /// handshake and each `setOption` — so the socket-teardown rule cannot be
    /// enforced on one path and missed on the other. See [`NegotiationLink`] for
    /// why the decision is carried out rather than applied in place.
    fn with_negotiation<T>(
        &mut self,
        user: &AsynUser,
        f: impl FnOnce(&mut ComPortOptions, &mut NegotiationLink) -> T,
    ) -> Option<T> {
        let com = self.com.as_mut()?;
        let mut link = NegotiationLink {
            io: &mut self.io,
            disconnect_on_read_timeout: self.disconnect_on_read_timeout,
            teardown: None,
        };
        let out = f(&mut com.options, &mut link);
        if let Some(why) = link.teardown {
            if self.base.is_connected() {
                self.drop_connection(user, why, false);
            }
        }
        Some(out)
    }

    /// Run the RFC 2217 handshake on a connection edge.
    ///
    /// C wires this to `asynExceptionConnect` (`asynInterposeCom.c:777-788`):
    /// every time the port connects — including after a terminal server reboots
    /// — the negotiation is replayed so the serial line comes back up with the
    /// settings the IOC last asked for. Its failure is reported and *swallowed*,
    /// exactly as C's `exceptionHandler` does (`asynPrint(ASYN_TRACE_ERROR)`,
    /// :784-786): a terminal server that merely refuses the negotiation still
    /// leaves a usable TCP link, and failing the connect here would instead put
    /// the port into an endless reconnect loop. A negotiation that failed because
    /// the *socket* died is a different matter, and `with_negotiation` has
    /// already closed it by the time we get here — same as C.
    fn restore_com_settings(&mut self, user: &AsynUser) {
        let Some(Err(e)) = self.with_negotiation(user, |com, link| com.restore_settings(link))
        else {
            return;
        };
        asyn_trace!(
            self.base.trace,
            &self.base.port_name,
            TraceMask::ERROR,
            "Unable to restore parameters for port {}: {}\n",
            self.base.port_name,
            e.message()
        );
    }

    /// The `restoreSettings` C's `asynInterposeCOM` runs at configure
    /// (`asynInterposeCom.c:858-866`), for the configuring thread to report.
    /// `None` for a port without the COM layer.
    pub(crate) fn restore_com_settings_at_configure(&mut self) -> Option<AsynResult<()>> {
        self.with_negotiation(&AsynUser::default(), |com, link| com.restore_settings(link))
    }

    /// Push an interpose layer onto the octet I/O stack.
    pub fn install_interpose(&mut self, layer: Box<dyn crate::interpose::OctetInterpose>) {
        self.base.install_octet_interpose(layer);
    }

    /// Everything `drvAsynIPPortConfigure` gives a port beyond constructing the
    /// driver (drvAsynIPPort.c:1082-1120):
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
    /// each slot (drvAsynIPServerPort.c:709-715). A child is not "a port plus
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
        base.init_auto_connect(!no_auto_connect);
        // `asynInterposeEosConfig(portName, -1, 1, 1)` unless suppressed. Without
        // it the port has NO EOS layer at all, so an IEOS set on it is accepted,
        // reads back, and terminates nothing.
        if !no_process_eos {
            base.install_octet_interpose(Box::new(crate::interpose::eos::EosInterpose::default()));
        }
    }

    /// Create the socket C's `connectIt` creates: fresh fd, then every option
    /// the protocol suffix asked for — `SO_BROADCAST` (drvAsynIPPort.c:453-462)
    /// and `SO_REUSEPORT` (:467-481) — applied **before** the socket is bound or
    /// connected, because that is the only time the kernel honours them. Binding
    /// first and setting `SO_REUSEPORT` afterwards, as this driver used to, left
    /// `udp&` with a local port failing `EADDRINUSE` where C binds.
    ///
    /// Single owner of socket creation for both transports, so a new option
    /// cannot be added to one and forgotten on the other.
    ///
    /// C picks SO_REUSEADDR when `USE_SO_REUSEADDR` is defined for the target
    /// and SO_REUSEPORT otherwise (:468-472) — exactly *one* of the pair, unlike
    /// the datagram-fanout helper `drvAsynIPServerPort` uses, which sets both.
    /// Which one is the platform's business, and
    /// [`socket::SocketOptions::reuse_port`] carries C's own
    /// `#ifndef SO_REUSEPORT` degrade rule for it.
    fn socket_options(config: &IpPortConfig) -> socket::SocketOptions {
        socket::SocketOptions {
            broadcast: config.protocol.broadcast(),
            reuse_port: config.protocol.reuse_port(),
            reuse_address: false,
            datagram_fanout: false,
        }
    }

    /// C `connectIt`'s `farAddr` (drvAsynIPPort.c:484-497): the address an
    /// earlier lookup kept, else a lookup now — at the first connect rather
    /// than at configure, "in case a device has just appeared in a DNS
    /// database" — kept for every later connect until a TCP connect to it
    /// fails ([`Self::connect_tcp`]). One address, `hostToIPAddr`'s
    /// ([`one_remote`]), so one connect under one `autoConnectTimeout`; a
    /// numeric host is its own address and asks the resolver nothing. A
    /// failed or empty lookup is C's one message for `hostToIPAddr` failing.
    fn resolve_remote(&mut self) -> AsynResult<std::net::SocketAddr> {
        let host_info = self.config.as_mut().expect("the port is shut down");
        if let Some(addr) = host_info.kept {
            return Ok(addr);
        }
        let IpPortConfig { host, port, .. } = &host_info.config;
        let addr = match host.parse::<std::net::IpAddr>() {
            Ok(ip) => std::net::SocketAddr::new(ip, *port),
            Err(_) => {
                one_remote(&(self.resolver)(host, *port)).ok_or_else(|| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("Unknown host \"{host}\""),
                })?
            }
        };
        host_info.kept = Some(addr);
        Ok(addr)
    }

    fn enable_nodelay(&self, stream: &TcpStream) -> AsynResult<()> {
        stream.set_nodelay(true).map_err(|e| AsynError::Status {
            status: AsynStatus::Error,
            message: nodelay_failure_text(&self.io.host_info, &e),
        })
    }

    fn connect_tcp(&mut self) -> AsynResult<TcpStream> {
        let remote_addr = self.resolve_remote()?;
        let config = self.config();
        // C binds the local address only when one was configured — "a very
        // unusual configuration" (:499-506).
        let local_addr: Option<std::net::SocketAddr> = config.local_port.map(|local_port| {
            if remote_addr.is_ipv6() {
                (std::net::Ipv6Addr::UNSPECIFIED, local_port).into()
            } else {
                (std::net::Ipv4Addr::UNSPECIFIED, local_port).into()
            }
        });
        // No SO_REUSEADDR before the local bind (:499-509): during the last
        // connection's TIME_WAIT, C's reconnect fails the bind.
        let opts = Self::socket_options(config);
        // C bounds the connect by `pasynBase->autoConnectTimeout`, read here
        // and now (drvAsynIPPort.c:541-566).
        let timeout = crate::runtime::config::connect_poll_timeout();
        socket::tcp_connect(remote_addr, local_addr, opts, timeout).map_err(|e| {
            // C :572-574: a failed connect, not a failed setup step, owes the
            // next connect a lookup, "in case the problem is just that the
            // device has DHCP'd itself an new number" (:530-532).
            if !matches!(e, socket::TcpConnectError::Setup(_)) {
                self.config.as_mut().expect("the port is shut down").kept = None;
            }
            AsynError::Status {
                status: AsynStatus::Error,
                message: connect_failure_text(&self.io.host_info, &e),
            }
        })
    }

    fn connect_udp(&mut self) -> AsynResult<(UdpSocket, std::net::SocketAddr)> {
        // C drvAsynIPPort.c::connectIt (484-493) resolves the remote name to
        // tty->farAddr but (513) does NOT connect() a SOCK_DGRAM socket.
        // Resolve the peer once, then leave the socket unconnected and bind
        // a local endpoint of the peer's address family.
        let peer = self.resolve_remote()?;
        let config = self.config();
        let local_port = config.local_port.unwrap_or(0);
        let local_addr: std::net::SocketAddr = if peer.is_ipv6() {
            (std::net::Ipv6Addr::UNSPECIFIED, local_port).into()
        } else {
            (std::net::Ipv4Addr::UNSPECIFIED, local_port).into()
        };
        // The options go on the fresh socket, before the bind — the whole point
        // of `udp&` is two ports sharing one local port, and the kernel only
        // honours SO_REUSEPORT on an unbound socket (C :467-481, then :499-506).
        // That ordering is the seam's contract, not this call site's.
        let socket = socket::udp_socket(local_addr, Self::socket_options(config)).map_err(|e| {
            AsynError::Status {
                status: AsynStatus::Error,
                message: setup_failure_text(&self.io.host_info, &e),
            }
        })?;
        Ok((socket, peer))
    }

    #[cfg(asyn_af_unix)]
    fn connect_unix(&self) -> AsynResult<std::os::unix::net::UnixStream> {
        // C's one connect text for every family (drvAsynIPPort.c:568-571); an
        // AF_UNIX connect fails at once, so its own errno is the one C saves.
        let stream = std::os::unix::net::UnixStream::connect(&self.config().host).map_err(|e| {
            AsynError::Status {
                status: AsynStatus::Error,
                message: format!(
                    "Can't connect to {}: {}",
                    self.io.host_info,
                    super::errno_text(&e)
                ),
            }
        })?;
        Ok(stream)
    }
}

/// `hostToIPAddr`'s answer from a lookup's: its first IPv4 address, else its
/// first address, so a name with only IPv6 ones, which C cannot reach, still
/// connects.
fn one_remote(addrs: &[std::net::SocketAddr]) -> Option<std::net::SocketAddr> {
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .copied()
}

impl DrvAsynIPPort {
    /// Install a freshly dialled transport, and the only place that installs
    /// one.
    ///
    /// **Invariant: `connect` returning `Err` MUST leave `io.inner` empty.**
    /// The socket is published *after* the last step that can fail, which is C
    /// exactly — every fallible step in `connectIt` runs `epicsSocketDestroy(fd)`
    /// and returns (`drvAsynIPPort.c:516-523` for `setNonBlock`, `:580-589` for
    /// `TCP_NODELAY`), and `tty->fd = fd` lands only at `:593`. That ordering is
    /// what makes a failed connect retryable: `tty->fd` is still
    /// `INVALID_SOCKET`, so the already-open guard at `:425-428` does not fire.
    ///
    /// Owning the mode after the socket was installed inverted it. A failure
    /// there left an open socket in `io.inner` with `connected` false, so
    /// `check_ready` refused every transfer, the `is_connected()`-gated
    /// `drop_connection` could never run, and every auto-connect retry answered
    /// `"<port>: Link already open!"` for the life of the IOC.
    fn publish_transport(&mut self, user: &AsynUser, inner: IpIoInner) -> AsynResult<()> {
        // The last fallible step, taken while the transport is still a local
        // that a failure simply drops — closing the socket the way C's
        // `epicsSocketDestroy` does.
        inner.own_blocking_mode(&self.io.host_info)?;
        self.io.inner = Some(inner);
        user.print(
            TraceMask::FLOW,
            file!(),
            line!(),
            format_args!("Opened connection OK to {}\n", self.io.host_info),
        );
        Ok(())
    }

    /// Open the socket (C `connectIt`, drvAsynIPPort.c:406-593); raising the
    /// connect edge is [`PortDriver::connect`]'s.
    fn connect_it(&mut self, user: &AsynUser) -> AsynResult<()> {
        user.print(
            TraceMask::FLOW,
            file!(),
            line!(),
            format_args!(
                "Attempting to connect to {}  reason:{}  fd: {}\n",
                self.io.host_info,
                user.reason,
                self.io.fd()
            ),
        );
        // C drvAsynIPPort.c::connectIt (424-427): reject a connect on an
        // already-open link ("Link already open!") rather than opening a
        // second socket and leaking the first.
        if self.io.inner.is_some() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("{}: Link already open!", self.io.host_info),
            });
        }
        // C :429-432, after the already-open check.
        let Some(protocol) = self.config.as_ref().map(|h| h.config.protocol) else {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("{}: Link shutdown!", self.io.host_info),
            });
        };
        // Built into a local and installed by `publish_transport` below, never
        // stored here: a step after the install that can fail is exactly what
        // wedged the port.
        let inner = match protocol {
            // C :364-367 — `com` leaves `socketType = SOCK_STREAM` and sets no
            // flags, so its transport is plain TCP; the whole of what `COM` adds
            // rides in the interpose, not the socket.
            IpProtocol::Tcp | IpProtocol::TcpReusePort | IpProtocol::Com => {
                let stream = self.connect_tcp()?;
                if self.no_delay {
                    self.enable_nodelay(&stream)?;
                }
                // `tcp&` = TCP + SO_REUSEPORT (C :360-363, :467-481). The option
                // is set by `new_socket` on the unconnected socket, which is the
                // only place it means anything.
                IpIoInner::Tcp(stream)
            }
            IpProtocol::Udp
            | IpProtocol::UdpReusePort
            | IpProtocol::UdpBroadcast
            | IpProtocol::UdpBroadcastReusePort => {
                // BROADCAST / SO_REUSEPORT are the socket's, applied by
                // `new_socket` before the bind — they are not a post-bind fixup.
                let (socket, peer) = self.connect_udp()?;
                IpIoInner::Udp(socket, peer)
            }
            #[cfg(asyn_af_unix)]
            IpProtocol::Unix => IpIoInner::Unix(self.connect_unix()?),
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
            // HTTP opens its socket per transaction, from write_octet and
            // read_octet_core.
            IpProtocol::Http => {
                let stream = self.connect_tcp()?;
                self.enable_nodelay(&stream)?;
                IpIoInner::Tcp(stream)
            }
        };
        self.publish_transport(user, inner)
    }
}

impl PortDriver for DrvAsynIPPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    /// C `asynCommonConnect` (drvAsynIPPort.c:598-607): an HTTP port opens
    /// no socket here, so it is connected whether or not the host answers
    /// and a dead host fails each write instead.
    fn connect(&mut self, user: &AsynUser) -> AsynResult<()> {
        if !self.per_transaction() {
            self.connect_it(user)?;
        }
        self.set_link_connected(user, true);
        Ok(())
    }

    /// C `asynCommonDisconnect` (drvAsynIPPort.c:611-620).
    fn disconnect(&mut self, user: &AsynUser) -> AsynResult<()> {
        let per_transaction = self.per_transaction();
        self.drop_connection(user, "Disconnect request", false);
        if per_transaction {
            self.set_link_connected(user, false);
        }
        Ok(())
    }

    /// C `asynCommonReport` (drvAsynIPPort.c:223-239): nothing at details 0 —
    /// the IP port is no `asynPortDriver`, so it has no name line or parameter
    /// list — and "connected" is whether a socket is open.
    fn report(&mut self, out: &mut dyn std::fmt::Write, level: i32) {
        use std::fmt::Write as _;
        if level >= 1 {
            let _ = writeln!(
                out,
                "    Port {}: {}onnected",
                self.io.host_info,
                if self.io.inner.is_some() { "C" } else { "Disc" }
            );
        }
        if level >= 2 {
            let _ = writeln!(out, "                    fd: {}", self.io.fd());
            let _ = writeln!(out, "    Characters written: {}", self.io.n_written);
            let _ = writeln!(out, "       Characters read: {}", self.io.n_read);
        }
    }

    fn io_read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> OctetReadResult {
        // Override the synthetic default so the real END/EOS reason from the
        // base read + interpose chain reaches the actor (C reports eomReason
        // from readIt; END marks a TCP EOF, EOS an input-EOS match).
        self.read_octet_core(user, buf)
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        // HTTP connect-per-transaction: open the socket if there isn't one
        // (C writeIt, drvAsynIPPort.c:645-653, which replaces connectIt's
        // message).
        if self.per_transaction() && self.io.inner.is_none() {
            self.connect_it(user).map_err(|e| AsynError::Status {
                status: e.status(),
                message: format!("{} connect failed", self.io.host_info),
            })?;
        }
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
                    self.drop_connection(user, "Write error", false);
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
        // below (`setOption`'s trailing else, :659-666). Same order here: the
        // seven serial keys negotiate, `hostInfo`/`disconnectOnReadTimeout` fall
        // through to the IP chain — which is why this is one dispatch and not two
        // independent option maps.
        if self.com.is_some() && ComPortOptions::owns_key(key) {
            // Same teardown gate as the connect-time handshake: C's setOption
            // reads run through the same `readIt` below the interpose.
            // The close line prints through the request's trace link while
            // the request itself is lent to the negotiation.
            let mut who = AsynUser::new(user.reason).with_addr(user.addr);
            who.trace = user.trace.clone();
            return self
                .with_negotiation(&who, |com, link| com.set_option(user, link, key, value))
                .expect("com is Some");
        }
        user.print(
            TraceMask::FLOW,
            file!(),
            line!(),
            format_args!("{} setOption key {key} val {value}\n", self.base.port_name),
        );
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
            self.no_delay = enabled;
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
            // C drvAsynIPPort.c::parseHostInfo (273-401): close a live socket
            // with FLAG_SHUTDOWN, reset the counters and IPDeviceName, shut the
            // port down, then parse; only a good parse reopens it.
            if self.io.inner.is_some() {
                self.drop_connection(
                    user,
                    "drvAsynIPPort::parseHostInfo, closing socket to open new connection",
                    true,
                );
                // C's "if this delay is not present then the sockets are not
                // always really closed cleanly".
                std::thread::sleep(Duration::from_millis(20));
            }
            self.io.n_read = 0;
            self.io.n_written = 0;
            self.io.host_info = value.to_string();
            self.config = None;
            self.config = Some(HostInfo {
                config: IpPortConfig::parse(value)?,
                kept: None,
            });
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
    /// (`epicsStrCaseCmp`); any other key is C's "Unsupported key" error.
    fn get_option(&self, _user: &AsynUser, key: &str) -> AsynResult<String> {
        // Same interposed order as `set_option`. C's COM `getOption` (:671-739)
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
            Ok(self.io.host_info.clone())
        } else {
            Err(AsynError::OptionNotFound(key.to_string()))
        }
    }
}

impl DrvAsynIPPort {
    /// The parsed hostInfo of a port that is not shut down.
    fn config(&self) -> &IpPortConfig {
        &self.config.as_ref().expect("the port is shut down").config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// Enter the port's octet interface the way a caller does — through the
    /// interpose chain, with the driver at the bottom of it. C `findInterface`
    /// hands the caller the *topmost* interpose (asynManager.c:1494-1503,
    /// 2190-2220); the driver's own `read`/`write`/`flush` are the base the chain
    /// ends at, so a test that wants the EOS layer must enter above it, exactly
    /// as `PortActor` does.
    fn chain_read(drv: &mut DrvAsynIPPort, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        let r = crate::port::octet_read_chain(drv, user, buf);
        r.status.map(|()| r.nbytes_transferred)
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
        let r = crate::port::octet_read_chain(drv, user, buf);
        r.status.map(|()| (r.nbytes_transferred, r.eom_reason))
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

    /// Both ports are C `sscanf(cp, ":%d")` (drvAsynIPPort.c:334, :340): a
    /// prefix scan that skips whitespace and takes a sign.
    #[test]
    fn ports_are_read_as_c_percent_d() {
        let ports = |spec: &str| {
            IpPortConfig::parse(spec)
                .map(|c| (c.port, c.local_port))
                .map_err(|_| ())
        };
        assert_eq!(ports("host:5025x"), Ok((5025, None)));
        assert_eq!(ports("host:+5025:6000x"), Ok((5025, Some(6000))));
        assert_eq!(ports("host:\t5025"), Ok((5025, None)));
        assert_eq!(ports("host:5025x:6000:7"), Ok((5025, Some(6000))));
        assert_eq!(ports("[::1]:5025x:6000x"), Ok((5025, Some(6000))));
        if std::ffi::c_long::BITS == 64 {
            // glibc's (int)strtol keeps the low 32 bits.
            assert_eq!(ports("host:4294972321"), Ok((5025, None)));
        }
        for bad in [
            "host:5025:",
            "host:x5025",
            "host:70000",
            "host:-1",
            "host:5025:-1",
        ] {
            assert_eq!(ports(bad), Err(()), "{bad}");
        }
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
        assert!(drv.base().is_auto_connect());
        assert!(drv.base().flags.can_block);
    }

    /// drvAsynIPPort.c:1091-1109: asynCommon, asynOption and asynOctet, whose
    /// `interruptProcess = 1` makes asynOctet an interrupt source.
    #[test]
    fn the_port_registers_drv_asyn_ip_port_s_interfaces() {
        let drv = DrvAsynIPPort::new("iptest", "localhost:5025").unwrap();
        assert_eq!(
            (drv.base().interface_mask(), drv.base().interrupt_mask()),
            (
                InterfaceMask::COMMON | InterfaceMask::OPTION | InterfaceMask::OCTET,
                InterfaceMask::OCTET
            )
        );
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
        assert!(!drv.base().is_auto_connect());

        // (3) The IP options the asyn record renders as HOSTINFO / DRTO.
        assert_eq!(
            drv.get_option(&AsynUser::default(), "hostinfo").unwrap(),
            "localhost:1"
        );
        assert_eq!(
            drv.get_option(&AsynUser::default(), "disconnectOnReadTimeout")
                .unwrap(),
            "N"
        );
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
        let n = drv.io_read_octet(&user, &mut buf).into_count().unwrap();
        assert_eq!(&buf[..n], b"hello");

        handle.join().unwrap();
    }

    /// C `asynCommonReport`: silent at 0, the endpoint and socket state at 1,
    /// the fd and the byte counts at 2.
    #[test]
    fn report_prints_what_c_asyn_common_report_prints() {
        let (listener, port) = start_echo_server();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let n = stream.read(&mut buf).unwrap();
            stream.write_all(&buf[..n]).unwrap();
        });
        let host = format!("127.0.0.1:{port}");
        let mut drv = DrvAsynIPPort::new("iprep", &host).unwrap();
        let report = |drv: &mut DrvAsynIPPort, level| {
            let mut out = String::new();
            drv.report(&mut out, level);
            out
        };
        assert_eq!(report(&mut drv, 0), "");
        assert_eq!(
            report(&mut drv, 1),
            format!("    Port {host}: Disconnected\n")
        );

        drv.connect(&AsynUser::default()).unwrap();
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user, b"hello").unwrap();
        let mut buf = [0u8; 32];
        let n = drv.io_read_octet(&user, &mut buf).into_count().unwrap();
        assert_eq!(n, 5);
        handle.join().unwrap();

        let fd = drv.io.fd();
        assert!(fd >= 0);
        assert_eq!(
            report(&mut drv, 2),
            format!(
                "    Port {host}: Connected\n                    fd: {fd}\n    \
                 Characters written: 5\n       Characters read: 5\n"
            )
        );
    }

    /// C `asynCommonConnect` skips `connectIt` for HTTP (drvAsynIPPort.c:603):
    /// the port connects without its host and the write fails with
    /// asynError, not asynDisconnected.
    #[test]
    fn an_http_port_connects_without_its_host_and_the_write_fails() {
        use crate::request::RequestOp;
        use crate::runtime::config::RuntimeConfig;
        use crate::runtime::port::create_port_runtime_unclaimed;

        let drv = DrvAsynIPPort::new("httpdead", "127.0.0.1:1 HTTP").unwrap();
        let (runtime, _) = create_port_runtime_unclaimed(drv, RuntimeConfig::default()).unwrap();
        let handle = runtime.port_handle();
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let e = handle
            .submit_blocking(
                RequestOp::OctetWrite {
                    data: b"X".to_vec(),
                },
                user,
            )
            .unwrap_err();
        assert_eq!(e.status(), AsynStatus::Error);
        assert_eq!(e.message(), "127.0.0.1:1 HTTP connect failed");
        assert!(handle.is_connected_blocking(0).unwrap());
    }

    /// C `readIt` opens a per-transaction socket itself
    /// (drvAsynIPPort.c:776-780): a read with no socket connects instead of
    /// failing "disconnected".
    #[test]
    fn an_http_read_without_a_socket_opens_one() {
        let (listener, port) = start_echo_server();
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(b"HELLO").unwrap();
        });
        let mut drv = DrvAsynIPPort::new("httpread", &format!("127.0.0.1:{port} HTTP")).unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        assert!(drv.io.inner.is_none());
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 5];
        let (n, _) = drv.io_read_octet(&user, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"HELLO");
        server.join().unwrap();
    }

    /// C `parseHostInfo` sets `flags = FLAG_SHUTDOWN` before it parses and
    /// clears it only on success (drvAsynIPPort.c:286, :399-400): a bad
    /// hostInfo leaves even an HTTP port shut down, and `connectIt` refuses.
    #[test]
    fn a_bad_host_info_leaves_the_port_shut_down() {
        let mut drv = DrvAsynIPPort::new("badreparse", "127.0.0.1:1 HTTP").unwrap();
        let mut user = AsynUser::default();
        assert!(drv.set_option(&mut user, "hostInfo", "no-colon").is_err());
        let e = drv.connect(&user).unwrap_err();
        assert_eq!(e.status(), AsynStatus::Error);
        assert_eq!(e.message(), "no-colon: Link shutdown!");
        assert!(!drv.base().is_connected());
    }

    /// C `asynCommonDisconnect` disconnects a per-transaction link itself,
    /// since `closeConnection` does not (drvAsynIPPort.c:616-618).
    #[test]
    fn a_disconnect_request_disconnects_an_http_port() {
        let mut drv = DrvAsynIPPort::new("httpdisc", "127.0.0.1:1 HTTP").unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        drv.disconnect(&AsynUser::default()).unwrap();
        assert!(!drv.base().is_connected());
    }

    /// C `parseHostInfo` closes a live socket with FLAG_SHUTDOWN set, so
    /// `closeConnection` disconnects even a per-transaction link
    /// (drvAsynIPPort.c:214-216, :280-281).
    #[test]
    fn a_host_info_change_disconnects_a_live_http_port() {
        let (listener, port) = start_echo_server();
        let server = thread::spawn(move || {
            let _ = listener.accept().unwrap();
        });
        let host = format!("127.0.0.1:{port} HTTP");
        let mut drv = DrvAsynIPPort::new("httpreparse", &host).unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user, b"GET /\r\n").unwrap();
        server.join().unwrap();
        drv.set_option(&mut user, "hostInfo", &host).unwrap();
        assert!(drv.io.inner.is_none());
        assert!(!drv.base().is_connected());
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
        let (n1, eom1) = drv.io_read_octet(&ruser, &mut buf).unwrap();
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

        let (n2, _) = drv.io_read_octet(&ruser, &mut buf).unwrap();
        assert_eq!(
            &buf[..n2],
            b"BBBB",
            "second chunk must be the rest of the response"
        );

        // Third read hits the server's EOF -> END; the socket is released but
        // the logical connection stays up (connect-per-transaction = no flap).
        let (n3, eom3) = drv.io_read_octet(&ruser, &mut buf).unwrap();
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
        let (n4, _) = drv.io_read_octet(&ruser, &mut buf2).unwrap();
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
        let err = drv.io_read_octet(&user, &mut buf).unwrap_err();
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
        // C drvAsynIPPort.c::readIt (815-821): a TCP EOF returns success
        // with zero bytes and ASYN_EOM_END, then closes the connection.
        let (n, eom) = drv.io_read_octet(&user, &mut buf).unwrap();
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
        let n1 = drv.io_read_octet(&user, &mut buf).into_count().unwrap();
        assert!(n1 > 0);
        assert!(n1 <= 5);

        handle.join().unwrap();
    }

    /// R8-57 end-to-end over a real socket: connecting a `COM` port must run C's
    /// RFC 2217 handshake (`asynInterposeCom.c::restoreSettings`, :743-772) on
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
                0, // :754  IAC DO  BINARY
                IAC,
                WILL_B,
                0, // :756  IAC WILL BINARY
                IAC,
                WILL_B,
                COM_PORT_OPTION, // :758  IAC WILL COM-PORT-OPTION
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
        assert_eq!(
            drv.get_option(&AsynUser::default(), "baud").unwrap(),
            "9600"
        );
        assert_eq!(drv.get_option(&AsynUser::default(), "bits").unwrap(), "8");
        assert_eq!(
            drv.get_option(&AsynUser::default(), "parity").unwrap(),
            "none"
        );
        assert_eq!(drv.get_option(&AsynUser::default(), "stop").unwrap(), "1");

        // Data path out: 3 caller bytes, one of them an IAC. The interpose
        // reports the caller's count, not the 4 bytes it put on the wire.
        let mut wuser = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let n = drv.write_octet(&mut wuser, &[b'A', IAC, b'B']).unwrap();
        assert_eq!(n, 3);

        // Data path in: 4 wire bytes unstuff to 3.
        let ruser = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 16];
        let n = drv.io_read_octet(&ruser, &mut buf).into_count().unwrap();
        assert_eq!(&buf[..n], &[b'X', IAC, b'Y']);

        server.join().unwrap();
    }

    /// C's COM interpose restores on every `asynExceptionConnect`
    /// (asynInterposeCom.c:777-788), and exceptionDisconnect raises it too: a
    /// COM port's disconnect tries the restore on the closed link and reports
    /// the failure, while the connect that negotiated reports nothing.
    #[test]
    fn a_com_port_disconnect_reports_the_failed_restore() {
        use crate::interpose::com::{IAC, SB, SE};
        use crate::trace::TraceFile;

        let ack = |subcmd: u8, values: &[u8]| {
            let mut v = vec![IAC, SB, 44, subcmd + 100];
            v.extend_from_slice(values);
            v.extend_from_slice(&[IAC, SE]);
            v
        };
        let (listener, port) = start_echo_server();
        let mut reply = vec![IAC, 251, 0, IAC, 253, 0, IAC, 253, 44];
        reply.extend(ack(11, &[0]));
        reply.extend(ack(1, &[0x00, 0x00, 0x25, 0x80]));
        reply.extend(ack(2, &[8]));
        reply.extend(ack(3, &[1]));
        reply.extend(ack(4, &[1]));
        reply.extend(ack(5, &[1]));
        reply.extend(ack(5, &[1]));
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(&reply).unwrap();
            let mut sink = Vec::new();
            let _ = stream.read_to_end(&mut sink);
        });

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.log");
        let file =
            std::sync::Arc::new(std::sync::Mutex::new(std::fs::File::create(&path).unwrap()));
        let mut drv = DrvAsynIPPort::new("comdrop", &format!("127.0.0.1:{port} COM")).unwrap();
        let trace = drv.base().trace.clone();
        trace.set_trace_file(Some("comdrop"), TraceFile::File(file));
        trace.set_trace_mask(Some("comdrop"), TraceMask::ERROR);
        let user = AsynUser::default().with_timeout(Duration::from_secs(2));
        drv.connect(&user).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "",
            "the restore succeeded"
        );

        drv.disconnect(&user).unwrap();
        server.join().unwrap();
        let log = std::fs::read_to_string(&path).unwrap();
        assert!(
            log.contains("Unable to restore parameters for port comdrop: "),
            "{log}"
        );
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
            // carry on" path (C exceptionHandler, :784-786) and the link stays
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
        assert_eq!(
            drv.get_option(&AsynUser::default(), "baud").unwrap(),
            "115200"
        );

        server.join().unwrap();
    }

    /// R8-57 boundary: C runs the negotiation's reads through
    /// `readIt`, so `readIt`'s teardown gate (drvAsynIPPort.c:851-860)
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
        // applies readIt's teardown to the negotiation's reads too.
        let (port, server) = silent_server();
        let mut drv = DrvAsynIPPort::new("comto2", &format!("127.0.0.1:{port} COM")).unwrap();
        drv.set_option(&mut AsynUser::default(), "disconnectOnReadTimeout", "Y")
            .unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        assert!(
            !drv.base.is_connected(),
            "a timed-out negotiation read must close the socket, as C readIt does"
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
    /// inside readIt, below the interpose, on the raw recv timeout
    /// (drvAsynIPPort.c:851-860); asyn-rs runs it above the interpose, where
    /// the same timeout arrives with the interpose's transfer beside it — so
    /// the test must key off `e.status()`, not the error variant. Matching by
    /// variant made the option inert on every default IP port.
    #[test]
    fn disconnect_on_read_timeout_fires_through_the_eos_interpose() {
        use crate::interpose::eos::{EosConfig, EosInterpose};

        let (listener, port) = start_echo_server();
        // The peer accepts and then says nothing: the read must time out with
        // zero bytes, which the EOS interpose reports as a Timeout with 0 bytes.
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
            .io_read_octet(&user, &mut buf)
            .into_count()
            .expect_err("a silent peer must time the read out");
        // R11-47: the branch that closes the socket is the branch that reports
        // asynError (drvAsynIPPort.c:805) — the lower layer's asynTimeout does
        // not survive the teardown.
        assert_eq!(
            err.status(),
            AsynStatus::Error,
            "C readIt overwrites the status on the disconnect branch"
        );
        // C's teardown text, `"%s read error: %s"` with the recv errno's
        // `strerror` (drvAsynIPPort.c:852-859).
        assert_eq!(
            err.message(),
            format!("127.0.0.1:{port} read error: {}", would_block_text())
        );
        assert!(
            !drv.base().is_connected(),
            "disconnectOnReadTimeout must drop the connection (C drvAsynIPPort.c:851-860)"
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
        let err = drv.io_read_octet(&user, &mut buf).unwrap_err();
        // R11-47 negative control: the non-teardown branch keeps C's asynTimeout
        // (drvAsynIPPort.c:865) — the restamp is the teardown branch's, not every
        // failed read's.
        assert_eq!(err.status(), AsynStatus::Timeout);
        // `"%s timeout: %s"` with the recv errno's `strerror`
        // (drvAsynIPPort.c:861-865).
        assert_eq!(
            err.message(),
            format!("127.0.0.1:{port} timeout: {}", would_block_text())
        );
        assert!(
            drv.base().is_connected(),
            "a plain read timeout leaves the socket intact (C returns asynTimeout, no close)"
        );

        handle.join().unwrap();
    }

    /// R11-47: the restamp keeps the bytes the device did send. C assigns
    /// `*nbytesTransferred = thisRead` (drvAsynIPPort.c:878) *below* both branches,
    /// so a device that emits half a line and goes quiet reaches the record as
    /// asynError **plus** its partial line — asynRecord commits NORD/EOMR
    /// regardless of status (asynRecord.c:1591,1627).
    #[test]
    fn a_torn_down_read_reports_asyn_error_and_still_carries_the_partial_bytes() {
        use crate::interpose::eos::{EosConfig, EosInterpose};
        use std::io::Write;

        let (listener, port) = start_echo_server();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"OK").unwrap();
            thread::sleep(Duration::from_millis(500));
        });
        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        drv.install_interpose(Box::new(EosInterpose::new(EosConfig {
            input_eos: vec![b'\n'],
            output_eos: vec![],
        })));
        drv.set_option(&mut AsynUser::default(), "disconnectOnReadTimeout", "Y")
            .unwrap();
        drv.connect(&AsynUser::default()).unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_millis(200));
        let mut buf = [0u8; 32];
        let r = crate::port::octet_read_chain(&mut drv, &user, &mut buf);
        assert_eq!(
            &buf[..r.nbytes_transferred],
            b"OK",
            "the transfer C writes out at :878 survives the restamp"
        );
        let e = r.unwrap_err();
        assert_eq!(e.status(), AsynStatus::Error, "C :860");
        assert_eq!(
            e.message(),
            format!("127.0.0.1:{port} read error: {}", would_block_text()),
            "C :856-858"
        );
        assert!(!drv.base().is_connected());
        handle.join().unwrap();
    }

    #[test]
    fn the_read_failure_rule_matches_c_should_disconnect_at_every_boundary() {
        let timeout = || AsynError::Status {
            status: AsynStatus::Timeout,
            message: "read timeout".into(),
        };
        let fatal = || AsynError::Io(std::io::Error::other("cable yanked"));
        let dev = "host:1";

        // option off → no teardown, status unchanged, whatever the timeout.
        let f = classify_read_failure(timeout(), false, Some(Duration::from_secs(1)), dev);
        assert!(!f.disconnect);
        assert_eq!(f.error.status(), AsynStatus::Timeout);

        // option on but timeout == 0 (a poll) → C's `pasynUser->timeout > 0`
        // fails: no teardown, asynTimeout stands.
        let f = classify_read_failure(timeout(), true, Some(Duration::ZERO), dev);
        assert!(!f.disconnect);
        assert_eq!(f.error.status(), AsynStatus::Timeout);

        // option on, negative timeout (`None`, C's wait forever) → the same
        // `pasynUser->timeout > 0` test fails: a forever-wait keeps its socket
        // (DRV-42).
        let f = classify_read_failure(timeout(), true, None, dev);
        assert!(!f.disconnect);
        assert_eq!(f.error.status(), AsynStatus::Timeout);

        // option on, timeout > 0 → teardown + asynError.
        let f = classify_read_failure(timeout(), true, Some(Duration::from_millis(1)), dev);
        assert!(f.disconnect);
        assert_eq!(f.error.status(), AsynStatus::Error);

        // a real errno tears down with the option off — C's second disjunct.
        let f = classify_read_failure(fatal(), false, Some(Duration::ZERO), dev);
        assert!(f.disconnect);
        assert_eq!(f.error.status(), AsynStatus::Error);
        assert_eq!(f.error.message(), "host:1 read error: cable yanked");
    }

    #[test]
    fn test_read_write_when_disconnected() {
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:9999").unwrap();
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let mut buf = [0u8; 32];
        assert!(drv.io_read_octet(&user, &mut buf).into_count().is_err());
        let mut user = AsynUser::new(0);
        assert!(drv.write_octet(&mut user, b"hello").is_err());
    }

    #[test]
    fn test_set_option_nodelay() {
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:5025").unwrap();
        drv.set_option(&mut AsynUser::default(), "noDelay", "Y")
            .unwrap();
        assert!(drv.no_delay);
        drv.set_option(&mut AsynUser::default(), "noDelay", "n")
            .unwrap();
        assert!(!drv.no_delay);
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
        let n = drv.io_read_octet(&user, &mut buf).into_count().unwrap();
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

        // The driver binds an ephemeral local port; the "other peer" below
        // answers to the port it got. A probe bound and dropped to pick a fixed
        // port leaves it for a parallel test to take first (EADDRINUSE).
        let mut drv =
            DrvAsynIPPort::new("udptest", &format!("127.0.0.1:{server_port} udp")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        let Some(IpIoInner::Udp(socket, _)) = &drv.io.inner else {
            panic!("a udp port holds a datagram socket");
        };
        let local_port = socket.local_addr().unwrap().port();

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

        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user, b"ping").unwrap();

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 32];
        let n = drv.io_read_octet(&user, &mut buf).into_count().unwrap();
        assert_eq!(&buf[..n], b"pong");

        handle.join().unwrap();
    }

    #[test]
    fn test_udp_empty_datagram_is_not_eof() {
        // DRV-4: a zero-length UDP datagram is a legitimate read of 0 bytes,
        // not a connection close. C readIt only treats recv()==0 as EOF for
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
        let n = drv.io_read_octet(&ruser, &mut buf).into_count().unwrap();
        assert_eq!(n, 0);
        // The port must remain open — a 0-byte datagram is not EOF.
        assert!(drv.base().is_connected());

        // And it can still read a subsequent real datagram.
        server.send_to(b"world", src).unwrap();
        let n = drv.io_read_octet(&ruser, &mut buf).into_count().unwrap();
        assert_eq!(&buf[..n], b"world");
    }

    #[test]
    fn test_udp_zero_length_write_sends_nothing() {
        // DRV-14: C writeIt (613-614) returns success on a zero-length
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
        let _ = drv.io_read_octet(&user, &mut buf);
        assert!(!drv.base().is_connected());
    }

    #[test]
    fn socket_poll_timeout_floors_zero_to_one_ms() {
        // C drvAsynIPPort.c::readIt/writeIt floor `(int)(timeout*1000)==0`
        // to a 1 ms poll; std rejects a zero Duration in set_read_timeout.
        assert_eq!(
            socket_poll_timeout(Some(Duration::ZERO)),
            Some(Duration::from_millis(1))
        );
        // A negative "wait forever" has no interval to floor: it passes through
        // as C's `pollmsec = -1` (DRV-42).
        assert_eq!(socket_poll_timeout(None), None);
        // Positive timeouts pass through verbatim; the whole-millisecond
        // rounding the poll sites need belongs to `wait_millis`, not here.
        assert_eq!(
            socket_poll_timeout(Some(Duration::from_micros(500))),
            Some(Duration::from_micros(500))
        );
        assert_eq!(
            socket_poll_timeout(Some(Duration::from_secs(2))),
            Some(Duration::from_secs(2))
        );
    }

    /// D5 family: the IP transports reach `poll` through the same
    /// `Duration` -> milliseconds conversion the serial driver did, and
    /// truncating a 500 us budget to 0 made the read bound non-blocking — the
    /// recv that follows then reports asynTimeout without the socket ever
    /// having been waited on. `poll` waits at least its interval, so the call
    /// cannot return in the few microseconds the truncated form took.
    #[test]
    fn bound_next_read_waits_a_whole_tick_for_a_sub_millisecond_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let _accepted = listener.accept().unwrap();

        let start = std::time::Instant::now();
        client.bound_next_read(Some(Duration::from_micros(500)));
        let elapsed = start.elapsed();

        assert!(
            elapsed >= Duration::from_micros(400),
            "a 500 us read bound must wait, not poll non-blocking: {elapsed:?}"
        );
    }

    #[test]
    fn zero_timeout_read_polls_without_disconnect() {
        // C drvAsynIPPort.c::readIt: a `timeout == 0` read becomes a 1 ms
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
        let err = drv.io_read_octet(&user, &mut buf).unwrap_err();
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
        // C readIt (798-800) excludes EWOULDBLOCK *and* EINTR from the fatal
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

    /// A peer that accepts `chunk` bytes per pass, each pass becoming
    /// writable `delay` after the wait starts.
    struct DribblePeer {
        chunk: usize,
        delay: Duration,
    }

    impl WaitWritable for DribblePeer {
        fn wait_writable(&self, deadline: Option<std::time::Instant>) -> std::io::Result<bool> {
            let ready = std::time::Instant::now() + self.delay;
            if deadline.is_some_and(|d| ready > d) {
                // What `poll` does when its interval expires first.
                let deadline = deadline.expect("checked above");
                std::thread::sleep(deadline.saturating_duration_since(std::time::Instant::now()));
                return Ok(false);
            }
            std::thread::sleep(self.delay);
            Ok(true)
        }
    }

    impl SendWithoutParking for DribblePeer {
        fn send_without_parking(&self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(self.chunk.min(buf.len()))
        }
    }

    /// DRV-58. C's `writePollmsec` bounds one `poll(POLLOUT)` and is re-armed
    /// on every pass (drvAsynIPPort.c:662-663, 686-701), so a peer that keeps
    /// taking bytes never times out however long the transfer runs. One
    /// deadline built at write-start instead expired mid-transfer and returned
    /// `asynTimeout` with a partial count where C returns success — a WRITE
    /// alarm and NAWT below NOWT on an `asynOctetWrite` record.
    #[test]
    fn a_slow_but_progressing_peer_never_times_out() {
        let peer = DribblePeer {
            chunk: 1,
            delay: Duration::from_millis(20),
        };
        // Six passes of 20 ms against a 50 ms bound: every individual wait is
        // inside the bound, the whole transfer is far outside it.
        let n = write_with_retry(&peer, b"abcdef", Some(Duration::from_millis(50)), None)
            .expect("a peer that keeps accepting bytes must not time out");
        assert_eq!(n, 6);
    }

    /// The other half of the same bound: when one wait really does outlast it,
    /// C reports `asynTimeout` with `*nbytesTransferred` (`:697-701`, `:727`).
    #[test]
    fn a_peer_that_stalls_longer_than_one_poll_times_out_with_its_count() {
        let peer = DribblePeer {
            chunk: 1,
            delay: Duration::from_millis(50),
        };
        let err = write_with_retry(&peer, b"abcdef", Some(Duration::from_millis(5)), None)
            .expect_err("a wait longer than the bound is C's asynTimeout");
        // C returns from inside the loop (:697-701): no end line.
        let WriteFailure::Returned(err) = err else {
            panic!("a poll timeout is C's return, got {err:?}");
        };
        assert_eq!(err.status(), AsynStatus::Timeout);
        assert_eq!(err.partial_write(), Some(0));
    }

    /// A socket `poll` calls writable and `send` then refuses.
    ///
    /// Darwin's `poll(POLLOUT)` reports writable at `SO_SNDLOWAT`, not at an
    /// empty buffer, so this pair is what a real macOS write to a peer that has
    /// stopped reading does. The per-pass deadline cannot bound it — each pass
    /// re-arms a fresh `now + poll` and neither the wait nor the send ever
    /// blocks — so before C's `haveStartTime` retry bound was carried across,
    /// this call did not return at all, on any timeout the caller gave it.
    struct WritableButRefusing;

    impl WaitWritable for WritableButRefusing {
        fn wait_writable(&self, _deadline: Option<std::time::Instant>) -> std::io::Result<bool> {
            Ok(true)
        }
    }

    impl SendWithoutParking for WritableButRefusing {
        fn send_without_parking(&self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
        }
    }

    #[test]
    fn a_writable_socket_that_keeps_refusing_bytes_expires_on_the_retry_bound() {
        let started = std::time::Instant::now();
        let err = write_with_retry(
            &WritableButRefusing,
            b"abcdef",
            Some(Duration::from_millis(50)),
            None,
        )
        .expect_err("a send that never accepts a byte is C's asynTimeout");
        // C's `thisWrite = 0; break;` (:719-721), which prints the end line.
        let WriteFailure::Broke(err) = err else {
            panic!("the retry bound is C's break, got {err:?}");
        };
        assert_eq!(err.status(), AsynStatus::Timeout);
        assert_eq!(err.partial_write(), Some(0));
        // The bound is cumulative from the first refusal, so it expires once
        // rather than once per pass.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the retry bound did not hold: {:?}",
            started.elapsed()
        );
    }

    /// The caller's own budget still caps the transfer where one exists — the
    /// server port's broadcast states a single timeout for all its slots.
    #[test]
    fn a_caller_budget_still_caps_a_progressing_transfer() {
        let peer = DribblePeer {
            chunk: 1,
            delay: Duration::from_millis(20),
        };
        let cap = std::time::Instant::now() + Duration::from_millis(50);
        let err = write_with_retry(&peer, b"abcdef", Some(Duration::from_millis(50)), Some(cap))
            .expect_err("the caller's budget bounds the whole call")
            .into_error();
        assert_eq!(err.status(), AsynStatus::Timeout);
        assert!(
            matches!(err.partial_write(), Some(n) if (1..6).contains(&n)),
            "the accepted count rides out on the failure, got {:?}",
            err.partial_write()
        );
    }

    #[test]
    fn zero_timeout_write_attempts_send_not_instant_timeout() {
        // DRV-11 (write side): C `writeIt` floors a zero timeout to a 1 ms
        // poll then attempts the send (:662-663, :703-705), so a `timeout == 0` write of
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
    /// This driver used to arm the option on every write and propagate a
    /// `setsockopt` failure with `?`. C's write bound is
    /// `poll(POLLOUT, writePollmsec)` (`:684-701`) under `USE_POLL`; it
    /// compiles the option only under `USE_SOCKTIMEOUT`, which is
    /// `#if defined(__rtems__)` (`drvAsynIPPort.c:71-72`).
    ///
    /// Linux implements the option, so the pre-fix code also returned Timeout
    /// here. What fails before the fix is the `write_timeout()` assertion —
    /// arming the option AT ALL is the defect.
    ///
    /// Darwin included, and it is the case that proves the mode is what carries
    /// the bound: there `SO_SNDTIMEO` is absent by this test's own construction
    /// *and* `MSG_DONTWAIT` is ignored by XNU, so before
    /// [`IpIoInner::own_blocking_mode`] this write parked with nothing to end
    /// it.
    #[test]
    fn a_stalled_write_bounds_itself_without_arming_so_sndtimeo() {
        let (listener, port) = start_echo_server();
        // The peer accepts and never reads. It must hold the connection open
        // until the write has run: a dropped socket would make the write fail
        // with EPIPE instead of stalling.
        let (release, wait_for_release) = std::sync::mpsc::channel::<()>();
        let handle = thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let _ = wait_for_release.recv();
            drop(sock);
        });

        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        // Stalled by refusal, not by size: a payload sized to outrun the
        // buffers fitted whole into Winsock's loopback.
        let Some(IpIoInner::Tcp(stream)) = drv.io.inner.as_ref() else {
            panic!("a TCP connect installs the TCP arm");
        };
        fill_until_refused(stream);

        let payload = vec![0x5Au8; 64 * 1024];
        let timeout = Duration::from_millis(200);
        let mut user = AsynUser::new(0).with_timeout(timeout);
        let start = std::time::Instant::now();
        let res = drv.write_octet(&mut user, &payload);
        let elapsed = start.elapsed();

        let err = match res {
            Err(e) => e,
            other => panic!("a peer that never reads must stall the write, got {other:?}"),
        };
        let AsynError::PartialWrite { source, .. } = &err else {
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
             (drvAsynIPPort.c:697-701), got {source:?}"
        );
        // The deadline was waited out, not refused at once. The accepted count
        // is the kernel's (Linux zero, Darwin some), so it is not asserted.
        assert!(
            elapsed >= timeout - Duration::from_millis(50),
            "the write must wait its deadline ({timeout:?}), took {elapsed:?}"
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
            "the write path must not arm SO_SNDTIMEO — C only sets it under \
             __rtems__"
        );

        let _ = release.send(());
        handle.join().unwrap();
    }

    #[test]
    fn zero_length_read_request_rejected_not_eof_teardown() {
        // DRV-55: C readIt (736-740) rejects maxchars == 0 with asynError
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
        let res = drv.io_read_octet(&ruser, &mut empty).into_count();
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
        assert_eq!(
            drv.get_option(&AsynUser::default(), "disconnectOnReadTimeout")
                .unwrap(),
            "Y"
        );
        drv.set_option(&mut AsynUser::default(), "disconnectOnReadTimeout", "n")
            .unwrap();
        assert_eq!(
            drv.get_option(&AsynUser::default(), "disconnectOnReadTimeout")
                .unwrap(),
            "N"
        );

        // Values C never accepts must now error instead of mapping to "off".
        for bad in ["1", "yes", "true", "", "maybe"] {
            assert!(
                drv.set_option(&mut AsynUser::default(), "disconnectOnReadTimeout", bad)
                    .is_err(),
                "value {bad:?} should be rejected"
            );
        }
        // The last accepted value (N) is unchanged by the rejected sets.
        assert_eq!(
            drv.get_option(&AsynUser::default(), "disconnectOnReadTimeout")
                .unwrap(),
            "N"
        );
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
        assert_eq!(
            err.message(),
            format!("127.0.0.1:{port}: Link already open!")
        );
        // The original socket is left intact (still connected).
        assert!(drv.base().is_connected());
    }

    // --- hostInfo option tests ---

    #[test]
    fn test_set_option_host_info() {
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:5025").unwrap();
        drv.set_option(&mut AsynUser::default(), "hostInfo", "192.168.1.1:8080")
            .unwrap();
        assert_eq!(drv.config().host, "192.168.1.1");
        assert_eq!(drv.config().port, 8080);
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
        assert_eq!(drv.config().port, 9999);
    }

    /// C `connectIt` bounds a TCP connect by `pasynBase->autoConnectTimeout`
    /// (drvAsynIPPort.c:546-555), 0.5 s unless `asynSetAutoConnectTimeout`
    /// moved it, and on expiry reports the errno `connect()` left
    /// (:534-539, :568-571). The held port leaves the handshake unanswered
    /// on loopback, so the poll is what ends it, whatever routes the host
    /// has; the windows open 50 ms early because `poll` may wake a hair
    /// short.
    #[test]
    fn a_tcp_connect_expires_at_the_auto_connect_timeout_in_c_s_words() {
        use crate::runtime::config::set_auto_connect_timeout;

        let held = named_port::HeldPort::unanswered_tcp();
        let device = format!("127.0.0.1:{}", held.port);
        let attempt = || {
            let mut drv = DrvAsynIPPort::new("iptest", &device).unwrap();
            let start = std::time::Instant::now();
            let err = drv.connect(&AsynUser::default()).unwrap_err();
            (start.elapsed(), err)
        };
        let (took, err) = attempt();
        assert!(
            took >= Duration::from_millis(450) && took < Duration::from_millis(900),
            "C DEFAULT_AUTOCONNECT_TIMEOUT is 0.5 s, took {took:?}"
        );
        assert_eq!(err.status(), AsynStatus::Error);
        assert_eq!(
            err.message(),
            format!("Can't connect to {device}: {}", connect_in_progress_text())
        );

        set_auto_connect_timeout(1.0);
        let (took, err) = attempt();
        set_auto_connect_timeout(0.5);
        assert!(
            took >= Duration::from_millis(950) && took < Duration::from_millis(1400),
            "asynSetAutoConnectTimeout 1 bounds the connect at 1 s, took {took:?}"
        );
        assert_eq!(
            err.message(),
            format!("Can't connect to {device}: {}", connect_in_progress_text())
        );
    }

    /// `hostToIPAddr` answers a name with its first IPv4 address whatever
    /// comes before it, and a name with none keeps its first address.
    #[test]
    fn the_remote_is_the_first_ipv4_answer_else_the_first() {
        let a = |s: &str| s.parse::<std::net::SocketAddr>().unwrap();
        assert_eq!(
            one_remote(&[a("[::1]:5"), a("127.0.0.2:5"), a("127.0.0.1:5")]),
            Some(a("127.0.0.2:5"))
        );
        assert_eq!(
            one_remote(&[a("[::1]:5"), a("[::2]:5")]),
            Some(a("[::1]:5"))
        );
        assert_eq!(one_remote(&[]), None);
    }

    /// A dual-stack `localhost` reaches its 127.0.0.1 listener, over TCP and
    /// UDP, and never `::1`, which every CI cell answers first. Where the
    /// hosts file gives `localhost` 127.0.0.1 alone there is nothing else to
    /// reach; `the_remote_is_the_first_ipv4_answer_else_the_first` holds the
    /// rule there.
    #[test]
    fn a_dual_stack_name_reaches_its_ipv4_address() {
        use std::net::Ipv6Addr;

        let (tcp4, tcp6, udp4, udp6) = (0..named_port::ATTEMPTS)
            .find_map(|_| {
                let tcp4 = TcpListener::bind("127.0.0.1:0").ok()?;
                let port = tcp4.local_addr().ok()?.port();
                let tcp6 = TcpListener::bind((Ipv6Addr::LOCALHOST, port)).ok()?;
                let udp4 = UdpSocket::bind(("127.0.0.1", port)).ok()?;
                let udp6 = UdpSocket::bind((Ipv6Addr::LOCALHOST, port)).ok()?;
                Some((tcp4, tcp6, udp4, udp6))
            })
            .expect("one number free on both loopbacks for TCP and UDP");
        let port = tcp4.local_addr().unwrap().port();

        let mut drv = DrvAsynIPPort::new("iptest", &format!("localhost:{port}")).unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        tcp4.set_nonblocking(true).unwrap();
        tcp6.set_nonblocking(true).unwrap();
        assert!(tcp4.accept().is_ok(), "the connect reached 127.0.0.1");
        assert!(tcp6.accept().is_err(), "and not ::1");

        let mut drv = DrvAsynIPPort::new("iptest", &format!("localhost:{port} udp")).unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user, b"ping").unwrap();
        udp4.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 8];
        let n = udp4.recv(&mut buf).expect("the datagram reached 127.0.0.1");
        assert_eq!(&buf[..n], b"ping");
        udp6.set_nonblocking(true).unwrap();
        assert!(udp6.recv(&mut buf).is_err(), "and not ::1");
    }

    /// C `connectIt` makes one connect, to `hostToIPAddr`'s one address,
    /// under one `autoConnectTimeout` (drvAsynIPPort.c:484-494, :541-571),
    /// so a `localhost` whose `::1` and 127.0.0.1 both leave the handshake
    /// unanswered fails in 0.5 s, not 0.5 s for each.
    #[test]
    fn a_name_s_unanswered_addresses_wait_one_auto_connect_timeout() {
        let held = named_port::HeldPort::unanswered_tcp_dual_stack();
        let device = format!("localhost:{}", held.port);
        let mut drv = DrvAsynIPPort::new("iptest", &device).unwrap();
        let start = std::time::Instant::now();
        let err = drv.connect(&AsynUser::default()).unwrap_err();
        let took = start.elapsed();
        assert!(
            took >= Duration::from_millis(450) && took < Duration::from_millis(900),
            "one C DEFAULT_AUTOCONNECT_TIMEOUT of 0.5 s, took {took:?}"
        );
        assert_eq!(
            err.message(),
            format!("Can't connect to {device}: {}", connect_in_progress_text())
        );
    }

    /// A resolver the test answers for, which counts the lookups it is asked.
    #[derive(Clone, Default)]
    struct FakeDns {
        answer: std::sync::Arc<std::sync::Mutex<Vec<std::net::SocketAddr>>>,
        lookups: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FakeDns {
        /// A driver for `hostInfo` that looks its names up here, answering `at`.
        fn port(host_info: &str, at: std::net::SocketAddr) -> (DrvAsynIPPort, Self) {
            let dns = Self::default();
            dns.answer(at);
            let mut drv = DrvAsynIPPort::new("iptest", host_info).unwrap();
            let asked = dns.clone();
            drv.resolver = Box::new(move |_, _| {
                asked
                    .lookups
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                asked.answer.lock().unwrap().clone()
            });
            (drv, dns)
        }

        fn answer(&self, at: std::net::SocketAddr) {
            *self.answer.lock().unwrap() = vec![at];
        }

        fn forget(&self) {
            self.answer.lock().unwrap().clear();
        }

        fn lookups(&self) -> usize {
            self.lookups.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Read the EOF of a peer that closed, which closes the link (C readIt,
    /// drvAsynIPPort.c:815-821).
    fn read_the_peer_s_close(drv: &mut DrvAsynIPPort) {
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let (n, eom) = drv.io_read_octet(&user, &mut [0u8; 8]).unwrap();
        assert!(n == 0 && eom.contains(EomReason::END));
        assert!(!drv.base().is_connected());
    }

    /// C keeps `farAddr` once looked up (`FLAG_DONE_LOOKUP`,
    /// drvAsynIPPort.c:488-497): a reconnect after the peer closes looks
    /// nothing up.
    #[test]
    fn a_reconnect_after_the_peer_closes_looks_nothing_up() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = listener.local_addr().unwrap();
        let (mut drv, dns) = FakeDns::port(&format!("dev.test:{}", at.port()), at);
        drv.connect(&AsynUser::default()).unwrap();
        drop(listener.accept().unwrap());
        read_the_peer_s_close(&mut drv);
        drv.connect(&AsynUser::default()).unwrap();
        assert!(listener.accept().is_ok());
        assert_eq!(dns.lookups(), 1);
    }

    /// The kept address is the device's whatever DNS says later, so a name
    /// gone from it still reconnects.
    #[test]
    fn a_name_gone_from_dns_still_reconnects_to_the_kept_address() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = listener.local_addr().unwrap();
        let (mut drv, dns) = FakeDns::port(&format!("dev.test:{}", at.port()), at);
        drv.connect(&AsynUser::default()).unwrap();
        drop(listener.accept().unwrap());
        read_the_peer_s_close(&mut drv);
        dns.forget();
        drv.connect(&AsynUser::default()).unwrap();
        assert!(listener.accept().is_ok());
    }

    /// A failed TCP connect owes the next connect a lookup, "in case the
    /// device has DHCP'd itself an new number" (drvAsynIPPort.c:530-532,
    /// :572-574), and the new number is the one it reaches.
    #[test]
    fn a_failed_connect_owes_the_next_connect_a_lookup() {
        let refused = named_port::HeldPort::closed_tcp();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let moved = listener.local_addr().unwrap();
        let (mut drv, dns) = FakeDns::port(
            "dev.test:1",
            std::net::SocketAddr::from(([127, 0, 0, 1], refused.port)),
        );
        drv.connect(&AsynUser::default()).unwrap_err();
        dns.answer(moved);
        drv.connect(&AsynUser::default()).unwrap();
        assert!(listener.accept().is_ok());
        assert_eq!(dns.lookups(), 2);
    }

    /// A numeric host is its own address: no lookup, not even after a
    /// failed connect.
    #[test]
    fn a_numeric_host_never_looks_up() {
        let refused = named_port::HeldPort::closed_tcp();
        let at = std::net::SocketAddr::from(([127, 0, 0, 1], refused.port));
        let (mut drv, dns) = FakeDns::port(&at.to_string(), at);
        drv.connect(&AsynUser::default()).unwrap_err();
        drv.connect(&AsynUser::default()).unwrap_err();
        assert_eq!(dns.lookups(), 0);
    }

    /// A UDP port never fails a connect, so C looks its name up once per
    /// hostInfo: a reconnect looks nothing up, a new hostInfo does.
    #[test]
    fn a_udp_port_looks_up_once_per_host_info() {
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let at = peer.local_addr().unwrap();
        let host_info = format!("dev.test:{} udp", at.port());
        let (mut drv, dns) = FakeDns::port(&host_info, at);
        let mut user = AsynUser::default();
        drv.connect(&user).unwrap();
        drv.disconnect(&user).unwrap();
        drv.connect(&user).unwrap();
        assert_eq!(dns.lookups(), 1);
        drv.set_option(&mut user, "hostInfo", &host_info).unwrap();
        drv.connect(&user).unwrap();
        assert_eq!(dns.lookups(), 2);
    }

    /// Each HTTP transaction opens its own socket to the kept address
    /// (drvAsynIPPort.c:776-780), looking nothing up after the first.
    #[test]
    fn http_transactions_look_the_name_up_once() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut s, _) = listener.accept().unwrap();
                s.write_all(b"HELLO").unwrap();
            }
        });
        let (mut drv, dns) = FakeDns::port(&format!("dev.test:{} HTTP", at.port()), at);
        drv.connect(&AsynUser::default()).unwrap();
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        for _ in 0..2 {
            let mut buf = [0u8; 5];
            let (n, _) = drv.io_read_octet(&user, &mut buf).unwrap();
            assert_eq!(&buf[..n], b"HELLO");
            let (n, eom) = drv.io_read_octet(&user, &mut buf).unwrap();
            assert!(n == 0 && eom.contains(EomReason::END));
        }
        server.join().unwrap();
        assert_eq!(dns.lookups(), 1);
    }

    /// A refusal the poll finds still reports `connect()`'s own errno, as C's
    /// saved `sockerrmsg` does (drvAsynIPPort.c:534-539, :568-571). Linux only:
    /// its loopback connect defers the refusal to `SO_ERROR`, other stacks may
    /// refuse in `connect()` itself.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_refused_tcp_connect_reports_connect_s_errno_in_c_s_words() {
        let held = named_port::HeldPort::closed_tcp();
        let port = held.port;
        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{port}")).unwrap();
        let err = drv.connect(&AsynUser::default()).unwrap_err();
        assert_eq!(err.status(), AsynStatus::Error);
        assert_eq!(
            err.message(),
            format!(
                "Can't connect to 127.0.0.1:{port}: {}",
                connect_in_progress_text()
            )
        );
    }

    /// A name that does not resolve is C's `Unknown host "<host>"`, for TCP and
    /// UDP alike (drvAsynIPPort.c:488-494). `.invalid` never resolves (RFC 6761).
    #[test]
    fn an_unknown_host_is_reported_in_c_s_words() {
        for info in [
            "no-such-device.invalid:5025",
            "no-such-device.invalid:5025 udp",
        ] {
            let mut drv = DrvAsynIPPort::new("iptest", info).unwrap();
            let err = drv.connect(&AsynUser::default()).unwrap_err();
            assert_eq!(err.status(), AsynStatus::Error, "{info}");
            assert_eq!(
                err.message(),
                "Unknown host \"no-such-device.invalid\"",
                "{info}"
            );
        }
    }

    fn setup_failure(step: socket::SetupStep) -> String {
        let e = socket::SetupError {
            step,
            error: std::io::Error::other("cause"),
        };
        setup_failure_text("10.0.0.1:5025 udp", &e)
    }

    /// drvAsynIPPort.c:454-459.
    #[test]
    fn a_broadcast_option_that_fails_is_reported_in_c_s_words() {
        assert_eq!(
            setup_failure(socket::SetupStep::Broadcast),
            "Can't set 10.0.0.1:5025 udp socket BROADCAST option: cause"
        );
    }

    /// drvAsynIPPort.c:470-478 — C keeps this text when `USE_SO_REUSEADDR`
    /// swaps the option underneath it.
    #[test]
    fn a_reuse_port_option_that_fails_is_reported_in_c_s_words() {
        assert_eq!(
            setup_failure(socket::SetupStep::ReusePort),
            "Can't set 10.0.0.1:5025 udp socket SO_REUSEPORT option: cause"
        );
        assert_eq!(
            setup_failure(socket::SetupStep::ReuseAddress),
            "Can't set 10.0.0.1:5025 udp socket SO_REUSEADDR option: cause"
        );
    }

    /// drvAsynIPPort.c:516-523.
    #[test]
    fn a_blocking_mode_that_cannot_be_set_is_reported_in_c_s_words() {
        assert_eq!(
            setup_failure(socket::SetupStep::NonBlocking),
            "Can't set 10.0.0.1:5025 udp O_NONBLOCK option: cause"
        );
    }

    /// drvAsynIPPort.c:580-589.
    #[test]
    fn a_nodelay_option_that_fails_is_reported_in_c_s_words() {
        assert_eq!(
            nodelay_failure_text("10.0.0.1:5025", &std::io::Error::other("cause")),
            "Can't set 10.0.0.1:5025 socket NODELAY option: cause"
        );
    }

    /// drvAsynIPPort.c:443-446: no device name, the errno text only.
    #[test]
    fn a_socket_that_cannot_be_created_is_reported_in_c_s_words() {
        assert_eq!(
            setup_failure(socket::SetupStep::Create),
            "Can't create socket: cause"
        );
    }

    /// drvAsynIPPort.c:500-505: no device name, the errno text only.
    #[test]
    fn a_local_bind_that_fails_is_reported_in_c_s_words() {
        assert_eq!(
            setup_failure(socket::SetupStep::Bind),
            "unable to bind to local port: cause"
        );
    }

    /// The bind C provokes with a local port another socket listens on: the
    /// setup text, not "Can't connect to" (drvAsynIPPort.c:500-505).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_local_port_in_use_fails_the_bind_in_c_s_words() {
        let held = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let local = held.local_addr().unwrap().port();
        let mut drv = DrvAsynIPPort::new("iptest", &format!("127.0.0.1:9:{local}")).unwrap();
        let err = drv.connect(&AsynUser::default()).unwrap_err();
        assert_eq!(
            err.message(),
            "unable to bind to local port: Address already in use"
        );
    }

    /// C binds a fixed local port with no SO_REUSEADDR (drvAsynIPPort.c:499-509),
    /// so the client socket carries none. No hold can share that bind, so a
    /// local port taken before the driver binds it is drawn again.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_local_port_bind_sets_no_reuse_address() {
        use std::os::fd::AsRawFd;
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let remote = server.local_addr().unwrap().port();
        let (stream, local) = named_port::on_a_named_port(|local| {
            let mut drv =
                DrvAsynIPPort::new("iptest", &format!("127.0.0.1:{remote}:{local}")).unwrap();
            match drv.connect_tcp() {
                Ok(stream) => Some((stream, local)),
                Err(e) if e.message() == "unable to bind to local port: Address already in use" => {
                    None
                }
                Err(e) => panic!("connect from {local}: {e:?}"),
            }
        });
        assert_eq!(stream.local_addr().unwrap().port(), local);
        let mut on: libc::c_int = -1;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `on`/`len` describe a c_int the call writes into.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                (&mut on as *mut libc::c_int).cast(),
                &mut len,
            )
        };
        assert_eq!(rc, 0);
        assert_eq!(on, 0, "SO_REUSEADDR on the client socket");
    }

    /// An AF_UNIX connect failure reads as C's, naming the whole hostInfo
    /// (drvAsynIPPort.c:568-571).
    #[cfg(asyn_af_unix)]
    #[test]
    fn a_unix_connect_failure_is_reported_in_c_s_words() {
        let mut drv = DrvAsynIPPort::new("iptest", "unix:///nonexistent/asyn.sock").unwrap();
        let err = drv.connect(&AsynUser::default()).unwrap_err();
        assert_eq!(
            err.message(),
            "Can't connect to unix:///nonexistent/asyn.sock: No such file or directory"
        );
    }

    /// hostInfo runtime reparse must update the protocol field too,
    /// matching C parseHostInfo (drvAsynIPPort.c:356-391). Previously
    /// only host/port/local_port were copied, so switching from TCP
    /// to UDP at runtime left the socket type unchanged.
    #[test]
    fn host_info_reparse_updates_protocol_and_flags() {
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:5025 tcp").unwrap();
        assert_eq!(drv.config().protocol, IpProtocol::Tcp);

        drv.set_option(&mut AsynUser::default(), "hostInfo", "127.0.0.1:5026 udp")
            .unwrap();
        assert_eq!(drv.config().protocol, IpProtocol::Udp);
        assert_eq!(drv.config().port, 5026);

        drv.set_option(&mut AsynUser::default(), "hostInfo", "127.0.0.1:5027 udp*")
            .unwrap();
        assert_eq!(drv.config().protocol, IpProtocol::UdpBroadcast);

        drv.set_option(&mut AsynUser::default(), "hostInfo", "127.0.0.1:5028 udp&")
            .unwrap();
        assert_eq!(drv.config().protocol, IpProtocol::UdpReusePort);

        drv.set_option(&mut AsynUser::default(), "hostInfo", "127.0.0.1:5029 udp*&")
            .unwrap();
        assert_eq!(drv.config().protocol, IpProtocol::UdpBroadcastReusePort);

        drv.set_option(&mut AsynUser::default(), "hostInfo", "127.0.0.1:5030 tcp&")
            .unwrap();
        assert_eq!(drv.config().protocol, IpProtocol::TcpReusePort);
    }

    /// hostInfo reparse must clear `local_port` when the new spec
    /// omits the second-colon field. C parseHostInfo only sets
    /// tty->localAddr when it parses a value (line 339-348); the
    /// previous Rust impl preserved the old local_port on omission,
    /// which silently bound the new socket to the prior outgoing port.
    #[test]
    fn host_info_reparse_clears_omitted_local_port() {
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:5025:12345 tcp").unwrap();
        assert_eq!(drv.config().local_port, Some(12345));
        drv.set_option(&mut AsynUser::default(), "hostInfo", "127.0.0.1:5026 tcp")
            .unwrap();
        assert_eq!(
            drv.config().local_port,
            None,
            "local_port must reset on hostInfo reparse"
        );
    }

    /// Regression.
    ///
    /// C `drvAsynIPPort` compares option keys with `epicsStrCaseCmp`
    /// (drvAsynIPPort.c:953/991), so the asynRecord's lowercase
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
        assert_eq!(drv.config().host, "10.0.0.5");
        assert_eq!(drv.config().port, 1234);
        assert_eq!(drv.config().protocol, IpProtocol::Udp);

        // get must echo the live endpoint for either key case
        // (C getOption returns tty->IPDeviceName verbatim).
        assert_eq!(
            drv.get_option(&AsynUser::default(), "hostinfo").unwrap(),
            "10.0.0.5:1234 udp"
        );
        assert_eq!(
            drv.get_option(&AsynUser::default(), "hostInfo").unwrap(),
            "10.0.0.5:1234 udp"
        );

        // disconnectOnReadTimeout shares the same case-insensitive key
        // contract for set and get (C getOption -> "Y"/"N").
        drv.set_option(&mut AsynUser::default(), "DISCONNECTONREADTIMEOUT", "Y")
            .unwrap();
        assert_eq!(
            drv.get_option(&AsynUser::default(), "disconnectonreadtimeout")
                .unwrap(),
            "Y"
        );
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
        // R11-49: reported in C's words (drvAsynIPPort.c:996-997).
        assert_eq!(err.message(), "Unsupported key \"bogusKey\"");
        assert!(drv.get_option(&AsynUser::default(), "bogusKey").is_err());

        // Empty key is a silent no-op (C `epicsStrCaseCmp(key,"") != 0`).
        drv.set_option(&mut AsynUser::default(), "", "ignored")
            .unwrap();
    }

    /// R11-49: the IP driver's own invalid-value texts are C's
    /// (drvAsynIPPort.c:986-987), and the Rust-only `noDelay` key follows the
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

    /// R8-57: `isCom` is what gates the install (C drvAsynIPPort.c:1115), so a
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

    /// R8-57 ordering: C installs COM at :1115 and the EOS interpose at :1119,
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
        // goes on top (noProcessEos defaults off, C :1119-1120).
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
    /// (:724-731). Reading `baud` off a COM port must not reach the IP driver's
    /// option map, and reading `hostInfo` must still reach it.
    #[test]
    fn com_option_interface_is_layered_over_the_ip_drivers() {
        let com = DrvAsynIPPort::new("comport", "1.2.3.4:5000 COM").unwrap();
        // Answered by the interpose, from the defaults C installs (:851-855).
        assert_eq!(
            com.get_option(&AsynUser::default(), "baud").unwrap(),
            "9600"
        );
        assert_eq!(
            com.get_option(&AsynUser::default(), "parity").unwrap(),
            "none"
        );
        assert_eq!(
            com.get_option(&AsynUser::default(), "crtscts").unwrap(),
            "N"
        );
        // Not the interpose's key: falls through to the IP driver.
        assert_eq!(
            com.get_option(&AsynUser::default(), "hostInfo").unwrap(),
            "1.2.3.4:5000 COM"
        );
        assert_eq!(
            com.get_option(&AsynUser::default(), "disconnectOnReadTimeout")
                .unwrap(),
            "N"
        );

        // A plain TCP port has no COM layer, so the serial keys are unknown to it
        // — the same asynError C's IP driver returns for an unsupported key.
        let tcp = DrvAsynIPPort::new("tcpport", "1.2.3.4:5000 TCP").unwrap();
        assert!(tcp.get_option(&AsynUser::default(), "baud").is_err());
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
        let n = drv.io_read_octet(&user, &mut buf).into_count().unwrap();
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
        let n = drv.io_read_octet(&ruser, &mut buf).into_count().unwrap();
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

    /// C `readIt`/`writeIt` answer a missing socket themselves, with asynError
    /// and their own text — the read's with a colon, the write's without.
    #[test]
    fn a_transfer_with_no_socket_is_the_drivers_error() {
        let mut drv = DrvAsynIPPort::new("iptest", "127.0.0.1:9999").unwrap();
        let mut user = AsynUser::new(0);
        let mut buf = [0u8; 8];
        for (err, text) in [
            (
                drv.io_read_octet(&user, &mut buf).unwrap_err(),
                "127.0.0.1:9999 disconnected:",
            ),
            (
                drv.write_octet(&mut user, b"x").unwrap_err(),
                "127.0.0.1:9999 disconnected",
            ),
        ] {
            match err {
                AsynError::Status { status, message } => {
                    assert_eq!(status, AsynStatus::Error);
                    assert_eq!(message, text);
                }
                other => panic!("expected the driver's error, got {other:?}"),
            }
        }
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
        assert_eq!(drv.config().protocol, IpProtocol::UdpBroadcast);
    }

    /// Regression: `connect` publishes the socket only after its last
    /// fallible step, so a failure there cannot leave one installed.
    ///
    /// The failure is real rather than simulated. An `O_PATH` descriptor is a
    /// valid, owned, closeable descriptor on which every `ioctl(2)` — and so
    /// `set_nonblocking`, which is `ioctl(FIONBIO)` — returns `EBADF`
    /// (open(2)). It stands in for the targets where owning the blocking mode
    /// genuinely can fail; on Linux a dialled socket never refuses `FIONBIO`,
    /// so there is nothing to race against and no seam to add.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_transport_that_fails_to_publish_leaves_the_port_retryable() {
        use std::os::fd::FromRawFd;

        let mut drv = DrvAsynIPPort::new("publish_last", "127.0.0.1:1").unwrap();

        let root = std::ffi::CString::new("/").unwrap();
        let fd = unsafe { libc::open(root.as_ptr(), libc::O_PATH) };
        assert!(fd >= 0, "could not open an O_PATH descriptor");
        // SAFETY: `fd` was just opened and is owned by nothing else, so the
        // `TcpStream` is its sole owner and closes it on drop. It is not a
        // socket, which is the point — the socket calls on it fail.
        let unusable = unsafe { TcpStream::from_raw_fd(fd) };

        let err = drv
            .publish_transport(&AsynUser::default(), IpIoInner::Tcp(unusable))
            .expect_err("set_nonblocking on an O_PATH descriptor must fail");
        // C's setNonBlock text (drvAsynIPPort.c:516-523) around the EBADF.
        assert_eq!(
            err.message(),
            "Can't set 127.0.0.1:1 O_NONBLOCK option: Bad file descriptor"
        );
        assert!(
            drv.io.inner.is_none(),
            "a publish that failed must install nothing"
        );
        assert!(!drv.base.is_connected());

        // The wedge C avoids by leaving `tty->fd == INVALID_SOCKET`
        // (drvAsynIPPort.c:516-523, :593): with a socket left behind, every
        // later connect answers the already-open guard instead of retrying,
        // and nothing clears it because `drop_connection` is gated on
        // `is_connected()`.
        let retry = drv
            .connect(&AsynUser::default())
            .expect_err("nothing listens on 127.0.0.1:1");
        assert!(
            !retry.to_string().contains("Link already open"),
            "a failed publish must leave the port retryable, got {retry}"
        );
    }
}
