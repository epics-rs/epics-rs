//! Request types for the port actor.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::SystemTime;

use crate::error::AsynStatus;

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
    /// Flush changed parameters as interrupt notifications (callParamCallbacks).
    CallParamCallbacks {
        addr: i32,
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
