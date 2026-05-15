//! Request types for the port actor.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::SystemTime;

use crate::error::AsynStatus;

/// A param value to set directly in the store (no writeInt32/on_param_change).
/// Mirrors C ADCore's setIntegerParam/setDoubleParam.
#[derive(Debug, Clone)]
pub enum ParamSetValue {
    Int32 {
        reason: usize,
        addr: i32,
        value: i32,
    },
    Float64 {
        reason: usize,
        addr: i32,
        value: f64,
    },
    Octet {
        reason: usize,
        addr: i32,
        value: String,
    },
    Float64Array {
        reason: usize,
        addr: i32,
        value: Vec<f64>,
    },
    UInt32Digital {
        reason: usize,
        addr: i32,
        value: u32,
        mask: u32,
    },
}

/// Operation the worker thread will dispatch to the port driver.
#[derive(Debug, Clone)]
pub enum RequestOp {
    OctetWrite {
        data: Vec<u8>,
    },
    OctetRead {
        buf_size: usize,
    },
    OctetWriteRead {
        data: Vec<u8>,
        buf_size: usize,
    },
    Int32Write {
        value: i32,
    },
    Int32Read,
    Int64Write {
        value: i64,
    },
    Int64Read,
    Float64Write {
        value: f64,
    },
    Float64Read,
    UInt32DigitalWrite {
        value: u32,
        mask: u32,
    },
    UInt32DigitalRead {
        mask: u32,
    },
    Flush,
    /// Connect to the port (bypass enabled/connected checks).
    Connect,
    /// Disconnect from the port (bypass enabled/connected checks).
    Disconnect,
    /// Permanently shut down a `ASYN_DESTRUCTIBLE` port. C parity:
    /// `asynManager.c::shutdownPort` (lines 2251-2308). Marks the
    /// port defunct so every subsequent request short-circuits;
    /// idempotent; broadcasts `AsynException::Shutdown`.
    ShutdownPort,
    /// Connect a specific device address (multi-device ports).
    ConnectAddr,
    /// Disconnect a specific device address (multi-device ports).
    DisconnectAddr,
    /// Enable a specific device address (multi-device ports).
    EnableAddr,
    /// Disable a specific device address (multi-device ports).
    DisableAddr,
    /// Enable / disable the entire port. C parity:
    /// `pasynManager->enable(pasynUser, enable)`
    /// (`asynManager.c::enable`, fired by asynRecord `ENBL` writes
    /// at `asynRecord.c:484-486`).
    SetEnable {
        yes: bool,
    },
    /// Enable / disable auto-connect for the port. C parity:
    /// `pasynManager->autoConnect(pasynUser, autoConnect)`
    /// (`asynManager.c::autoConnect`, fired by asynRecord `AUCT`
    /// writes at `asynRecord.c:481-482`). `asynExceptionAutoConnect`
    /// is emitted unconditionally on every call.
    SetAutoConnect {
        yes: bool,
    },
    /// Query int32 bounds (low, high).
    GetBoundsInt32,
    /// Query int64 bounds (low, high).
    GetBoundsInt64,
    /// Block the port: only this user's requests will be dequeued until unblocked.
    BlockProcess,
    /// Unblock the port.
    UnblockProcess,
    /// Resolve a driver info string to a parameter reason index.
    DrvUserCreate {
        drv_info: String,
    },
    /// Read an enum value (index + string choices).
    EnumRead,
    /// Write an enum index.
    EnumWrite {
        index: usize,
    },
    /// Read an i32 array.
    Int32ArrayRead {
        max_elements: usize,
    },
    /// Write an i32 array.
    Int32ArrayWrite {
        data: Vec<i32>,
    },
    /// Read an f64 array.
    Float64ArrayRead {
        max_elements: usize,
    },
    /// Write an f64 array.
    Float64ArrayWrite {
        data: Vec<f64>,
    },
    /// Read an i8 array.
    Int8ArrayRead {
        max_elements: usize,
    },
    /// Write an i8 array.
    Int8ArrayWrite {
        data: Vec<i8>,
    },
    /// Read an i16 array.
    Int16ArrayRead {
        max_elements: usize,
    },
    /// Write an i16 array.
    Int16ArrayWrite {
        data: Vec<i16>,
    },
    /// Read an i64 array.
    Int64ArrayRead {
        max_elements: usize,
    },
    /// Write an i64 array.
    Int64ArrayWrite {
        data: Vec<i64>,
    },
    /// Read an f32 array.
    Float32ArrayRead {
        max_elements: usize,
    },
    /// Write an f32 array.
    Float32ArrayWrite {
        data: Vec<f32>,
    },
    /// Set params directly in the store (like C setIntegerParam/setDoubleParam)
    /// and then fire interrupt notifications (callParamCallbacks).
    /// Does NOT trigger writeInt32/on_param_change — avoids re-entrancy.
    CallParamCallbacks {
        addr: i32,
        /// Param updates to apply before firing callbacks.
        /// Empty = just fire callbacks for previously changed params.
        updates: Vec<ParamSetValue>,
    },
    /// Get a port/driver option by key.
    GetOption {
        key: String,
    },
    /// Set a port/driver option by key.
    SetOption {
        key: String,
        value: String,
    },
    /// Print a driver report (matches C `asynManager->report` /
    /// iocsh `asynReport`). The actor calls
    /// [`crate::port::PortDriver::report`] which writes to stderr
    /// at the requested verbosity. Carried by the actor so the
    /// driver is observed from its own thread (consistent with C
    /// asyn's `pport->lock` invariant for `report`).
    Report {
        level: i32,
    },
    /// Set the port's input EOS bytes — C `pasynOctet->setInputEos`.
    /// Drives the same `PortDriver::set_input_eos(&[u8])` hook the EOS
    /// interpose layer reads, so asynRecord IEOS writes survive a
    /// round trip through the actor (previously routed through the
    /// generic option HashMap which no driver consumes).
    SetInputEos {
        eos: Vec<u8>,
    },
    /// Set the port's output EOS bytes — C `pasynOctet->setOutputEos`.
    SetOutputEos {
        eos: Vec<u8>,
    },
}

/// Result returned by the worker after executing a request.
#[derive(Debug)]
pub struct RequestResult {
    pub status: AsynStatus,
    pub message: String,
    pub nbytes: usize,
    pub data: Option<Vec<u8>>,
    pub int_val: Option<i32>,
    pub int64_val: Option<i64>,
    pub float_val: Option<f64>,
    pub uint_val: Option<u32>,
    /// Reason index (from DrvUserCreate).
    pub reason: Option<usize>,
    /// Enum index (from EnumRead).
    pub enum_index: Option<usize>,
    /// i32 array data (from Int32ArrayRead).
    pub int32_array: Option<Vec<i32>>,
    /// f64 array data (from Float64ArrayRead).
    pub float64_array: Option<Vec<f64>>,
    /// i8 array data (from Int8ArrayRead).
    pub int8_array: Option<Vec<i8>>,
    /// i16 array data (from Int16ArrayRead).
    pub int16_array: Option<Vec<i16>>,
    /// i64 array data (from Int64ArrayRead).
    pub int64_array: Option<Vec<i64>>,
    /// f32 array data (from Float32ArrayRead).
    pub float32_array: Option<Vec<f32>>,
    /// Alarm status from the driver param store (populated on reads).
    pub alarm_status: u16,
    /// Alarm severity from the driver param store (populated on reads).
    pub alarm_severity: u16,
    /// Timestamp from the driver param store (populated on reads).
    pub timestamp: Option<SystemTime>,
    /// Option value string (from GetOption).
    pub option_value: Option<String>,
    /// Int64 bounds (from GetBoundsInt32/Int64).
    pub bounds: Option<(i64, i64)>,
    /// End-of-message reason flags from an octet read.
    ///
    /// C parity: `asynOctet::read` returns `nbytes` together with
    /// `int *eomReason` (`interfaces/asynOctet.h:38-40`). The flags
    /// `ASYN_EOM_CNT | ASYN_EOM_EOS | ASYN_EOM_END` mirror
    /// [`crate::interpose::EomReason`]. Stored as `u32` so the
    /// request layer stays bitflag-crate-free; converters live on
    /// `EomReason::from_bits_truncate`.
    pub eom_reason: u32,
}

impl RequestResult {
    fn base() -> Self {
        Self {
            status: AsynStatus::Success,
            message: String::new(),
            nbytes: 0,
            data: None,
            int_val: None,
            int64_val: None,
            float_val: None,
            uint_val: None,
            reason: None,
            enum_index: None,
            int32_array: None,
            float64_array: None,
            int8_array: None,
            int16_array: None,
            int64_array: None,
            float32_array: None,
            alarm_status: 0,
            alarm_severity: 0,
            timestamp: None,
            option_value: None,
            bounds: None,
            eom_reason: 0,
        }
    }

    pub fn write_ok() -> Self {
        Self::base()
    }

    pub fn octet_read(buf: Vec<u8>, nbytes: usize) -> Self {
        Self {
            nbytes,
            data: Some(buf),
            ..Self::base()
        }
    }

    /// Variant of [`Self::octet_read`] that carries the
    /// end-of-message reason flags returned by
    /// [`crate::port::PortDriver::io_read_octet_eom`]. The raw `u32`
    /// is decoded with `EomReason::from_bits_truncate` on the
    /// consumer side.
    pub fn octet_read_eom(buf: Vec<u8>, nbytes: usize, eom_reason: u32) -> Self {
        Self {
            nbytes,
            data: Some(buf),
            eom_reason,
            ..Self::base()
        }
    }

    pub fn int32_read(value: i32) -> Self {
        Self {
            int_val: Some(value),
            ..Self::base()
        }
    }

    pub fn int64_read(value: i64) -> Self {
        Self {
            int64_val: Some(value),
            ..Self::base()
        }
    }

    pub fn float64_read(value: f64) -> Self {
        Self {
            float_val: Some(value),
            ..Self::base()
        }
    }

    pub fn uint32_read(value: u32) -> Self {
        Self {
            uint_val: Some(value),
            ..Self::base()
        }
    }

    pub fn drv_user_create(reason: usize) -> Self {
        Self {
            reason: Some(reason),
            ..Self::base()
        }
    }

    pub fn enum_read(index: usize) -> Self {
        Self {
            enum_index: Some(index),
            ..Self::base()
        }
    }

    pub fn int32_array_read(data: Vec<i32>) -> Self {
        Self {
            int32_array: Some(data),
            ..Self::base()
        }
    }

    pub fn float64_array_read(data: Vec<f64>) -> Self {
        Self {
            float64_array: Some(data),
            ..Self::base()
        }
    }

    pub fn int8_array_read(data: Vec<i8>) -> Self {
        Self {
            int8_array: Some(data),
            ..Self::base()
        }
    }

    pub fn int16_array_read(data: Vec<i16>) -> Self {
        Self {
            int16_array: Some(data),
            ..Self::base()
        }
    }

    pub fn int64_array_read(data: Vec<i64>) -> Self {
        Self {
            int64_array: Some(data),
            ..Self::base()
        }
    }

    pub fn float32_array_read(data: Vec<f32>) -> Self {
        Self {
            float32_array: Some(data),
            ..Self::base()
        }
    }

    pub fn option_read(value: String) -> Self {
        Self {
            option_value: Some(value),
            ..Self::base()
        }
    }

    pub fn bounds_read(low: i64, high: i64) -> Self {
        Self {
            bounds: Some((low, high)),
            ..Self::base()
        }
    }

    /// Attach alarm/timestamp metadata to this result.
    pub fn with_alarm(
        mut self,
        alarm_status: u16,
        alarm_severity: u16,
        timestamp: Option<SystemTime>,
    ) -> Self {
        self.alarm_status = alarm_status;
        self.alarm_severity = alarm_severity;
        self.timestamp = timestamp;
        self
    }
}

/// Token for cancelling a queued request before execution.
#[derive(Clone, Debug)]
pub struct CancelToken(pub Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, AtomicOrdering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(AtomicOrdering::Acquire)
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_token() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
    }
}
