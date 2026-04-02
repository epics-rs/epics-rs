//! Serial port driver (drvAsynSerialPort equivalent).
//!
//! Uses `libc` termios directly for serial I/O. Unix-only (`#[cfg(unix)]`).

use std::os::unix::io::RawFd;
use std::time::Duration;

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
    match value.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "1" | "true" => Ok(true),
        "n" | "no" | "0" | "false" => Ok(false),
        _ => Err(AsynError::Status {
            status: AsynStatus::Error,
            message: format!(
                "invalid boolean value: '{value}' (expected y/yes/1/true or n/no/0/false)"
            ),
        }),
    }
}

// --- I/O state ---

struct SerialIoState {
    fd: Option<RawFd>,
}

impl SerialIoState {
    fn new() -> Self {
        Self { fd: None }
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
        let timeout_ms = duration_to_poll_ms(user.timeout);

        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };

        let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if ret < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }
        if ret == 0 {
            return Err(AsynError::Status {
                status: AsynStatus::Timeout,
                message: "serial read timeout".into(),
            });
        }

        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }
        if n == 0 {
            return Err(AsynError::Status {
                status: AsynStatus::Disconnected,
                message: "serial port EOF".into(),
            });
        }

        Ok(OctetReadResult {
            nbytes_transferred: n as usize,
            eom_reason: EomReason::CNT,
        })
    }

    fn write(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let fd = self.fd_or_err()?;
        let timeout_ms = duration_to_poll_ms(user.timeout);

        let mut total = 0usize;
        while total < data.len() {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };

            let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if ret < 0 {
                return Err(AsynError::Io(std::io::Error::last_os_error()));
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
                return Err(AsynError::Io(std::io::Error::last_os_error()));
            }
            total += n as usize;
        }

        Ok(total)
    }

    fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
        if let Some(fd) = self.fd {
            let ret = unsafe { libc::tcdrain(fd) };
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

impl DrvAsynSerialPort {
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

    /// Push an interpose layer onto the octet I/O stack.
    pub fn push_interpose(&mut self, layer: Box<dyn crate::interpose::OctetInterpose>) {
        self.base.push_octet_interpose(layer);
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

        // 2. Save original termios
        let saved = self.get_current_termios()?;
        self.saved_termios = Some(saved);

        // 3. Configure: cfmakeraw + apply config
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        unsafe { libc::cfmakeraw(&mut t) };
        // Enable receiver, local mode
        t.c_cflag |= libc::CREAD | libc::CLOCAL;
        // VMIN=1, VTIME=0 — blocking read waits for at least 1 byte
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        self.config.apply_to_termios(&mut t);
        self.apply_termios(&t)?;

        // 4. Restore blocking mode
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags >= 0 {
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) };
        }

        self.base.connected = true;
        self.base.announce_exception(AsynException::Connect, -1);
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

        self.base.connected = false;
        self.base.announce_exception(AsynException::Connect, -1);
        Ok(())
    }

    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        self.base.check_ready()?;
        let result = self
            .base
            .interpose_octet
            .dispatch_read(user, buf, &mut self.io)?;
        asyn_trace_io!(
            Some(self.base.trace),
            &self.base.port_name,
            TraceMask::IO_DRIVER,
            &buf[..result.nbytes_transferred],
            "read"
        );
        Ok(result.nbytes_transferred)
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
                self.config.baud = baud;
                if self.io.fd.is_some() {
                    let mut t = self.get_current_termios()?;
                    let speed = baud_to_speed(baud);
                    unsafe {
                        libc::cfsetispeed(&mut t, speed);
                        libc::cfsetospeed(&mut t, speed);
                    }
                    self.apply_termios(&t)?;
                }
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
                self.config.data_bits = bits;
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
            }
            "parity" => {
                let val_lower = value.to_ascii_lowercase();
                let parity = match val_lower.as_str() {
                    "none" | "n" => Parity::None,
                    "even" | "e" => Parity::Even,
                    "odd" | "o" => Parity::Odd,
                    _ => {
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: format!(
                                "invalid parity: '{value}' (expected none/odd/even; mark/space not supported)"
                            ),
                        });
                    }
                };
                self.config.parity = parity;
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
                self.config.stop_bits = stop;
                if self.io.fd.is_some() {
                    let mut t = self.get_current_termios()?;
                    match stop {
                        StopBits::One => t.c_cflag &= !libc::CSTOPB,
                        StopBits::Two => t.c_cflag |= libc::CSTOPB,
                    }
                    self.apply_termios(&t)?;
                }
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
                if enabled {
                    self.config.flow_control = FlowControl::Hardware;
                } else if self.config.flow_control == FlowControl::Hardware {
                    self.config.flow_control = FlowControl::None;
                }
                if self.io.fd.is_some() {
                    let mut t = self.get_current_termios()?;
                    if enabled {
                        t.c_cflag |= libc::CRTSCTS;
                    } else {
                        t.c_cflag &= !libc::CRTSCTS;
                    }
                    self.apply_termios(&t)?;
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
            _ => {
                self.base.options.insert(key.to_string(), value.to_string());
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
            _ => self
                .base
                .options
                .get(key)
                .cloned()
                .ok_or_else(|| AsynError::OptionNotFound(key.to_string())),
        }
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
        drv.set_option("parity", "O").unwrap();
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
        drv.set_option("parity", "n").unwrap();
        assert_eq!(drv.config.parity, Parity::None);
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
        // Truthy
        for v in &["y", "Y", "yes", "YES", "Yes", "1", "true", "TRUE", "True"] {
            assert!(parse_bool_option(v).unwrap(), "expected true for '{v}'");
        }
        // Falsy
        for v in &["n", "N", "no", "NO", "No", "0", "false", "FALSE", "False"] {
            assert!(!parse_bool_option(v).unwrap(), "expected false for '{v}'");
        }
        // Invalid
        assert!(parse_bool_option("maybe").is_err());
        assert!(parse_bool_option("").is_err());
    }

    #[test]
    fn test_set_option_unknown() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        drv.set_option("custom", "value").unwrap();
        assert_eq!(drv.get_option("custom").unwrap(), "value");
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
}
