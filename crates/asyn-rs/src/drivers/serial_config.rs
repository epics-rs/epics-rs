//! Platform-independent serial line configuration.
//!
//! Shared by the POSIX termios backend ([`super::serial_port`], unix) and the
//! Win32 DCB backend ([`super::serial_port`] on Windows, from
//! `serial_port_win32.rs`). C asyn keeps the two device backends
//! (`drvAsynSerialPort.c` / `drvAsynSerialPortWin32.c`) as separate files that
//! share only the spec grammar and option names; this module is the single
//! source of that shared grammar so the two Rust backends cannot drift.

use crate::error::{AsynError, AsynResult, AsynStatus};

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
}

/// Parse a boolean option value.
///
/// Accepted truthy value (case-insensitive): `Y`. Accepted falsy value
/// (case-insensitive): `N`. Returns `Err` for any other value.
pub fn parse_bool_option(value: &str) -> AsynResult<bool> {
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
