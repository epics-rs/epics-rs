pub mod ad_driver;
pub mod ndarray_driver;
pub mod write_tail;

// Re-exports for backward compatibility
pub use crate::color::NDColorMode as ColorMode;
pub use ad_driver::{ADDriver, ADDriverBase};

use asyn_rs::error::{AsynError, AsynResult};
use asyn_rs::port_handle::PortHandle;

/// An acquisition task's read of one param, as C's `getIntegerParam`: an
/// undefined param (no record, or its PINI not yet run) reads 0 and the frame
/// goes on, where a failed read would stop the task with Acquire still 1.
pub fn undefined_as_zero<T: Default>(read: AsynResult<T>) -> AsynResult<T> {
    match read {
        Err(e) if matches!(e.param_failure(), AsynError::ParamUndefined(_)) => Ok(T::default()),
        r => r,
    }
}

/// [`undefined_as_zero`] over a port handle's `getIntegerParam`.
pub async fn snapshot_int32(handle: &PortHandle, reason: usize, addr: i32) -> AsynResult<i32> {
    undefined_as_zero(handle.read_int32(reason, addr).await)
}

/// [`undefined_as_zero`] over a port handle's `getDoubleParam`.
pub async fn snapshot_float64(handle: &PortHandle, reason: usize, addr: i32) -> AsynResult<f64> {
    undefined_as_zero(handle.read_float64(reason, addr).await)
}

/// Detector status states matching ADStatus_t.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ADStatus {
    Idle = 0,
    Acquire = 1,
    Readout = 2,
    Correct = 3,
    Saving = 4,
    Aborting = 5,
    Error = 6,
    Waiting = 7,
    Initializing = 8,
    Disconnected = 9,
    Aborted = 10,
}

/// Image mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ImageMode {
    Single = 0,
    Multiple = 1,
    Continuous = 2,
}

impl ImageMode {
    pub fn from_i32(v: i32) -> Self {
        match v {
            0 => Self::Single,
            1 => Self::Multiple,
            _ => Self::Continuous,
        }
    }
}

/// Shutter mode (C++ has 3 modes: None, EPICS, Detector).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ShutterMode {
    None = 0,
    EpicsOnly = 1,
    DetectorOnly = 2,
}

impl ShutterMode {
    pub fn from_i32(v: i32) -> Option<Self> {
        match v {
            0 => Some(Self::None),
            1 => Some(Self::EpicsOnly),
            2 => Some(Self::DetectorOnly),
            _ => None,
        }
    }
}
