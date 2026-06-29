//! Serial port driver (drvAsynSerialPort equivalent).
//!
//! Uses `libc` termios directly for serial I/O. Unix-only (`#[cfg(unix)]`).

use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::exception::AsynException;
use crate::interpose::{EomReason, OctetNext, OctetReadResult};
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::trace::TraceMask;
use crate::user::AsynUser;
use crate::{asyn_trace, asyn_trace_io};

// --- Configuration types ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataBits {
    Five,
    Six,
    Seven,
    Eight,
}

impl Default for DataBits {
    fn default() -> Self {
        DataBits::Eight
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parity {
    None,
    Odd,
    Even,
}

impl Default for Parity {
    fn default() -> Self {
        Parity::None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopBits {
    One,
    Two,
}

impl Default for StopBits {
    fn default() -> Self {
        StopBits::One
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowControl {
    None,
    Hardware,
    Software,
}

impl Default for FlowControl {
    fn default() -> Self {
        FlowControl::None
    }
}

#[derive(Debug, Clone)]
pub struct SerialConfig {
    pub device: String,
    pub baud: u32,
    pub data_bits: DataBits,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow_control: FlowControl,
}

impl SerialConfig {
    /// Parse a serial port specification string.
    ///
    /// Format: `"/dev/ttyUSB0"` — just the device path.
    /// Baud and other settings default to 9600 8N1 no flow control.
    pub fn parse(spec: &str) -> AsynResult<Self> {
        let device = spec.trim().to_string();
        if device.is_empty() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "empty serial device path".into(),
            });
        }
        Ok(Self {
            device,
            baud: 9600,
            data_bits: DataBits::default(),
            parity: Parity::default(),
            stop_bits: StopBits::default(),
            flow_control: FlowControl::default(),
        })
    }

    /// Apply this configuration to a raw termios struct.
    pub fn apply_to_termios(&self, t: &mut libc::termios) {
        let baud = baud_to_speed(self.baud);
        unsafe {
            libc::cfsetispeed(t, baud);
            libc::cfsetospeed(t, baud);
        }

        // Data bits
        t.c_cflag &= !libc::CSIZE;
        t.c_cflag |= match self.data_bits {
            DataBits::Five => libc::CS5,
            DataBits::Six => libc::CS6,
            DataBits::Seven => libc::CS7,
            DataBits::Eight => libc::CS8,
        };

        // Parity
        match self.parity {
            Parity::None => {
                t.c_cflag &= !libc::PARENB;
            }
            Parity::Even => {
                t.c_cflag |= libc::PARENB;
                t.c_cflag &= !libc::PARODD;
            }
            Parity::Odd => {
                t.c_cflag |= libc::PARENB;
                t.c_cflag |= libc::PARODD;
            }
        }

        // Stop bits
        match self.stop_bits {
            StopBits::One => t.c_cflag &= !libc::CSTOPB,
            StopBits::Two => t.c_cflag |= libc::CSTOPB,
        }

        // Flow control
        match self.flow_control {
            FlowControl::None => {
                t.c_cflag &= !libc::CRTSCTS;
                t.c_iflag &= !(libc::IXON | libc::IXOFF | libc::IXANY);
            }
            FlowControl::Hardware => {
                t.c_cflag |= libc::CRTSCTS;
                t.c_iflag &= !(libc::IXON | libc::IXOFF | libc::IXANY);
            }
            FlowControl::Software => {
                t.c_cflag &= !libc::CRTSCTS;
                t.c_iflag |= libc::IXON | libc::IXOFF;
            }
        }
    }
}

fn baud_to_speed(baud: u32) -> libc::speed_t {
    match baud {
        0 => libc::B0,
        50 => libc::B50,
        75 => libc::B75,
        110 => libc::B110,
        134 => libc::B134,
        150 => libc::B150,
        200 => libc::B200,
        300 => libc::B300,
        600 => libc::B600,
        1200 => libc::B1200,
        1800 => libc::B1800,
        2400 => libc::B2400,
        4800 => libc::B4800,
        9600 => libc::B9600,
        19200 => libc::B19200,
        38400 => libc::B38400,
        57600 => libc::B57600,
        115200 => libc::B115200,
        230400 => libc::B230400,
        // High baud rates: available on Linux/FreeBSD/NetBSD, not macOS.
        // C parity: conditional on #ifdef B460800 etc.
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        460800 => libc::B460800,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        500000 => libc::B500000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        576000 => libc::B576000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        921600 => libc::B921600,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        1000000 => libc::B1000000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        1152000 => libc::B1152000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        1500000 => libc::B1500000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        2000000 => libc::B2000000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        2500000 => libc::B2500000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        3000000 => libc::B3000000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        3500000 => libc::B3500000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        4000000 => libc::B4000000,
        _ => libc::B9600, // fallback
    }
}

#[allow(dead_code)]
fn speed_to_baud(speed: libc::speed_t) -> u32 {
    match speed {
        libc::B0 => 0,
        libc::B50 => 50,
        libc::B75 => 75,
        libc::B110 => 110,
        libc::B134 => 134,
        libc::B150 => 150,
        libc::B200 => 200,
        libc::B300 => 300,
        libc::B600 => 600,
        libc::B1200 => 1200,
        libc::B1800 => 1800,
        libc::B2400 => 2400,
        libc::B4800 => 4800,
        libc::B9600 => 9600,
        libc::B19200 => 19200,
        libc::B38400 => 38400,
        libc::B57600 => 57600,
        libc::B115200 => 115200,
        libc::B230400 => 230400,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        libc::B460800 => 460800,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        libc::B500000 => 500000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        libc::B576000 => 576000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        libc::B921600 => 921600,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        libc::B1000000 => 1000000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        libc::B1152000 => 1152000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        libc::B1500000 => 1500000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        libc::B2000000 => 2000000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        libc::B2500000 => 2500000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        libc::B3000000 => 3000000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        libc::B3500000 => 3500000,
        #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"))]
        libc::B4000000 => 4000000,
        _ => 0,
    }
}

/// Supported baud rates. `baud_to_speed` returns the matching `libc::speed_t`,
/// or falls back to B9600 for unsupported values. Use `is_supported_baud()` to
/// check before setting.
const SUPPORTED_BAUDS: &[u32] = &[
    0, 50, 75, 110, 134, 150, 200, 300, 600, 1200, 1800, 2400, 4800, 9600, 19200, 38400, 57600,
    115200, 230400,
];

fn is_supported_baud(baud: u32) -> bool {
    SUPPORTED_BAUDS.contains(&baud)
}

/// Parse a boolean option value.
///
/// Accepted truthy values (case-insensitive): `y`, `yes`, `1`, `true`.
/// Accepted falsy values (case-insensitive): `n`, `no`, `0`, `false`.
/// Returns `Err` for unrecognized values.
fn parse_bool_option(value: &str) -> AsynResult<bool> {
    // C drvAsynSerialPort.c::setOption validates the boolean serial options
    // (clocal/crtscts/ixon/ixoff/ixany, lines 410-504) strictly with
    // epicsStrCaseCmp(val,"Y")/("N"): only "Y"/"N" (case-insensitive) are
    // accepted; anything else returns asynError "Invalid <key> value."
    // Match that strict accept-set instead of the looser y/yes/1/true
    // coercion, so a typo errors rather than silently selecting the wrong
    // setting (the same reason disconnectOnReadTimeout/noDelay are strict).
    if value.eq_ignore_ascii_case("Y") {
        Ok(true)
    } else if value.eq_ignore_ascii_case("N") {
        Ok(false)
    } else {
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: format!("invalid boolean value: '{value}' (expected Y or N)"),
        })
    }
}

// --- I/O state ---

struct SerialIoState {
    fd: Option<RawFd>,
    /// Cumulative bytes successfully read / written, for `report()` diagnostics
    /// (C tracks `tty->nRead` / `tty->nWritten`, drvAsynSerialPort.c).
    n_read: u64,
    n_written: u64,
}

impl SerialIoState {
    fn new() -> Self {
        Self {
            fd: None,
            n_read: 0,
            n_written: 0,
        }
    }

    fn fd_or_err(&self) -> AsynResult<RawFd> {
        self.fd.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Disconnected,
            message: "serial port not open".into(),
        })
    }
}

fn duration_to_poll_ms(d: Duration) -> i32 {
    d.as_millis().min(i32::MAX as u128) as i32
}

impl OctetNext for SerialIoState {
    fn read(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
        let fd = self.fd_or_err()?;
        // C readIt (drvAsynSerialPort.c:871-875): reject maxchars == 0 with
        // asynError right after the fd check and before touching the device.
        // An empty buffer would otherwise reach libc::read(fd, ptr, 0), which
        // returns 0 and is misclassified below (n == 0) as a disconnect (EOF)
        // — tearing down a live serial port. (Message matches the serial
        // driver's own wording, which omits the period the IP driver carries.)
        if buf.is_empty() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "maxchars 0 Why <=0?".into(),
            });
        }
        let timeout_ms = duration_to_poll_ms(user.timeout);

        // C parity (drvAsynSerialPort.c): retry poll/read on EINTR (a signal
        // interrupted the call) and EAGAIN/EWOULDBLOCK (spurious wakeup);
        // only a real error is fatal. Without this, a benign signal would be
        // surfaced as a fatal Io error and tear the connection down.
        loop {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };

            let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(AsynError::Io(err));
            }
            if ret == 0 {
                return Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "serial read timeout".into(),
                });
            }

            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted
                    || err.kind() == std::io::ErrorKind::WouldBlock
                {
                    continue;
                }
                return Err(AsynError::Io(err));
            }
            if n == 0 {
                return Err(AsynError::Status {
                    status: AsynStatus::Disconnected,
                    message: "serial port EOF".into(),
                });
            }

            self.n_read += n as u64; // C parity: tty->nRead += thisRead
            return Ok(OctetReadResult {
                nbytes_transferred: n as usize,
                // C parity: CNT only when the requested count was reached.
                eom_reason: if n as usize >= buf.len() {
                    EomReason::CNT
                } else {
                    EomReason::empty()
                },
            });
        }
    }

    fn write(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let fd = self.fd_or_err()?;

        // The driver fd is blocking (connect restores O_NONBLOCK off so reads
        // can block on poll). A blocking `write(fd, all_remaining)` would not
        // return until the *entire* buffer is accepted by the kernel, so a
        // stalled or slow peer blocks the write past the timeout regardless of
        // the poll below — C instead unblocks a stuck write from its timeout
        // timer via tcflush(TCOFLUSH) (drvAsynSerialPort.c:649). This driver has
        // no such timer, so make the fd non-blocking for the duration of the
        // write: each `write` then returns immediately with what fit (or EAGAIN)
        // and the poll/deadline loop bounds the whole write. Blocking mode is
        // restored on every exit path.
        let prev_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if prev_flags < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFL, prev_flags | libc::O_NONBLOCK) } < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }

        // C parity (drvAsynSerialPort.c:815-842): writeIt arms a single timer
        // for the whole writeTimeout *before* the loop and breaks when it fires,
        // so the timeout bounds the TOTAL write, not each chunk. Bound total
        // time with one deadline and poll with the remaining budget each
        // iteration (the IP driver's write_with_retry total-deadline model).
        // The previous code reused the full per-call timeout on every poll, so
        // a slowly-draining peer could keep a multi-chunk write alive for up to
        // timeout x iterations.
        let deadline = Instant::now() + user.timeout;
        let result = (|| -> AsynResult<usize> {
            let mut total = 0usize;
            while total < data.len() {
                let poll_ms =
                    duration_to_poll_ms(deadline.saturating_duration_since(Instant::now()));
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };

                let ret = unsafe { libc::poll(&mut pfd, 1, poll_ms) };
                if ret < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(AsynError::Io(err));
                }
                if ret == 0 {
                    return Err(AsynError::Status {
                        status: AsynStatus::Timeout,
                        message: "serial write timeout".into(),
                    });
                }

                let n = unsafe {
                    libc::write(
                        fd,
                        data[total..].as_ptr() as *const libc::c_void,
                        data.len() - total,
                    )
                };
                if n < 0 {
                    // C parity: retry on EINTR/EAGAIN; only a real error is fatal.
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted
                        || err.kind() == std::io::ErrorKind::WouldBlock
                    {
                        continue;
                    }
                    return Err(AsynError::Io(err));
                }
                total += n as usize;
                self.n_written += n as u64; // C parity: tty->nWritten += thisWrite

                // C parity (drvAsynSerialPort.c:827): after each write, if the
                // total deadline has passed stop with asynTimeout even though
                // some bytes went out. A non-blocking poll that finds free space
                // (e.g. a slow peer that drains a little each gap) would
                // otherwise let the write keep going past the deadline, since
                // `poll(0)` returns POLLOUT instead of timing out. (timeout==0
                // collapses to a single write attempt then bail, matching
                // writeIt's `writeTimeout==0`.)
                if total < data.len() && Instant::now() >= deadline {
                    return Err(AsynError::Status {
                        status: AsynStatus::Timeout,
                        message: "serial write timeout".into(),
                    });
                }
            }
            Ok(total)
        })();

        // Restore blocking mode on every exit path (success, timeout, error).
        unsafe { libc::fcntl(fd, libc::F_SETFL, prev_flags) };
        result
    }

    fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
        if let Some(fd) = self.fd {
            // C parity: tcflush(TCIFLUSH) discards received-but-unread input data,
            // matching C drvAsynSerialPort's flush behavior. NOT tcdrain (output wait).
            let ret = unsafe { libc::tcflush(fd, libc::TCIFLUSH) };
            if ret < 0 {
                return Err(AsynError::Io(std::io::Error::last_os_error()));
            }
        }
        Ok(())
    }
}

// --- Driver ---

/// Serial port driver.
pub struct DrvAsynSerialPort {
    base: PortDriverBase,
    config: SerialConfig,
    io: SerialIoState,
    saved_termios: Option<libc::termios>,
}

/// A transport error meaning the serial line is broken and the connection
/// must be torn down (vs a timeout, which leaves it open). C parity:
/// `drvAsynSerialPort.c` calls `closeConnection` on a real read/write error
/// or EOF but returns `asynTimeout` with the fd intact on a poll timeout.
/// Mirrors the same predicate in `ip_port.rs` (both transports share the
/// `closeConnection`-on-fatal-error contract).
fn is_fatal_transport_error(e: &AsynError) -> bool {
    matches!(
        e,
        AsynError::Status {
            status: AsynStatus::Disconnected,
            ..
        } | AsynError::Io(_)
    )
}

impl DrvAsynSerialPort {
    /// Close the fd and mark the port disconnected so the actor's
    /// auto-reconnect re-opens it on the next request. C parity:
    /// `drvAsynSerialPort.c::closeConnection` (close, fd=-1,
    /// `exceptionDisconnect`). Unlike the graceful `disconnect`, a
    /// fatal-error teardown does not restore termios — the device is gone
    /// and the fd is being closed.
    fn drop_connection(&mut self) {
        if let Some(fd) = self.io.fd.take() {
            unsafe { libc::close(fd) };
        }
        self.saved_termios = None;
        self.base.set_connected(false);
    }

    /// Create a new serial port driver.
    ///
    /// The driver starts disconnected with `auto_connect = true` and `can_block = true`.
    pub fn new(port_name: &str, config_str: &str) -> AsynResult<Self> {
        let config = SerialConfig::parse(config_str)?;
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
            io: SerialIoState::new(),
            saved_termios: None,
        })
    }

    /// Configure a serial port the way C `drvAsynSerialPortConfigure`
    /// does (`drvAsynSerialPort.c:1031-1126`): parse the device, honor
    /// `noAutoConnect`, and enable EOS processing by default unless
    /// `noProcessEos`. C does this by passing `(noProcessEos ? 0 : 1)` to
    /// `pasynOctetBase->initialize`; the Rust octet stack expresses EOS
    /// through the interpose layer, so the equivalent is to auto-install
    /// an `EosInterpose` (empty terminator until `setInputEos`/`OEOS`).
    ///
    /// `new` stays the parse-only constructor (no EOS), matching the
    /// lower-level C octet init without the `Configure` wrapper.
    pub fn configure(
        port_name: &str,
        config_str: &str,
        no_auto_connect: bool,
        no_process_eos: bool,
    ) -> AsynResult<Self> {
        let mut driver = Self::new(port_name, config_str)?;
        if no_auto_connect {
            driver.base.auto_connect = false;
        }
        if !no_process_eos {
            driver.push_interpose(Box::new(crate::interpose::eos::EosInterpose::default()));
        }
        Ok(driver)
    }

    /// Push an interpose layer onto the octet I/O stack.
    pub fn push_interpose(&mut self, layer: Box<dyn crate::interpose::OctetInterpose>) {
        self.base.push_octet_interpose(layer);
    }

    /// Send a serial line BREAK condition (RS-232 BREAK), mirroring
    /// asyn PR #188 ("auto serial break"). Duration is in tenths of
    /// a second per POSIX `tcsendbreak(fd, duration)` (Linux honors
    /// the value, BSD/macOS treats non-zero as ≥0.25s — match the
    /// platform semantic). `duration = 0` requests the minimum
    /// implementation-defined BREAK length (typically 250-500ms).
    ///
    /// Returns an error if the port is not currently connected.
    /// Operators driving break-reset protocols (e.g. some Tektronix
    /// scopes, certain Allen-Bradley PLCs) call this between
    /// commands to force the device's serial state machine to its
    /// initial state.
    pub fn send_break(&self, duration_tenths: i32) -> AsynResult<()> {
        let fd = self.io.fd_or_err()?;
        let ret = unsafe { libc::tcsendbreak(fd, duration_tenths) };
        if ret < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Drain any output queued on the serial port, blocking until
    /// every byte the kernel has accepted has actually been
    /// transmitted (POSIX `tcdrain`). Useful immediately before
    /// [`Self::send_break`] so the BREAK signal isn't preceded by
    /// unflushed user data.
    pub fn drain_output(&self) -> AsynResult<()> {
        let fd = self.io.fd_or_err()?;
        let ret = unsafe { libc::tcdrain(fd) };
        if ret < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn get_current_termios(&self) -> AsynResult<libc::termios> {
        let fd = self.io.fd_or_err()?;
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::tcgetattr(fd, &mut t) };
        if ret < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }
        Ok(t)
    }

    fn apply_termios(&self, t: &libc::termios) -> AsynResult<()> {
        let fd = self.io.fd_or_err()?;
        let ret = unsafe { libc::tcsetattr(fd, libc::TCSANOW, t) };
        if ret < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }
}

impl PortDriver for DrvAsynSerialPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        // C drvAsynSerialPort.c::connectIt (694-698): reject a connect on
        // an already-open link ("Link already open!") rather than opening a
        // second fd and leaking the first (along with its saved termios).
        if self.io.fd.is_some() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("{}: Link already open!", self.base.port_name),
            });
        }
        // 1. Open device
        let c_path =
            std::ffi::CString::new(self.config.device.as_str()).map_err(|_| AsynError::Status {
                status: AsynStatus::Error,
                message: "invalid device path (contains NUL)".into(),
            })?;

        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }
        self.io.fd = Some(fd);

        // Steps 2-4 configure the just-opened fd. Any failure here must
        // close the fd: `base.connected` is still false, so the `Drop`
        // impl would skip `disconnect()` and leak the descriptor.
        let setup = (|| -> AsynResult<()> {
            // C parity (drvAsynSerialPort.c:713-722): set close-on-exec right
            // after open so the serial fd is not inherited by child processes
            // (e.g. an iocsh `system` call), which would otherwise hold the
            // device open after this driver closes it.
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(AsynError::Io(std::io::Error::last_os_error()));
            }

            // 2. Save original termios
            let saved = self.get_current_termios()?;
            self.saved_termios = Some(saved);

            // 3. Configure: cfmakeraw + apply config
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            unsafe { libc::cfmakeraw(&mut t) };
            // Enable receiver, local mode
            t.c_cflag |= libc::CREAD | libc::CLOCAL;
            // C parity (drvAsynSerialPort.c:1080): the default input flags are
            // IGNBRK | IGNPAR. cfmakeraw clears IGNBRK (and never sets IGNPAR),
            // so without this a line BREAK or a framing/parity error reaches
            // the reader as a spurious 0x00 byte where C silently ignores it.
            // apply_to_termios only touches c_iflag for XON/XOFF flow, so these
            // survive the config layer.
            t.c_iflag |= libc::IGNBRK | libc::IGNPAR;
            // VMIN=1, VTIME=0 — blocking read waits for at least 1 byte.
            // Deliberate divergence from C (drvAsynSerialPort.c:1083 seeds
            // VMIN=0 and reprograms VMIN/VTIME per read from the requested
            // timeout, :899-908): C drives the read timeout through VTIME plus
            // an epicsTimer, whereas this driver gates every read with
            // poll(POLLIN, timeout) (read() below) and only reads when data is
            // ready. With that architecture VMIN=1 keeps `n == 0` meaning
            // exactly EOF/hangup; VMIN=0 would make a spurious poll-wake return
            // 0 and be misread as a disconnect. Every representable (non-
            // negative) timeout is already bounded by the poll above.
            t.c_cc[libc::VMIN] = 1;
            t.c_cc[libc::VTIME] = 0;
            // C parity (drvAsynSerialPort.c:1085-1086): the XON/XOFF flow
            // characters default to ^Q (0x11, VSTART) and ^S (0x13, VSTOP).
            // `t` was zeroed before cfmakeraw and cfmakeraw leaves c_cc
            // untouched, so without this FlowControl::Software (IXON|IXOFF)
            // would drive flow with NUL bytes instead of ^Q/^S.
            t.c_cc[libc::VSTART] = 0x11; // ^Q
            t.c_cc[libc::VSTOP] = 0x13; // ^S
            self.config.apply_to_termios(&mut t);
            self.apply_termios(&t)?;

            // C parity (drvAsynSerialPort.c:729): discard any bytes that
            // accumulated in the kernel input/output buffers before the port
            // was configured, so the first read/write starts from a clean
            // device state. C does this right before turning blocking back on.
            unsafe { libc::tcflush(fd, libc::TCIOFLUSH) };

            // 4. Restore blocking mode
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 {
                return Err(AsynError::Io(std::io::Error::last_os_error()));
            }
            if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
                return Err(AsynError::Io(std::io::Error::last_os_error()));
            }
            Ok(())
        })();
        if let Err(e) = setup {
            if let Some(fd) = self.io.fd.take() {
                unsafe { libc::close(fd) };
            }
            self.saved_termios = None;
            return Err(e);
        }

        self.base.set_connected(true);
        asyn_trace!(
            Some(self.base.trace),
            &self.base.port_name,
            TraceMask::FLOW,
            "connected to {} at {} baud",
            self.config.device,
            self.config.baud
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

        // Restore original termios if available
        if let (Some(fd), Some(saved)) = (self.io.fd, &self.saved_termios) {
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, saved) };
        }

        // Close fd
        if let Some(fd) = self.io.fd.take() {
            unsafe { libc::close(fd) };
        }
        self.saved_termios = None;

        self.base.set_connected(false);
        Ok(())
    }

    fn report(&self, level: i32) {
        // C parity (drvAsynSerialPort.c:666-680): report the connection state,
        // and at details>=1 the fd plus cumulative bytes written/read.
        eprintln!(
            "Serial line {}: {}",
            self.config.device,
            if self.base.connected {
                "Connected"
            } else {
                "Disconnected"
            }
        );
        if level >= 1 {
            eprintln!("                    fd: {}", self.io.fd.unwrap_or(-1));
            eprintln!("    Characters written: {}", self.io.n_written);
            eprintln!("       Characters read: {}", self.io.n_read);
            self.base.report_params(level.saturating_sub(1));
        }
    }

    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        self.base.check_ready()?;
        let result = match self
            .base
            .interpose_octet
            .dispatch_read(user, buf, &mut self.io)
        {
            Ok(r) => r,
            Err(e) => {
                // C parity: drvAsynSerialPort.c::closeConnection on a fatal
                // read error / EOF so the actor's auto-reconnect re-opens the
                // device. EINTR/EAGAIN are already retried inside
                // SerialIoState::read, so an error reaching here is fatal.
                if is_fatal_transport_error(&e) && self.base.connected {
                    asyn_trace!(
                        Some(self.base.trace),
                        &self.base.port_name,
                        TraceMask::FLOW,
                        "read error, disconnecting: {e}"
                    );
                    self.drop_connection();
                }
                return Err(e);
            }
        };
        asyn_trace_io!(
            Some(self.base.trace),
            &self.base.port_name,
            TraceMask::IO_DRIVER,
            &buf[..result.nbytes_transferred],
            "read"
        );
        Ok(result.nbytes_transferred)
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        self.base.check_ready()?;
        asyn_trace_io!(
            Some(self.base.trace),
            &self.base.port_name,
            TraceMask::IO_DRIVER,
            data,
            "write"
        );
        match self
            .base
            .interpose_octet
            .dispatch_write(user, data, &mut self.io)
        {
            Ok(n) => Ok(n),
            Err(e) => {
                // C parity: closeConnection on a fatal write error so the
                // next request reconnects (symmetric with read; matches
                // ip_port DRV-5).
                if is_fatal_transport_error(&e) && self.base.connected {
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
        self.base.interpose_octet.dispatch_flush(user, &mut self.io)
    }

    fn set_option(&mut self, key: &str, value: &str) -> AsynResult<()> {
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();

        match key.as_str() {
            "baud" => {
                let baud: u32 = value.parse().map_err(|_| AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("invalid baud rate: '{value}'"),
                })?;
                if !is_supported_baud(baud) {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!(
                            "unsupported baud rate: {baud} (supported: {:?})",
                            SUPPORTED_BAUDS
                        ),
                    });
                }
                if self.io.fd.is_some() {
                    let mut t = self.get_current_termios()?;
                    let speed = baud_to_speed(baud);
                    unsafe {
                        libc::cfsetispeed(&mut t, speed);
                        libc::cfsetospeed(&mut t, speed);
                    }
                    self.apply_termios(&t)?;
                }
                // C parity (drvAsynSerialPort.c:601-604): setOption restores the
                // previous baud/termios if applyOptions fails. Commit the cached
                // value only after a successful apply (or when the port is not
                // open yet, where it is applied at the next connect), so
                // getOption never reports a value the device rejected.
                self.config.baud = baud;
            }
            "bits" => {
                let bits = match value {
                    "5" => DataBits::Five,
                    "6" => DataBits::Six,
                    "7" => DataBits::Seven,
                    "8" => DataBits::Eight,
                    _ => {
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: format!("invalid data bits: '{value}' (expected 5/6/7/8)"),
                        });
                    }
                };
                if self.io.fd.is_some() {
                    let mut t = self.get_current_termios()?;
                    t.c_cflag &= !libc::CSIZE;
                    t.c_cflag |= match bits {
                        DataBits::Five => libc::CS5,
                        DataBits::Six => libc::CS6,
                        DataBits::Seven => libc::CS7,
                        DataBits::Eight => libc::CS8,
                    };
                    self.apply_termios(&t)?;
                }
                // Commit cached config only after a successful apply (see baud).
                self.config.data_bits = bits;
            }
            "parity" => {
                // C drvAsynSerialPort.c::setOption (379-395) accepts only
                // "none"/"even"/"odd" (case-insensitive); anything else is
                // asynError "Invalid parity." The single-char aliases n/e/o
                // were a Rust-only superset and are dropped to match C.
                let val_lower = value.to_ascii_lowercase();
                let parity = match val_lower.as_str() {
                    "none" => Parity::None,
                    "even" => Parity::Even,
                    "odd" => Parity::Odd,
                    _ => {
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: format!(
                                "invalid parity: '{value}' (expected none/odd/even; mark/space not supported)"
                            ),
                        });
                    }
                };
                if self.io.fd.is_some() {
                    let mut t = self.get_current_termios()?;
                    match parity {
                        Parity::None => t.c_cflag &= !libc::PARENB,
                        Parity::Even => {
                            t.c_cflag |= libc::PARENB;
                            t.c_cflag &= !libc::PARODD;
                        }
                        Parity::Odd => {
                            t.c_cflag |= libc::PARENB;
                            t.c_cflag |= libc::PARODD;
                        }
                    }
                    self.apply_termios(&t)?;
                }
                // Commit cached config only after a successful apply (see baud).
                self.config.parity = parity;
            }
            "stop" => {
                let stop = match value {
                    "1" => StopBits::One,
                    "2" => StopBits::Two,
                    _ => {
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: format!("invalid stop bits: '{value}' (expected 1/2)"),
                        });
                    }
                };
                if self.io.fd.is_some() {
                    let mut t = self.get_current_termios()?;
                    match stop {
                        StopBits::One => t.c_cflag &= !libc::CSTOPB,
                        StopBits::Two => t.c_cflag |= libc::CSTOPB,
                    }
                    self.apply_termios(&t)?;
                }
                // Commit cached config only after a successful apply (see baud).
                self.config.stop_bits = stop;
            }
            "clocal" => {
                let enabled = parse_bool_option(value)?;
                if self.io.fd.is_some() {
                    let mut t = self.get_current_termios()?;
                    if enabled {
                        t.c_cflag |= libc::CLOCAL;
                    } else {
                        t.c_cflag &= !libc::CLOCAL;
                    }
                    self.apply_termios(&t)?;
                }
            }
            "crtscts" => {
                let enabled = parse_bool_option(value)?;
                if self.io.fd.is_some() {
                    let mut t = self.get_current_termios()?;
                    if enabled {
                        t.c_cflag |= libc::CRTSCTS;
                    } else {
                        t.c_cflag &= !libc::CRTSCTS;
                    }
                    self.apply_termios(&t)?;
                }
                // Commit cached config only after a successful apply (see baud).
                if enabled {
                    self.config.flow_control = FlowControl::Hardware;
                } else if self.config.flow_control == FlowControl::Hardware {
                    self.config.flow_control = FlowControl::None;
                }
            }
            "ixon" => {
                let enabled = parse_bool_option(value)?;
                if self.io.fd.is_some() {
                    let mut t = self.get_current_termios()?;
                    if enabled {
                        t.c_iflag |= libc::IXON;
                    } else {
                        t.c_iflag &= !libc::IXON;
                    }
                    self.apply_termios(&t)?;
                }
            }
            "ixoff" => {
                let enabled = parse_bool_option(value)?;
                if self.io.fd.is_some() {
                    let mut t = self.get_current_termios()?;
                    if enabled {
                        t.c_iflag |= libc::IXOFF;
                    } else {
                        t.c_iflag &= !libc::IXOFF;
                    }
                    self.apply_termios(&t)?;
                }
            }
            "ixany" => {
                let enabled = parse_bool_option(value)?;
                if self.io.fd.is_some() {
                    let mut t = self.get_current_termios()?;
                    if enabled {
                        t.c_iflag |= libc::IXANY;
                    } else {
                        t.c_iflag &= !libc::IXANY;
                    }
                    self.apply_termios(&t)?;
                }
            }
            "break" => {
                // C parity (drvAsynSerialPort.c:507-528): "off" = no-op (early
                // asynSuccess), "" or "on" = standard break (len 0), a number =
                // break duration; anything else is "Bad number" (asynError).
                // C validates the value and acts on the fd WITHOUT guarding on
                // it being open, so a break on a closed port fails (tcsendbreak
                // EBADF -> asynError) rather than silently succeeding. Mirror
                // that order: validate first, then require the fd.
                if value != "off" {
                    let duration = if value.is_empty() || value == "on" {
                        0 // standard break duration
                    } else {
                        value.parse::<i32>().map_err(|_| AsynError::Status {
                            status: AsynStatus::Error,
                            message: format!("invalid break duration: '{value}'"),
                        })?
                    };
                    // Disconnected -> error (not a silent no-op); C reaches
                    // tcdrain/tcsendbreak on the dead fd and returns asynError.
                    let fd = self.io.fd_or_err()?;
                    // Drain output first (C parity: tcdrain before tcsendbreak).
                    if unsafe { libc::tcdrain(fd) } < 0 {
                        return Err(AsynError::Io(std::io::Error::last_os_error()));
                    }
                    let ret = unsafe { libc::tcsendbreak(fd, duration) };
                    if ret < 0 {
                        return Err(AsynError::Io(std::io::Error::last_os_error()));
                    }
                }
            }
            #[cfg(target_os = "linux")]
            "rs485_enable"
            | "rs485_rts_on_send"
            | "rs485_rts_after_send"
            | "rs485_delay_rts_before_send"
            | "rs485_delay_rts_after_send" => {
                self.set_rs485_option(&key, value)?;
            }
            other => {
                // C drvAsynSerialPort.c::setOption (lines 594-598): the empty
                // key is a silent no-op (the `epicsStrCaseCmp(key,"") != 0`
                // guard); any other unsupported key returns asynError
                // "Unsupported key". The real handlers above own every
                // supported key, so there is no generic option store.
                if !other.is_empty() {
                    return Err(AsynError::OptionNotFound(other.to_string()));
                }
            }
        }
        Ok(())
    }

    fn get_option(&self, key: &str) -> AsynResult<String> {
        match key {
            "baud" => Ok(self.config.baud.to_string()),
            "bits" => Ok(match self.config.data_bits {
                DataBits::Five => "5",
                DataBits::Six => "6",
                DataBits::Seven => "7",
                DataBits::Eight => "8",
            }
            .to_string()),
            "parity" => Ok(match self.config.parity {
                Parity::None => "none",
                Parity::Even => "even",
                Parity::Odd => "odd",
            }
            .to_string()),
            "stop" => Ok(match self.config.stop_bits {
                StopBits::One => "1",
                StopBits::Two => "2",
            }
            .to_string()),
            "clocal" => {
                if self.io.fd.is_some() {
                    let t = self.get_current_termios()?;
                    Ok(if t.c_cflag & libc::CLOCAL != 0 {
                        "Y"
                    } else {
                        "N"
                    }
                    .to_string())
                } else {
                    Ok("N".to_string())
                }
            }
            "crtscts" => {
                if self.io.fd.is_some() {
                    let t = self.get_current_termios()?;
                    Ok(if t.c_cflag & libc::CRTSCTS != 0 {
                        "Y"
                    } else {
                        "N"
                    }
                    .to_string())
                } else {
                    Ok(match self.config.flow_control {
                        FlowControl::Hardware => "Y",
                        _ => "N",
                    }
                    .to_string())
                }
            }
            "ixon" => {
                if self.io.fd.is_some() {
                    let t = self.get_current_termios()?;
                    Ok(if t.c_iflag & libc::IXON != 0 {
                        "Y"
                    } else {
                        "N"
                    }
                    .to_string())
                } else {
                    Ok("N".to_string())
                }
            }
            "ixoff" => {
                if self.io.fd.is_some() {
                    let t = self.get_current_termios()?;
                    Ok(if t.c_iflag & libc::IXOFF != 0 {
                        "Y"
                    } else {
                        "N"
                    }
                    .to_string())
                } else {
                    Ok("N".to_string())
                }
            }
            "ixany" => {
                if self.io.fd.is_some() {
                    let t = self.get_current_termios()?;
                    Ok(if t.c_iflag & libc::IXANY != 0 {
                        "Y"
                    } else {
                        "N"
                    }
                    .to_string())
                } else {
                    Ok("N".to_string())
                }
            }
            // C getOption (drvAsynSerialPort.c:204-207): "break" is a momentary
            // line action, so a read always reports "off" rather than erroring.
            "break" => Ok("off".to_string()),
            #[cfg(target_os = "linux")]
            "rs485_enable"
            | "rs485_rts_on_send"
            | "rs485_rts_after_send"
            | "rs485_delay_rts_before_send"
            | "rs485_delay_rts_after_send" => self.get_rs485_option(key),
            _ => self
                .base
                .options
                .get(key)
                .cloned()
                .ok_or_else(|| AsynError::OptionNotFound(key.to_string())),
        }
    }
}

// --- RS485 support (Linux only) ---
//
// Mirror of `<linux/serial.h>` `struct serial_rs485` — same layout
// used by `drvAsynSerialPort.c:76-77` (`struct serial_rs485 rs485`).
// Layout: 4 + 4 + 4 + 5*4 = 32 bytes. Pre-Linux-4.20 kernels read the
// full 32-byte buffer in TIOCGRS485 / TIOCSRS485 even though only the
// first three u32 fields carry data; the 5-word padding tail MUST be
// present or the ioctl silently writes garbage on some drivers.
// (PR #22 originally tried to pass a single c_ulong — the kernel
// read the next 24 bytes of stack as "padding" and some PCIe UART
// drivers latched that as a multi-µs rts delay.)
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SerialRs485 {
    flags: u32,
    delay_rts_before_send: u32,
    delay_rts_after_send: u32,
    padding: [u32; 5],
}

#[cfg(target_os = "linux")]
mod rs485_flags {
    pub const SER_RS485_ENABLED: u32 = 1 << 0;
    pub const SER_RS485_RTS_ON_SEND: u32 = 1 << 1;
    pub const SER_RS485_RTS_AFTER_SEND: u32 = 1 << 2;
}

// TIOCGRS485 = 0x542E, TIOCSRS485 = 0x542F — asm-generic/ioctls.h.
#[cfg(target_os = "linux")]
const TIOCGRS485: libc::c_ulong = 0x542E;
#[cfg(target_os = "linux")]
const TIOCSRS485: libc::c_ulong = 0x542F;

#[cfg(target_os = "linux")]
impl DrvAsynSerialPort {
    fn rs485_get(&self, fd: RawFd) -> AsynResult<SerialRs485> {
        let mut r: SerialRs485 = SerialRs485::default();
        let ret = unsafe { libc::ioctl(fd, TIOCGRS485, &mut r as *mut SerialRs485) };
        if ret < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }
        Ok(r)
    }

    fn rs485_set(&self, fd: RawFd, r: &SerialRs485) -> AsynResult<()> {
        let ret = unsafe { libc::ioctl(fd, TIOCSRS485, r as *const SerialRs485) };
        if ret < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn set_rs485_option(&mut self, key: &str, value: &str) -> AsynResult<()> {
        let fd = self.io.fd.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Disconnected,
            message: "not connected".into(),
        })?;

        let mut r = self.rs485_get(fd)?;
        let prev = r;

        use rs485_flags::*;
        match key {
            // C `drvAsynSerialPort.c:531-543`: "Y" sets ENABLED; "N"
            // clears the whole flags word (not just the bit) — match
            // that semantic exactly.
            "rs485_enable" => match value.to_ascii_uppercase().as_str() {
                "Y" => r.flags |= SER_RS485_ENABLED,
                "N" => r.flags = 0,
                _ => {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "Invalid rs485_enable value.".into(),
                    });
                }
            },
            "rs485_rts_on_send" => match value.to_ascii_uppercase().as_str() {
                "Y" => r.flags |= SER_RS485_RTS_ON_SEND,
                "N" => r.flags &= !SER_RS485_RTS_ON_SEND,
                _ => {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "Invalid rs485_rts_on_send value.".into(),
                    });
                }
            },
            "rs485_rts_after_send" => match value.to_ascii_uppercase().as_str() {
                "Y" => r.flags |= SER_RS485_RTS_AFTER_SEND,
                "N" => r.flags &= !SER_RS485_RTS_AFTER_SEND,
                _ => {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "Invalid rs485_rts_after_send value.".into(),
                    });
                }
            },
            "rs485_delay_rts_before_send" => {
                r.delay_rts_before_send = value.parse::<u32>().map_err(|_| AsynError::Status {
                    status: AsynStatus::Error,
                    message: "Bad number".into(),
                })?;
            }
            "rs485_delay_rts_after_send" => {
                r.delay_rts_after_send = value.parse::<u32>().map_err(|_| AsynError::Status {
                    status: AsynStatus::Error,
                    message: "Bad number".into(),
                })?;
            }
            _ => {}
        }

        // C `drvAsynSerialPort.c:608-613`: on TIOCSRS485 failure
        // restore the previous struct state — note that an in-kernel
        // failure may already have applied the change, but the
        // userland copy must still reflect the last-known-good value.
        if let Err(e) = self.rs485_set(fd, &r) {
            let _ = self.rs485_set(fd, &prev);
            return Err(e);
        }
        Ok(())
    }

    fn get_rs485_option(&self, key: &str) -> AsynResult<String> {
        let fd = self.io.fd.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Disconnected,
            message: "not connected".into(),
        })?;
        let r = self.rs485_get(fd)?;
        use rs485_flags::*;
        // Format matches C drvAsynSerialPort.c:210-224 — 'Y'/'N' for
        // flags, "%u" for the delay fields.
        let s = match key {
            "rs485_enable" => if r.flags & SER_RS485_ENABLED != 0 {
                "Y"
            } else {
                "N"
            }
            .to_string(),
            "rs485_rts_on_send" => if r.flags & SER_RS485_RTS_ON_SEND != 0 {
                "Y"
            } else {
                "N"
            }
            .to_string(),
            "rs485_rts_after_send" => if r.flags & SER_RS485_RTS_AFTER_SEND != 0 {
                "Y"
            } else {
                "N"
            }
            .to_string(),
            "rs485_delay_rts_before_send" => r.delay_rts_before_send.to_string(),
            "rs485_delay_rts_after_send" => r.delay_rts_after_send.to_string(),
            _ => {
                return Err(AsynError::OptionNotFound(key.to_string()));
            }
        };
        Ok(s)
    }
}

impl Drop for DrvAsynSerialPort {
    fn drop(&mut self) {
        let user = AsynUser::default();
        if self.base.connected {
            let _ = self.disconnect(&user);
        }
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;

    // --- Config parsing tests ---

    #[test]
    fn test_parse_device() {
        let cfg = SerialConfig::parse("/dev/ttyUSB0").unwrap();
        assert_eq!(cfg.device, "/dev/ttyUSB0");
        assert_eq!(cfg.baud, 9600);
        assert_eq!(cfg.data_bits, DataBits::Eight);
        assert_eq!(cfg.parity, Parity::None);
        assert_eq!(cfg.stop_bits, StopBits::One);
        assert_eq!(cfg.flow_control, FlowControl::None);
    }

    #[test]
    fn test_parse_empty_error() {
        assert!(SerialConfig::parse("").is_err());
        assert!(SerialConfig::parse("   ").is_err());
    }

    // --- Driver creation tests ---

    #[test]
    fn test_driver_initial_state() {
        let drv = DrvAsynSerialPort::new("serial1", "/dev/ttyUSB0").unwrap();
        assert!(!drv.base().connected);
        assert!(drv.base().auto_connect);
        assert!(drv.base().flags.can_block);
        // `new` is parse-only: no EOS interpose (DRV-45).
        assert_eq!(drv.base().interpose_octet.len(), 0);
    }

    /// C drvAsynSerialPort.c:1126 enables EOS by default in Configure
    /// unless noProcessEos; `configure` is the Rust analogue (DRV-45).
    #[test]
    fn test_configure_installs_eos_unless_suppressed_and_honors_no_auto_connect() {
        let default_port =
            DrvAsynSerialPort::configure("s_eos_default", "/dev/ttyS0", false, false).unwrap();
        assert_eq!(
            default_port.base().interpose_octet.len(),
            1,
            "default serial port must auto-install the EOS interpose"
        );
        assert!(default_port.base().auto_connect);

        let suppressed =
            DrvAsynSerialPort::configure("s_eos_off", "/dev/ttyS0", true, true).unwrap();
        assert_eq!(
            suppressed.base().interpose_octet.len(),
            0,
            "noProcessEos must suppress the EOS interpose"
        );
        assert!(!suppressed.base().auto_connect);
    }

    #[test]
    fn test_set_option_baud_disconnected() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        drv.set_option("baud", "115200").unwrap();
        assert_eq!(drv.config.baud, 115200);
        assert_eq!(drv.get_option("baud").unwrap(), "115200");
    }

    #[test]
    fn test_set_option_bits() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        drv.set_option("bits", "7").unwrap();
        assert_eq!(drv.config.data_bits, DataBits::Seven);
        assert_eq!(drv.get_option("bits").unwrap(), "7");
    }

    #[test]
    fn test_set_option_parity() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        drv.set_option("parity", "even").unwrap();
        assert_eq!(drv.config.parity, Parity::Even);
        assert_eq!(drv.get_option("parity").unwrap(), "even");
        drv.set_option("parity", "odd").unwrap();
        assert_eq!(drv.config.parity, Parity::Odd);
    }

    #[test]
    fn test_set_option_stop() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        drv.set_option("stop", "2").unwrap();
        assert_eq!(drv.config.stop_bits, StopBits::Two);
        assert_eq!(drv.get_option("stop").unwrap(), "2");
    }

    #[test]
    fn test_set_option_invalid_baud() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        assert!(drv.set_option("baud", "abc").is_err());
    }

    #[test]
    fn test_set_option_unsupported_baud() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        let err = drv.set_option("baud", "12345").unwrap_err();
        match err {
            AsynError::Status { message, .. } => assert!(message.contains("unsupported")),
            _ => panic!("expected unsupported baud error"),
        }
    }

    #[test]
    fn test_set_option_invalid_bits() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        assert!(drv.set_option("bits", "9").is_err());
    }

    #[test]
    fn test_set_option_key_case_insensitive() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        drv.set_option("BAUD", "115200").unwrap();
        assert_eq!(drv.config.baud, 115200);
        drv.set_option("Parity", "Even").unwrap();
        assert_eq!(drv.config.parity, Parity::Even);
    }

    #[test]
    fn test_set_option_value_trimmed() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        drv.set_option("baud", " 9600 ").unwrap();
        assert_eq!(drv.config.baud, 9600);
    }

    #[test]
    fn test_set_option_parity_case_insensitive() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        drv.set_option("parity", "EVEN").unwrap();
        assert_eq!(drv.config.parity, Parity::Even);
        drv.set_option("parity", "None").unwrap();
        assert_eq!(drv.config.parity, Parity::None);
        // Single-char aliases (n/e/o) are no longer accepted (C parity).
        assert!(drv.set_option("parity", "n").is_err());
    }

    #[test]
    fn test_set_option_parity_mark_space_unsupported() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        let err = drv.set_option("parity", "mark").unwrap_err();
        match err {
            AsynError::Status { message, .. } => {
                assert!(message.contains("mark/space not supported"))
            }
            _ => panic!("expected mark/space unsupported error"),
        }
    }

    #[test]
    fn test_parse_bool_option() {
        // C drvAsynSerialPort.c validates these options strictly Y/N
        // (case-insensitive); the looser y/yes/1/true coercion is gone.
        assert!(parse_bool_option("Y").unwrap());
        assert!(parse_bool_option("y").unwrap());
        assert!(!parse_bool_option("N").unwrap());
        assert!(!parse_bool_option("n").unwrap());
        // Tokens C rejects now error instead of silently coercing.
        for v in &["yes", "1", "true", "no", "0", "false", "maybe", ""] {
            assert!(parse_bool_option(v).is_err(), "expected err for '{v}'");
        }
    }

    #[test]
    fn get_option_break_returns_off() {
        // DRV-39: C getOption (drvAsynSerialPort.c:204-207) reports "break" as
        // "off" (a momentary line action), not an error.
        let drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        assert_eq!(drv.get_option("break").unwrap(), "off");
    }

    #[test]
    fn set_option_break_on_disconnected_errors_but_off_is_noop() {
        // DRV-43: C setOption "break" (drvAsynSerialPort.c:507-528) does not
        // guard on the fd, so a real break on a closed port fails
        // (tcsendbreak EBADF -> asynError). The Rust arm previously skipped
        // silently when disconnected. "off" stays a no-op (C asynSuccess), a
        // bad duration is rejected before the fd is touched (C order).
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();

        // "off" is a no-op even on a disconnected port.
        drv.set_option("break", "off").unwrap();

        // A real break on a disconnected port must error, not silently succeed.
        let err = drv.set_option("break", "on").unwrap_err();
        assert!(
            matches!(
                err,
                AsynError::Status {
                    status: AsynStatus::Disconnected,
                    ..
                }
            ),
            "break on a disconnected port must error, got {err:?}"
        );

        // A bad duration is rejected (validated before the fd, matching C).
        let err = drv.set_option("break", "notanumber").unwrap_err();
        match err {
            AsynError::Status { message, .. } => {
                assert!(message.contains("invalid break duration"))
            }
            other => panic!("expected invalid break duration error, got {other:?}"),
        }
    }

    #[test]
    fn test_set_option_unknown() {
        // C drvAsynSerialPort.c::setOption (594-598) rejects any non-empty
        // unsupported key (asynError "Unsupported key") and never stores it,
        // so a later getOption cannot echo it back; the empty key is a
        // silent no-op.
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();

        let err = drv.set_option("custom", "value").unwrap_err();
        assert!(matches!(err, AsynError::OptionNotFound(_)));
        assert!(drv.get_option("custom").is_err());

        // Empty key is a silent no-op (C `epicsStrCaseCmp(key,"") != 0`).
        drv.set_option("", "ignored").unwrap();
    }

    #[test]
    fn test_get_option_not_found() {
        let drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        assert!(drv.get_option("nonexistent").is_err());
    }

    #[test]
    fn test_read_write_when_disconnected() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let mut buf = [0u8; 32];
        assert!(drv.read_octet(&user, &mut buf).is_err());
        let mut user = AsynUser::new(0);
        assert!(drv.write_octet(&mut user, b"hello").is_err());
    }

    #[test]
    fn test_baud_speed_roundtrip() {
        for baud in [
            0, 50, 75, 110, 134, 150, 200, 300, 600, 1200, 1800, 2400, 4800, 9600, 19200, 38400,
            57600, 115200, 230400,
        ] {
            let speed = baud_to_speed(baud);
            assert_eq!(
                speed_to_baud(speed),
                baud,
                "roundtrip failed for baud={baud}"
            );
        }
    }

    // --- PTY integration tests ---

    fn create_pty_pair() -> Option<(RawFd, RawFd, String)> {
        let mut master: RawFd = 0;
        let mut slave: RawFd = 0;
        let mut name_buf = [0u8; 256];

        let ret = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                name_buf.as_mut_ptr() as *mut libc::c_char,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ret < 0 {
            return None;
        }

        let name = unsafe {
            std::ffi::CStr::from_ptr(name_buf.as_ptr() as *const libc::c_char)
                .to_string_lossy()
                .into_owned()
        };

        Some((master, slave, name))
    }

    struct PtyGuard {
        master: RawFd,
        slave: RawFd,
    }

    impl Drop for PtyGuard {
        fn drop(&mut self) {
            unsafe {
                libc::close(self.master);
                libc::close(self.slave);
            }
        }
    }

    #[test]
    fn test_pty_connect_disconnect() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        // Close slave — driver will reopen it
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        let user = AsynUser::default();

        assert!(!drv.base().connected);
        drv.connect(&user).unwrap();
        assert!(drv.base().connected);

        drv.disconnect(&user).unwrap();
        assert!(!drv.base().connected);
    }

    /// DRV-57: a zero-length serial read request must be rejected with
    /// asynError (C drvAsynSerialPort.c:871-875), NOT fall through to
    /// libc::read(fd, ptr, 0) -> 0 -> EOF -> disconnect, which would tear
    /// down a live serial port. The connection must stay up.
    #[test]
    fn pty_zero_length_read_rejected_not_eof_teardown() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_maxchars", &slave_name).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        assert!(drv.base().connected);

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
            "zero-length serial read must be rejected with asynError, got {res:?}"
        );
        assert!(
            drv.base().connected,
            "zero-length serial read must not tear down the connection"
        );
    }

    #[test]
    fn test_pty_write_read_roundtrip() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        // Write from driver, read from master
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user, b"hello").unwrap();

        let mut buf = [0u8; 32];
        let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        assert!(n > 0);
        assert_eq!(&buf[..n as usize], b"hello");

        // Write from master, read from driver
        let msg = b"world";
        unsafe { libc::write(master, msg.as_ptr() as *const libc::c_void, msg.len()) };

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut rbuf = [0u8; 32];
        let n = drv.read_octet(&user, &mut rbuf).unwrap();
        assert_eq!(&rbuf[..n], b"world");
    }

    #[test]
    fn test_pty_read_timeout() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        // Don't write anything — read should timeout
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
    fn test_pty_read_error_disconnects() {
        // DRV-31: a fatal read error / EOF must tear the connection down so
        // the actor's auto-reconnect re-opens the device. Without it the port
        // stays `connected` with a dead fd and never self-heals.
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        assert!(drv.base().connected);

        // Break the link: closing the master makes the driver's slave fd
        // return EOF (macOS) or EIO (Linux) on the next read — both fatal.
        unsafe { libc::close(master) };

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let mut buf = [0u8; 32];
        let err = drv.read_octet(&user, &mut buf).unwrap_err();
        assert!(
            is_fatal_transport_error(&err),
            "expected a fatal transport error, got {err:?}"
        );
        assert!(
            !drv.base().connected,
            "DRV-31: fatal read error must set connected=false"
        );
    }

    #[test]
    fn test_pty_eos_interpose() {
        use crate::interpose::eos::{EosConfig, EosInterpose};

        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        let eos = EosInterpose::new(EosConfig {
            input_eos: vec![b'\r', b'\n'],
            output_eos: vec![],
        });
        drv.push_interpose(Box::new(eos));

        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        // Master sends "OK\r\n"
        let msg = b"OK\r\n";
        unsafe { libc::write(master, msg.as_ptr() as *const libc::c_void, msg.len()) };

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 32];
        let n = drv.read_octet(&user, &mut buf).unwrap();
        // EOS should strip the terminator
        assert_eq!(&buf[..n], b"OK");
    }

    #[test]
    fn test_pty_set_option_baud() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        drv.set_option("baud", "115200").unwrap();
        assert_eq!(drv.config.baud, 115200);

        // Verify via tcgetattr
        let t = drv.get_current_termios().unwrap();
        let actual_speed = unsafe { libc::cfgetospeed(&t) };
        assert_eq!(actual_speed, libc::B115200);
    }

    #[test]
    fn test_pty_connect_rejects_double_open() {
        // C drvAsynSerialPort.c::connectIt (694-698) returns asynError
        // "Link already open!" on a connect to an already-open link,
        // rather than opening a second fd and leaking the first.
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        let first_fd = drv.io.fd;
        assert!(first_fd.is_some());

        let err = drv.connect(&user).unwrap_err();
        assert!(matches!(err, AsynError::Status { .. }));
        // The original fd (and its saved termios) is left intact.
        assert_eq!(drv.io.fd, first_fd);
        assert!(drv.saved_termios.is_some());
    }

    #[test]
    fn test_pty_runtime_integration() {
        use crate::runtime::{RuntimeConfig, create_port_runtime};

        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let drv = DrvAsynSerialPort::new("pty_rt", &slave_name).unwrap();
        let (runtime_handle, _jh) = create_port_runtime(drv, RuntimeConfig::default());
        let ph = runtime_handle.port_handle();

        // Write via PortHandle
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        ph.submit_blocking(
            crate::request::RequestOp::OctetWrite {
                data: b"ping".to_vec(),
            },
            user,
        )
        .unwrap();

        // Read from master
        let mut buf = [0u8; 32];
        let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        assert!(n > 0);
        assert_eq!(&buf[..n as usize], b"ping");

        // Master sends response
        let resp = b"pong";
        unsafe { libc::write(master, resp.as_ptr() as *const libc::c_void, resp.len()) };

        // Read via PortHandle
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let result = ph
            .submit_blocking(crate::request::RequestOp::OctetRead { buf_size: 32 }, user)
            .unwrap();
        assert_eq!(result.data.as_deref(), Some(b"pong".as_slice()));

        runtime_handle.shutdown_and_wait();
    }

    #[test]
    fn test_pty_termios_restored_on_disconnect() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        // Read original termios before the driver touches it
        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        // saved_termios should exist
        assert!(drv.saved_termios.is_some());
        let saved = drv.saved_termios.unwrap();

        // cfmakeraw changes key flags; verify they differ now
        let current = drv.get_current_termios().unwrap();
        // Raw mode typically clears ECHO, ICANON in c_lflag
        assert_ne!(
            current.c_lflag & libc::ECHO,
            saved.c_lflag & libc::ECHO,
            "raw mode should have changed ECHO flag"
        );

        // Re-set saved_termios (disconnect reads from it)
        drv.saved_termios = Some(saved);
        drv.disconnect(&user).unwrap();
        assert!(drv.saved_termios.is_none());
        assert!(!drv.base().connected);

        // Now reopen and verify key flags were restored by reading termios
        // from the same PTY slave path. Re-open to read the restored state.
        let c_path = std::ffi::CString::new(slave_name.as_str()).unwrap();
        let fd2 = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK,
            )
        };
        if fd2 >= 0 {
            let mut restored: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(fd2, &mut restored) } == 0 {
                // Compare key flags (kernel may adjust some bits, so check important ones)
                assert_eq!(
                    restored.c_lflag & libc::ECHO,
                    saved.c_lflag & libc::ECHO,
                    "ECHO flag should be restored"
                );
                assert_eq!(
                    restored.c_lflag & libc::ICANON,
                    saved.c_lflag & libc::ICANON,
                    "ICANON flag should be restored"
                );
                assert_eq!(
                    restored.c_cflag & libc::CSIZE,
                    saved.c_cflag & libc::CSIZE,
                    "CSIZE should be restored"
                );
            }
            unsafe { libc::close(fd2) };
        }
    }

    /// DRV-32: C (drvAsynSerialPort.c:1080) sets the default input flags to
    /// IGNBRK | IGNPAR so a line BREAK / framing-parity error is ignored
    /// rather than delivered as a spurious 0x00 byte. cfmakeraw clears IGNBRK
    /// and never sets IGNPAR, so the driver must restore them after cfmakeraw.
    #[test]
    fn pty_termios_sets_ignbrk_ignpar() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_ignbrk", &slave_name).unwrap();
        drv.connect(&AsynUser::default()).unwrap();

        let t = drv.get_current_termios().unwrap();
        assert_ne!(
            t.c_iflag & libc::IGNBRK,
            0,
            "IGNBRK must be set (C default, drvAsynSerialPort.c:1080)"
        );
        assert_ne!(
            t.c_iflag & libc::IGNPAR,
            0,
            "IGNPAR must be set (C default, drvAsynSerialPort.c:1080)"
        );
    }

    /// DRV-33: C (drvAsynSerialPort.c:1085-1086) seeds the XON/XOFF flow
    /// characters to ^Q (0x11, VSTART) and ^S (0x13, VSTOP). cfmakeraw leaves
    /// c_cc untouched and `t` is zeroed first, so the driver must set them or
    /// software flow control would use NUL instead of ^Q/^S.
    #[test]
    fn pty_termios_sets_xon_xoff_chars() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_xonxoff", &slave_name).unwrap();
        drv.connect(&AsynUser::default()).unwrap();

        let t = drv.get_current_termios().unwrap();
        assert_eq!(t.c_cc[libc::VSTART], 0x11, "VSTART must be ^Q (C default)");
        assert_eq!(t.c_cc[libc::VSTOP], 0x13, "VSTOP must be ^S (C default)");
    }

    /// DRV-35: C setOption (drvAsynSerialPort.c:601-604) restores the previous
    /// baud/termios if applyOptions fails. The Rust driver must not leave the
    /// cached config reporting a value the device rejected. Point the driver at
    /// a non-tty fd so the apply path (tcgetattr/tcsetattr) fails, then assert
    /// the cached baud is unchanged.
    #[test]
    fn set_option_does_not_commit_cached_config_on_apply_failure() {
        let mut drv = DrvAsynSerialPort::new("rollback", "/dev/null").unwrap();
        // A /dev/null fd is open but not a terminal: tcgetattr -> ENOTTY.
        let badfd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
        assert!(badfd >= 0, "could not open /dev/null");
        drv.io.fd = Some(badfd);

        // Default cached baud is 9600.
        assert_eq!(drv.get_option("baud").unwrap(), "9600");

        // The apply path fails (tcgetattr ENOTTY on /dev/null) ...
        let r = drv.set_option("baud", "115200");
        assert!(r.is_err(), "set_option must fail when apply fails");

        // ... and the cached config must not have been mutated.
        assert_eq!(
            drv.get_option("baud").unwrap(),
            "9600",
            "cached baud must stay 9600 when apply fails (C restores baudPrev)"
        );

        // Clean up the fd ourselves (the driver never owned a real connection).
        drv.io.fd = None;
        unsafe { libc::close(badfd) };
    }

    /// DRV-37: C writeIt (drvAsynSerialPort.c:815-842) arms one timer for the
    /// whole write, so the timeout bounds TOTAL write time. The old Rust write
    /// reused the full timeout on every poll, so a peer that drains a little at
    /// a time (each gap under the timeout) would never trip a per-poll timeout
    /// and the write would run to completion well past the deadline. A slow
    /// drain (4 KiB / 10 ms ~= 400 KiB/s) keeps each POLLOUT gap far under the
    /// 300 ms timeout while a 512 KiB payload needs ~1.3 s to drain: the
    /// total-deadline fix times out at ~300 ms; the per-poll bug would complete.
    #[test]
    fn pty_write_timeout_bounds_total_not_per_poll() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_write_total", &slave_name).unwrap();
        drv.connect(&AsynUser::default()).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !stop2.load(Ordering::Relaxed) {
                let n =
                    unsafe { libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n <= 0 {
                    break; // slave closed (EOF) or error
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let payload = vec![0x5Au8; 512 * 1024];
        let mut user = AsynUser::new(0).with_timeout(Duration::from_millis(300));
        let start = Instant::now();
        let res = drv.write_octet(&mut user, &payload);
        let elapsed = start.elapsed();

        // Close the slave fd so the reader's blocking read returns EOF and the
        // thread exits, then join before the PtyGuard closes the master fd.
        stop.store(true, Ordering::Relaxed);
        drv.disconnect(&AsynUser::default()).ok();
        let _ = reader.join();

        match res {
            Err(AsynError::Status {
                status: AsynStatus::Timeout,
                ..
            }) => {}
            other => panic!("expected total-deadline Timeout, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_millis(900),
            "total write time must be bounded by ~the timeout, took {elapsed:?}"
        );
    }

    /// DRV-41: C connectIt (drvAsynSerialPort.c:713-722) sets FD_CLOEXEC on the
    /// serial fd right after open so it is not inherited across exec.
    #[test]
    fn pty_connect_sets_cloexec() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_cloexec", &slave_name).unwrap();
        drv.connect(&AsynUser::default()).unwrap();

        let fd = drv.io.fd.expect("connected fd");
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0, "F_GETFD failed");
        assert!(
            flags & libc::FD_CLOEXEC != 0,
            "FD_CLOEXEC must be set after connect (C parity)"
        );
    }

    /// DRV-44: C report (drvAsynSerialPort.c:666-680) shows cumulative
    /// nWritten/nRead. Verify the counters track real I/O and report() runs at
    /// every level without panicking.
    #[test]
    fn pty_report_tracks_byte_counters() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_report", &slave_name).unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        assert_eq!(drv.io.n_written, 0);
        assert_eq!(drv.io.n_read, 0);

        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user, b"hello").unwrap();
        assert_eq!(drv.io.n_written, 5, "n_written must track bytes written");

        let msg = b"world";
        unsafe { libc::write(master, msg.as_ptr() as *const libc::c_void, msg.len()) };
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut rbuf = [0u8; 32];
        let n = drv.read_octet(&user, &mut rbuf).unwrap();
        assert!(n > 0);
        assert_eq!(drv.io.n_read, n as u64, "n_read must track bytes read");

        // report() must not panic at any level.
        drv.report(0);
        drv.report(2);
    }
}
