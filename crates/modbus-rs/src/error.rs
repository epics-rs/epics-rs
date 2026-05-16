//! Error types for the Modbus driver.

use std::fmt;

/// Modbus protocol exception codes (returned in an exception response,
/// function code with the high bit set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionCode {
    /// 0x01 — function code not supported by the slave.
    IllegalFunction,
    /// 0x02 — data address not allowed for the slave.
    IllegalDataAddress,
    /// 0x03 — value in the request data field is not allowed.
    IllegalDataValue,
    /// 0x04 — slave failed to perform the requested action.
    SlaveDeviceFailure,
    /// 0x05 — request accepted, processing in progress (long duration).
    Acknowledge,
    /// 0x06 — slave is busy processing a long-duration command.
    SlaveDeviceBusy,
    /// 0x08 — memory parity error on the slave.
    MemoryParityError,
    /// 0x0A — gateway path unavailable.
    GatewayPathUnavailable,
    /// 0x0B — gateway target device failed to respond.
    GatewayTargetFailed,
    /// Any code not covered above.
    Other(u8),
}

impl ExceptionCode {
    /// Decode a raw exception byte.
    pub fn from_u8(code: u8) -> Self {
        match code {
            0x01 => Self::IllegalFunction,
            0x02 => Self::IllegalDataAddress,
            0x03 => Self::IllegalDataValue,
            0x04 => Self::SlaveDeviceFailure,
            0x05 => Self::Acknowledge,
            0x06 => Self::SlaveDeviceBusy,
            0x08 => Self::MemoryParityError,
            0x0A => Self::GatewayPathUnavailable,
            0x0B => Self::GatewayTargetFailed,
            other => Self::Other(other),
        }
    }

    /// The raw exception byte.
    pub fn as_u8(self) -> u8 {
        match self {
            Self::IllegalFunction => 0x01,
            Self::IllegalDataAddress => 0x02,
            Self::IllegalDataValue => 0x03,
            Self::SlaveDeviceFailure => 0x04,
            Self::Acknowledge => 0x05,
            Self::SlaveDeviceBusy => 0x06,
            Self::MemoryParityError => 0x08,
            Self::GatewayPathUnavailable => 0x0A,
            Self::GatewayTargetFailed => 0x0B,
            Self::Other(c) => c,
        }
    }
}

impl fmt::Display for ExceptionCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::IllegalFunction => "illegal function",
            Self::IllegalDataAddress => "illegal data address",
            Self::IllegalDataValue => "illegal data value",
            Self::SlaveDeviceFailure => "slave device failure",
            Self::Acknowledge => "acknowledge",
            Self::SlaveDeviceBusy => "slave device busy",
            Self::MemoryParityError => "memory parity error",
            Self::GatewayPathUnavailable => "gateway path unavailable",
            Self::GatewayTargetFailed => "gateway target device failed to respond",
            Self::Other(c) => return write!(f, "exception code 0x{c:02X}"),
        };
        f.write_str(s)
    }
}

/// Errors produced by the Modbus protocol, framing, and driver layers.
#[derive(Debug, thiserror::Error)]
pub enum ModbusError {
    /// The slave returned a Modbus exception response.
    #[error("modbus exception: {0}")]
    Exception(ExceptionCode),

    /// A received frame was shorter than the protocol requires.
    #[error("frame too short: got {got} bytes, need at least {need}")]
    FrameTooShort {
        /// Bytes actually received.
        got: usize,
        /// Minimum bytes required.
        need: usize,
    },

    /// A frame exceeded `MAX_MODBUS_FRAME_SIZE`.
    #[error("frame too large: {0} bytes exceeds the {max} byte limit", max = crate::protocol::MAX_MODBUS_FRAME_SIZE)]
    FrameTooLarge(usize),

    /// RTU CRC-16 check failed.
    #[error("RTU CRC check failed")]
    CrcError,

    /// ASCII LRC check failed.
    #[error("ASCII LRC check failed: received 0x{received:02X}, computed 0x{computed:02X}")]
    LrcError {
        /// LRC byte in the frame.
        received: u8,
        /// LRC computed over the frame.
        computed: u8,
    },

    /// An ASCII frame did not start with the ':' marker.
    #[error("ASCII frame missing ':' start marker")]
    MissingAsciiMarker,

    /// The response function code did not match the request.
    #[error("function code mismatch: requested 0x{requested:02X}, got 0x{got:02X}")]
    FunctionMismatch {
        /// Function code that was sent.
        requested: u8,
        /// Function code in the response.
        got: u8,
    },

    /// A register/coil offset fell outside the configured address range.
    #[error("offset {offset} out of range (configured length {length})")]
    OffsetOutOfRange {
        /// Offending offset.
        offset: i32,
        /// Configured Modbus length.
        length: i32,
    },

    /// The configured Modbus function code is not valid.
    #[error("invalid Modbus function code: {0}")]
    InvalidFunction(i32),

    /// I/O error from the underlying asyn octet port.
    #[error("octet I/O error: {0}")]
    Io(String),

    /// The driver timed out waiting for a response.
    #[error("Modbus I/O timed out")]
    Timeout,

    /// The driver is shutting down.
    #[error("driver is exiting")]
    Exiting,
}

/// Convenience result alias for the crate.
pub type ModbusResult<T> = Result<T, ModbusError>;
