pub mod ad_driver;
pub mod ndarray_driver;

// Re-exports for backward compatibility
pub use crate::color::NDColorMode as ColorMode;
pub use ad_driver::{ADDriver, ADDriverBase};

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

/// Shutter mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ShutterMode {
    None = 0,
    EpicsOnly = 1,
    DetectorOnly = 2,
    EpicsAndDetector = 3,
}

impl ShutterMode {
    pub fn from_i32(v: i32) -> Self {
        match v {
            0 => Self::None,
            1 => Self::EpicsOnly,
            2 => Self::DetectorOnly,
            _ => Self::EpicsAndDetector,
        }
    }
}
